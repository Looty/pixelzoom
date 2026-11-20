//! Offline quality harness: renders text natively with DirectWrite at 1x
//! (ClearType) and at kx (grayscale, the "ground truth"), pushes the 1x image
//! through the real zoom shader in several modes, and scores each result
//! against the native kx render (text MSE, RGB MSE, SSIM, colour fringing; plus
//! light and dark soft ramps and a photo-like colour patch that sharpening must
//! not harm).
//!
//! Run: `cargo run --bin compare [zooms]` (comma separated, default 2.5,3.5,5.5;
//! whole zooms take the shader's exact-block path and ignore the filter). It
//! needs a GPU, so CI only builds it. It also writes `compare_out\*.bmp`.
//!
//! The scores are a sanity check, not a verdict: the ground truth is unhinted
//! text at the larger size, and they have disagreed with how the output looks.
//! Always compare the BMPs and the live app by eye too.

use std::fs;
use std::path::Path;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D::Fxc::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::{Interface, PCSTR, w};

const SHADER: &str = include_str!("../shader.hlsl");
const W: u32 = 380;
const H: u32 = 196;
const FONT_PX: f32 = 13.0;
/// First source row of the soft-ramp strip below the text.
const RAMP_Y: u32 = 138;
/// First source row of the dark soft-ramp strip (shadows, dark UI).
const DARK_Y: u32 = 150;
/// First source row of the photo-like colour patch, which runs to `H`.
const PHOTO_Y: u32 = 162;
const LINES: &[&str] = &[
    "The quick brown fox jumps over the lazy dog",
    "Settings  File  Edit  View  Help   0123456789",
    "fn main() { let x = a[i] + b.len(); } // WAVY",
    "Pixel Zoom: crisp text at 4x? mWiIlL1|!@#%&",
    "Segoe UI renders with ClearType on screen.",
];

#[repr(C)]
struct CB {
    cursor: [f32; 2],
    size: [f32; 2],
    zoom: f32,
    mode: u32,
    sharpen: f32,
    smode: u32,
    flags: u32,
    _pad: [u32; 3],
}

type Res<T> = windows::core::Result<T>;

struct Ctx {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
}

fn compile(entry: &str, target: &str) -> Res<Vec<u8>> {
    // PZ_SHADER=<file> scores another shader version (e.g. `git show main:src/shader.hlsl`)
    // on the same test content, for before/after comparisons.
    // A set but unreadable path is an error: silently scoring the built-in
    // shader would make a before/after run compare the new shader with itself.
    let src = match std::env::var("PZ_SHADER") {
        Ok(p) => fs::read_to_string(&p).unwrap_or_else(|e| panic!("PZ_SHADER={p}: {e}")),
        Err(_) => SHADER.to_string(),
    };
    unsafe {
        let mut blob: Option<ID3DBlob> = None;
        let mut err: Option<ID3DBlob> = None;
        let entry = format!("{entry}\0");
        let target = format!("{target}\0");
        let r = D3DCompile(
            src.as_ptr() as *const _,
            src.len(),
            PCSTR::null(),
            None,
            None,
            PCSTR(entry.as_ptr()),
            PCSTR(target.as_ptr()),
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut blob,
            Some(&mut err),
        );
        if let Err(e) = r {
            if let Some(err) = err {
                let msg = std::slice::from_raw_parts(err.GetBufferPointer() as *const u8, err.GetBufferSize());
                eprintln!("shader error: {}", String::from_utf8_lossy(msg));
            }
            return Err(e);
        }
        let blob = blob.unwrap();
        Ok(std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize()).to_vec())
    }
}

fn make_tex(c: &Ctx, w: u32, h: u32, bind: u32) -> Res<ID3D11Texture2D> {
    unsafe {
        let mut t = None;
        c.device.CreateTexture2D(
            &D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: bind,
                ..Default::default()
            },
            None,
            Some(&mut t),
        )?;
        Ok(t.unwrap())
    }
}

/// Draw the test text at `scale` times the base size. ClearType or grayscale AA.
fn render_text(c: &Ctx, scale: f32, cleartype: bool) -> Res<ID3D11Texture2D> {
    unsafe {
        let (tw, th) = ((W as f32 * scale).round() as u32, (H as f32 * scale).round() as u32);
        let tex = make_tex(c, tw, th, (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32)?;
        let surface: IDXGISurface = tex.cast()?;
        let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_UNKNOWN, alphaMode: D2D1_ALPHA_MODE_IGNORE },
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let rt = d2d.CreateDxgiSurfaceRenderTarget(&surface, &props)?;
        rt.SetTextAntialiasMode(if cleartype { D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE } else { D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE });
        let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        // Held-out checks: PZ_FONT=<family> changes the font, PZ_DARK=1 draws a dark theme.
        let font = std::env::var("PZ_FONT").unwrap_or_else(|_| "Segoe UI".into());
        // An unknown family would silently fall back to the system font.
        let mut fonts = None;
        dw.GetSystemFontCollection(&mut fonts, false)?;
        let (mut idx, mut found) = (0u32, windows::core::BOOL(0));
        fonts.unwrap().FindFamilyName(&windows::core::HSTRING::from(font.as_str()), &mut idx, &mut found)?;
        assert!(found.as_bool(), "PZ_FONT={font}: no such font family");
        let dark = std::env::var("PZ_DARK").is_ok_and(|v| v == "1");
        let fmt = dw.CreateTextFormat(
            &windows::core::HSTRING::from(font),
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            FONT_PX * scale,
            w!("en-us"),
        )?;
        let grey = |v: f32| D2D1_COLOR_F { r: v, g: v, b: v, a: 1.0 };
        let (ink, paper) = if dark { (grey(0.85), grey(0.12)) } else { (grey(0.0), grey(1.0)) };
        let accent =
            if dark { D2D1_COLOR_F { r: 0.34, g: 0.61, b: 0.84, a: 1.0 } } else { D2D1_COLOR_F { r: 0.0, g: 0.25, b: 0.85, a: 1.0 } };
        let brush = rt.CreateSolidColorBrush(&ink, None)?;
        rt.BeginDraw();
        rt.Clear(Some(&paper));
        let s = scale;
        for (i, line) in LINES.iter().enumerate() {
            // Line 3 is coloured text (syntax-highlight style) so colour handling is exercised too.
            brush.SetColor(&if i == 3 { accent } else { ink });
            let text: Vec<u16> = line.encode_utf16().collect();
            let rect = D2D_RECT_F {
                left: 6.0 * s,
                top: (6.0 + i as f32 * 26.0) * s,
                right: (W as f32 - 4.0) * s,
                bottom: (6.0 + (i as f32 + 1.0) * 26.0) * s,
            };
            rt.DrawText(&text, &fmt, &rect, &brush, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
        }
        // Soft ramps below the text (what the snapping must NOT sharpen): 4 source
        // pixels wide, dark<->light, drawn per output pixel so the target is the ideal ramp.
        let ramp_top = RAMP_Y as f32 * s;
        for period in 0..15 {
            for px in 0..((24.0 * scale).round() as u32) {
                let u = (px as f32 + 0.5) / s; // source position of this pixel's centre, as the shader maps it
                let v = if u < 8.0 {
                    0.0
                } else if u < 12.0 {
                    (u - 8.0) / 4.0
                } else if u < 20.0 {
                    1.0
                } else {
                    1.0 - (u - 20.0) / 4.0
                };
                let g = (30.0 + (225.0 - 30.0) * v) / 255.0;
                brush.SetColor(&D2D1_COLOR_F { r: g, g, b: g, a: 1.0 });
                let x = (6.0 + period as f32 * 24.0) * s + px as f32;
                rt.FillRectangle(&D2D_RECT_F { left: x, top: ramp_top, right: x + 1.0, bottom: DARK_Y as f32 * s }, &brush);
            }
        }
        rt.EndDraw(None, None)?;
        fill_patches(c, &tex, tw, th, scale);
        Ok(tex)
    }
}

/// Analytic content below the light ramps, written straight into the texture
/// and sampled at each output pixel's source position so every scale shows the
/// ideal image: a dark ramp strip (4 source px edges between 5 and 60 of 255)
/// and a photo-like patch (smooth colour gradients plus soft-edged discs).
fn fill_patches(c: &Ctx, tex: &ID3D11Texture2D, tw: u32, th: u32, s: f32) {
    let top = (DARK_Y as f32 * s).round() as u32;
    let photo = (PHOTO_Y as f32 * s).round() as u32;
    let mut data = Vec::with_capacity(((th - top) * tw * 4) as usize);
    for y in top..th {
        for x in 0..tw {
            let (u, v) = ((x as f32 + 0.5) / s, (y as f32 + 0.5) / s);
            let rgb = if y < photo {
                // Same shape as the light ramps: 8 dark, 4 up, 8 light, 4 down per 24 px.
                let p = (u - 6.0).rem_euclid(24.0);
                let k = if p < 8.0 {
                    0.0
                } else if p < 12.0 {
                    (p - 8.0) / 4.0
                } else if p < 20.0 {
                    1.0
                } else {
                    1.0 - (p - 20.0) / 4.0
                };
                let g = (5.0 + 55.0 * k) / 255.0;
                [g, g, g]
            } else {
                let pv = v - PHOTO_Y as f32;
                let mut c3 =
                    [0.5 + 0.35 * (u / 9.0).sin(), 0.45 + 0.3 * (u / 14.0 + pv / 6.0).sin(), 0.5 + 0.35 * (u / 11.0 - pv / 8.0).cos()];
                // Soft discs every 40 px (radius 7, ~1.2 px soft edge), cycling colours.
                const DISC: [[f32; 3]; 4] = [[0.85, 0.2, 0.15], [0.1, 0.5, 0.2], [0.95, 0.85, 0.3], [0.25, 0.3, 0.7]];
                let i = ((u - 20.0) / 40.0).round();
                let (dx, dy) = (u - (20.0 + i * 40.0), pv - 17.0);
                let a = (1.0 - ((dx * dx + dy * dy).sqrt() - 6.4) / 1.2).clamp(0.0, 1.0);
                let col = DISC[(i.max(0.0) as usize) % DISC.len()];
                for k in 0..3 {
                    c3[k] += (col[k] - c3[k]) * a;
                }
                c3
            };
            let b = |f: f32| (f.clamp(0.0, 1.0) * 255.0).round() as u8;
            data.extend_from_slice(&[b(rgb[2]), b(rgb[1]), b(rgb[0]), 255]);
        }
    }
    unsafe {
        let bx = D3D11_BOX { left: 0, top, front: 0, right: tw, bottom: th, back: 1 };
        c.ctx.UpdateSubresource(tex, 0, Some(&bx), data.as_ptr() as *const _, tw * 4, 0);
    }
}

/// Rows of the realistic test card (`compare scenes`), 24 source px each.
const SCENES: &[&str] = &[
    "code, dark theme",
    "code, light theme",
    "muted colours, dark",
    "muted colours, light",
    "selection highlights",
    "chart lines",
    "text on colour",
    "grey text",
];
const SROW: u32 = 24;

/// The realistic card at `scale`: syntax-highlighted code (VS Code Dark+ and
/// Light+ colours), muted text colours, selection highlights, 1-2 px chart
/// lines in complementary colours, text on coloured banners and grey text.
/// ClearType at 1x; grayscale anti-aliasing for the large "truth".
fn render_scenes(c: &Ctx, scale: f32, cleartype: bool) -> Res<ID3D11Texture2D> {
    unsafe {
        let s = scale;
        let (tw, th) = ((W as f32 * s).round() as u32, ((SROW * SCENES.len() as u32) as f32 * s).round() as u32);
        let tex = make_tex(c, tw, th, (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32)?;
        let surface: IDXGISurface = tex.cast()?;
        let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_UNKNOWN, alphaMode: D2D1_ALPHA_MODE_IGNORE },
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let rt = d2d.CreateDxgiSurfaceRenderTarget(&surface, &props)?;
        rt.SetTextAntialiasMode(if cleartype { D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE } else { D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE });
        let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let font = |name: &str, px: f32| {
            dw.CreateTextFormat(
                &windows::core::HSTRING::from(name),
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                px * s,
                w!("en-us"),
            )
        };
        let (mono, ui) = (font("Consolas", 14.0)?, font("Segoe UI", 13.0)?);
        let col = |r: u8, g: u8, b: u8| D2D1_COLOR_F { r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a: 1.0 };
        let rect = |l: f32, t: f32, r: f32, b: f32| D2D_RECT_F { left: l * s, top: t * s, right: r * s, bottom: b * s };
        let brush = rt.CreateSolidColorBrush(&col(0, 0, 0), None)?;
        let fill = |r: D2D_RECT_F, k: D2D1_COLOR_F| {
            brush.SetColor(&k);
            rt.FillRectangle(&r, &brush);
        };
        // (text, colour) runs drawn left to right from x on row `row`.
        let runs = |row: u32, x0: f32, fmt: &IDWriteTextFormat, parts: &[(&str, D2D1_COLOR_F)]| -> Res<()> {
            let mut x = x0;
            for (t, k) in parts {
                let text: Vec<u16> = t.encode_utf16().collect();
                let layout = dw.CreateTextLayout(&text, fmt, 4000.0, 100.0)?;
                let mut m = DWRITE_TEXT_METRICS::default();
                layout.GetMetrics(&mut m)?;
                brush.SetColor(k);
                rt.DrawTextLayout(
                    windows_numerics::Vector2 { X: x * s, Y: (row * SROW) as f32 * s + 4.0 * s },
                    &layout,
                    &brush,
                    D2D1_DRAW_TEXT_OPTIONS_NONE,
                );
                x += m.widthIncludingTrailingWhitespace / s;
            }
            Ok(())
        };
        let y = |row: u32| (row * SROW) as f32;
        rt.BeginDraw();
        rt.Clear(Some(&col(255, 255, 255)));
        // 0: Dark+ code.
        fill(rect(0.0, y(0), W as f32, y(1)), col(30, 30, 30));
        let (kw, var, txt, st, ctl, num, func) = (
            col(86, 156, 214),
            col(156, 220, 254),
            col(212, 212, 212),
            col(206, 145, 120),
            col(197, 134, 192),
            col(181, 206, 168),
            col(220, 220, 170),
        );
        runs(
            0,
            4.0,
            &mono,
            &[
                ("const ", kw),
                ("url", var),
                (" = ", txt),
                ("\"x.io\"", st),
                ("; ", txt),
                ("if", ctl),
                (" (n > ", txt),
                ("42", num),
                (") ", txt),
                ("foo", func),
                ("(n);", txt),
            ],
        )?;
        // 1: Light+ code.
        let (kw, var, st, ctl, num, func) =
            (col(0, 0, 255), col(0, 16, 128), col(163, 21, 21), col(175, 0, 219), col(9, 134, 88), col(121, 94, 38));
        let blk = col(0, 0, 0);
        runs(
            1,
            4.0,
            &mono,
            &[
                ("const ", kw),
                ("url", var),
                (" = ", blk),
                ("\"x.io\"", st),
                ("; ", blk),
                ("if", ctl),
                (" (n > ", blk),
                ("42", num),
                (") ", blk),
                ("foo", func),
                ("(n);", blk),
            ],
        )?;
        // 2: muted colours on dark (comment green, number green, grey).
        fill(rect(0.0, y(2), W as f32, y(3)), col(30, 30, 30));
        runs(
            2,
            4.0,
            &ui,
            &[
                ("// comment text ", col(106, 153, 85)),
                ("1234 mint ", col(181, 206, 168)),
                ("slate words ", col(140, 150, 165)),
                ("dim grey", col(128, 128, 128)),
            ],
        )?;
        // 3: muted colours on light.
        runs(
            3,
            4.0,
            &ui,
            &[
                ("slate text ", col(102, 115, 128)),
                ("warm grey ", col(128, 102, 89)),
                ("brown func ", col(121, 94, 38)),
                ("mid grey", col(128, 128, 128)),
            ],
        )?;
        // 4: selection highlights, light and dark.
        fill(rect(4.0, y(4) + 3.0, 150.0, y(5) - 3.0), col(204, 232, 255));
        runs(4, 6.0, &ui, &[("Selected text here", col(0, 0, 0))])?;
        fill(rect(190.0, y(4), W as f32, y(5)), col(30, 30, 30));
        fill(rect(196.0, y(4) + 3.0, 340.0, y(5) - 3.0), col(38, 79, 120));
        runs(4, 198.0, &ui, &[("Selected dark text", col(212, 212, 212))])?;
        // 5: chart lines: complementary pairs side by side and crossing.
        let line = |x0: f32, y0: f32, x1: f32, y1: f32, k: D2D1_COLOR_F, wd: f32| {
            brush.SetColor(&k);
            rt.DrawLine(
                windows_numerics::Vector2 { X: x0 * s, Y: y0 * s },
                windows_numerics::Vector2 { X: x1 * s, Y: y1 * s },
                &brush,
                wd * s,
                None,
            );
        };
        let (orange, blue, red, green) = (col(255, 127, 14), col(31, 119, 180), col(214, 39, 40), col(44, 160, 44));
        // Crisp 1 px horizontal lines in adjacent rows (centred on pixel rows).
        line(4.0, y(5) + 8.5, 190.0, y(5) + 8.5, orange, 1.0);
        line(4.0, y(5) + 9.5, 190.0, y(5) + 9.5, blue, 1.0);
        // Crisp 1 px vertical lines side by side, orange|blue and red, gap, cyan:
        // indistinguishable from a fringed black stroke for a local test.
        fill(rect(192.0, y(5) + 2.0, 193.0, y(6) - 2.0), orange);
        fill(rect(193.0, y(5) + 2.0, 194.0, y(6) - 2.0), blue);
        fill(rect(196.0, y(5) + 2.0, 197.0, y(6) - 2.0), col(232, 17, 35));
        fill(rect(198.0, y(5) + 2.0, 199.0, y(6) - 2.0), col(0, 183, 195));
        line(4.0, y(5) + 20.0, 190.0, y(5) + 4.0, red, 1.5);
        line(4.0, y(5) + 4.0, 190.0, y(5) + 20.0, green, 1.5);
        line(200.0, y(5) + 6.0, 376.0, y(5) + 18.0, orange, 1.5);
        line(200.0, y(5) + 8.0, 376.0, y(5) + 20.0, blue, 1.5);
        // 6: text on coloured banners.
        fill(rect(0.0, y(6), 180.0, y(7)), col(255, 240, 200));
        runs(6, 4.0, &ui, &[("Warning: banner text", col(0, 0, 0))])?;
        fill(rect(190.0, y(6) + 2.0, 330.0, y(7) - 2.0), col(0, 120, 215));
        runs(6, 196.0, &ui, &[("Primary button", col(255, 255, 255))])?;
        // 7: grey text (disabled / secondary) on white and on dark.
        runs(7, 4.0, &ui, &[("Secondary grey label text", col(110, 110, 110))])?;
        fill(rect(190.0, y(7), W as f32, y(8)), col(30, 30, 30));
        runs(7, 196.0, &ui, &[("Dim text on dark", col(150, 150, 150))])?;
        rt.EndDraw(None, None)?;
        Ok(tex)
    }
}

/// Mean colour the output lost against the truth (chroma, clamped at 0): real
/// colour turned grey. The counterpart of `fringe`, which counts colour added.
fn colour_lost(out: &[u8], truth: &[u8]) -> f64 {
    fringe(truth, out)
}

/// `compare scenes [zooms]`: score the realistic card per row.
fn scenes_card(c: &Ctx, zooms: &[f32], variants: &[(&str, u32, u32)]) -> Res<()> {
    let out_dir = Path::new("compare_out");
    let src = render_scenes(c, 1.0, true)?;
    let (sw, sh, sb) = read_back(c, &src)?;
    write_bmp(&out_dir.join("scenes_src_1x.bmp"), sw, sh, &sb);
    for &zoom in zooms {
        let (tw, th, tb) = read_back(c, &render_scenes(c, zoom, false)?)?;
        write_bmp(&out_dir.join(format!("scenes_truth_{zoom}x.bmp")), tw, th, &tb);
        println!("{zoom}x: per row, RGB MSE / colour lost (real colour greyed) / fringe (colour added)");
        print!("{:<30}", "");
        for s in SCENES {
            print!(" | {:>22}", s);
        }
        println!();
        for (name, mode, smode) in variants {
            let (w, _, ob) = read_back(c, &zoom_pass(c, &src, zoom, *mode, *smode, 0.8)?)?;
            let tag: String = name.chars().filter(|ch| ch.is_alphanumeric()).collect();
            write_bmp(&out_dir.join(format!("scenes_{tag}_{zoom}x.bmp")), w, th, &ob);
            print!("{name:<30}");
            for i in 0..SCENES.len() as u32 {
                let (a, b) = (((i * SROW) as f32 * zoom).round() as usize, (((i + 1) * SROW) as f32 * zoom).round() as usize);
                let r = a * w as usize * 3..b.min(th as usize) * w as usize * 3;
                print!(
                    " | {:.4} {:.4} {:.4}",
                    mse_rgb(&ob[r.clone()], &tb[r.clone()]),
                    colour_lost(&ob[r.clone()], &tb[r.clone()]),
                    fringe(&ob[r.clone()], &tb[r])
                );
            }
            println!();
        }
        println!();
    }
    Ok(())
}

/// The shader's flag bits from PZ_FLAGS (1: BGR panel, 2: text clean-up off,
/// 4: HDR input). Set but unparsable is an error, not a silent 0.
fn pz_flags() -> u32 {
    match std::env::var("PZ_FLAGS") {
        Ok(v) => v.trim().parse().unwrap_or_else(|_| panic!("PZ_FLAGS={v}: not a decimal number")),
        Err(_) => 0,
    }
}

/// Read a BGRA texture back as (w, h, rgb bytes in B,G,R order per pixel).
fn read_back(c: &Ctx, tex: &ID3D11Texture2D) -> Res<(u32, u32, Vec<u8>)> {
    unsafe {
        let mut d = D3D11_TEXTURE2D_DESC::default();
        tex.GetDesc(&mut d);
        d.Usage = D3D11_USAGE_STAGING;
        d.BindFlags = 0;
        d.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        d.MiscFlags = 0;
        let mut st = None;
        c.device.CreateTexture2D(&d, None, Some(&mut st))?;
        let st: ID3D11Texture2D = st.unwrap();
        c.ctx.CopyResource(&st, tex);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        c.ctx.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
        let mut out = Vec::with_capacity((d.Width * d.Height * 3) as usize);
        for y in 0..d.Height {
            let row = std::slice::from_raw_parts((m.pData as *const u8).add((y * m.RowPitch) as usize), (d.Width * 4) as usize);
            for px in row.as_chunks::<4>().0 {
                out.extend_from_slice(&px[0..3]);
            }
        }
        c.ctx.Unmap(&st, 0);
        Ok((d.Width, d.Height, out))
    }
}

/// Run the real shader: source `src` zoomed by `zoom`, anchored at the top-left.
fn zoom_pass(c: &Ctx, src: &ID3D11Texture2D, zoom: f32, mode: u32, smode: u32, strength: f32) -> Res<ID3D11Texture2D> {
    unsafe {
        let mut sd = D3D11_TEXTURE2D_DESC::default();
        src.GetDesc(&mut sd);
        let (sw, sh) = (sd.Width, sd.Height);
        let (ow, oh) = ((sw as f32 * zoom).round() as u32, (sh as f32 * zoom).round() as u32);
        let out = make_tex(c, ow, oh, D3D11_BIND_RENDER_TARGET.0 as u32)?;
        let mut rtv = None;
        c.device.CreateRenderTargetView(&out, None, Some(&mut rtv))?;
        let mut srv = None;
        c.device.CreateShaderResourceView(src, None, Some(&mut srv))?;
        let mut vs = None;
        c.device.CreateVertexShader(&compile("vs_main", "vs_5_0")?, None, Some(&mut vs))?;
        let mut ps = None;
        c.device.CreatePixelShader(&compile("ps_main", "ps_5_0")?, None, Some(&mut ps))?;
        let mut cb = None;
        c.device.CreateBuffer(
            &D3D11_BUFFER_DESC {
                ByteWidth: size_of::<CB>() as u32,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                ..Default::default()
            },
            None,
            Some(&mut cb),
        )?;
        let mut sampler = None;
        c.device.CreateSamplerState(
            &D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                ..Default::default()
            },
            Some(&mut sampler),
        )?;
        let (cb, srv, vs, ps, sampler) = (cb.unwrap(), srv.unwrap(), vs.unwrap(), ps.unwrap(), sampler.unwrap());
        let data =
            CB { cursor: [0.0, 0.0], size: [sw as f32, sh as f32], zoom, mode, sharpen: strength, smode, flags: pz_flags(), _pad: [0; 3] };
        c.ctx.UpdateSubresource(&cb, 0, None, &data as *const _ as *const _, 0, 0);
        c.ctx.OMSetRenderTargets(Some(&[rtv]), None);
        c.ctx.RSSetViewports(Some(&[D3D11_VIEWPORT { Width: ow as f32, Height: oh as f32, MaxDepth: 1.0, ..Default::default() }]));
        c.ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        c.ctx.VSSetShader(&vs, None);
        c.ctx.PSSetShader(&ps, None);
        c.ctx.PSSetConstantBuffers(0, Some(&[Some(cb)]));
        c.ctx.PSSetShaderResources(0, Some(&[Some(srv)]));
        c.ctx.PSSetSamplers(0, Some(&[Some(sampler)]));
        c.ctx.Draw(3, 0);
        c.ctx.OMSetRenderTargets(None, None);
        Ok(out)
    }
}

/// Time per frame (ms) of the shipped mode (filter 3, sharpen 4) at `zoom`:
/// `n` back-to-back draws of `src` zoomed, synchronised by reading back a pixel.
fn bench(c: &Ctx, src: &ID3D11Texture2D, zoom: f32, n: u32) -> Res<f64> {
    unsafe {
        let mut sd = D3D11_TEXTURE2D_DESC::default();
        src.GetDesc(&mut sd);
        let (sw, sh) = (sd.Width, sd.Height);
        let (ow, oh) = ((sw as f32 * zoom).round() as u32, (sh as f32 * zoom).round() as u32);
        let out = make_tex(c, ow, oh, D3D11_BIND_RENDER_TARGET.0 as u32)?;
        let (mut rtv, mut srv, mut vs, mut ps, mut cb) = (None, None, None, None, None);
        c.device.CreateRenderTargetView(&out, None, Some(&mut rtv))?;
        c.device.CreateShaderResourceView(src, None, Some(&mut srv))?;
        c.device.CreateVertexShader(&compile("vs_main", "vs_5_0")?, None, Some(&mut vs))?;
        c.device.CreatePixelShader(&compile("ps_main", "ps_5_0")?, None, Some(&mut ps))?;
        let data =
            CB { cursor: [0.0, 0.0], size: [sw as f32, sh as f32], zoom, mode: 3, sharpen: 0.8, smode: 4, flags: pz_flags(), _pad: [0; 3] };
        c.device.CreateBuffer(
            &D3D11_BUFFER_DESC {
                ByteWidth: size_of::<CB>() as u32,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                ..Default::default()
            },
            Some(&D3D11_SUBRESOURCE_DATA { pSysMem: &data as *const _ as *const _, ..Default::default() }),
            Some(&mut cb),
        )?;
        c.ctx.OMSetRenderTargets(Some(&[rtv]), None);
        c.ctx.RSSetViewports(Some(&[D3D11_VIEWPORT { Width: ow as f32, Height: oh as f32, MaxDepth: 1.0, ..Default::default() }]));
        c.ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        c.ctx.VSSetShader(vs.as_ref(), None);
        c.ctx.PSSetShader(ps.as_ref(), None);
        c.ctx.PSSetConstantBuffers(0, Some(&[cb]));
        c.ctx.PSSetShaderResources(0, Some(&[srv]));
        let mut st = None;
        c.device.CreateTexture2D(
            &D3D11_TEXTURE2D_DESC {
                Width: 1,
                Height: 1,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                ..Default::default()
            },
            None,
            Some(&mut st),
        )?;
        let st: ID3D11Texture2D = st.unwrap();
        let sync = || -> Res<()> {
            let bx = D3D11_BOX { left: ow / 2, top: oh / 2, front: 0, right: ow / 2 + 1, bottom: oh / 2 + 1, back: 1 };
            c.ctx.CopySubresourceRegion(&st, 0, 0, 0, 0, &out, 0, Some(&bx));
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            c.ctx.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
            c.ctx.Unmap(&st, 0);
            Ok(())
        };
        c.ctx.Draw(3, 0); // warm-up
        sync()?;
        let t = std::time::Instant::now();
        for _ in 0..n {
            c.ctx.Draw(3, 0);
        }
        sync()?;
        let ms = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
        c.ctx.OMSetRenderTargets(None, None);
        Ok(ms)
    }
}

fn luma(bgr: &[u8]) -> Vec<f32> {
    bgr.as_chunks::<3>().0.iter().map(|p| (0.0722 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.2126 * p[2] as f32) / 255.0).collect()
}

/// Per-channel MSE on 0..1 values: unlike luminance scores it sees colour fringes.
fn mse_rgb(a: &[u8], b: &[u8]) -> f64 {
    a.iter().zip(b).map(|(x, y)| ((*x as f64 - *y as f64) / 255.0).powi(2)).sum::<f64>() / a.len() as f64
}

/// Mean colour (max - min channel, 0..1) the output adds over the truth: the
/// orange/blue ClearType fringing on text that should be grey.
fn fringe(out: &[u8], truth: &[u8]) -> f64 {
    let ch = |p: &[u8; 3]| (*p.iter().max().unwrap() as f64 - *p.iter().min().unwrap() as f64) / 255.0;
    let (o, t) = (out.as_chunks::<3>().0, truth.as_chunks::<3>().0);
    o.iter().zip(t).map(|(a, b)| (ch(a) - ch(b)).max(0.0)).sum::<f64>() / o.len() as f64
}

fn mse(a: &[f32], b: &[f32]) -> f64 {
    a.iter().zip(b).map(|(x, y)| ((x - y) as f64).powi(2)).sum::<f64>() / a.len() as f64
}

/// Mean SSIM over 8x8 windows, stride 4.
fn ssim(a: &[f32], b: &[f32], w: usize, h: usize) -> f64 {
    let (c1, c2) = (0.01f64.powi(2), 0.03f64.powi(2));
    let (mut sum, mut n) = (0.0, 0u32);
    let mut y = 0;
    while y + 8 <= h {
        let mut x = 0;
        while x + 8 <= w {
            let (mut ma, mut mb) = (0.0f64, 0.0f64);
            for j in 0..8 {
                for i in 0..8 {
                    ma += a[(y + j) * w + x + i] as f64;
                    mb += b[(y + j) * w + x + i] as f64;
                }
            }
            ma /= 64.0;
            mb /= 64.0;
            let (mut va, mut vb, mut cov) = (0.0f64, 0.0f64, 0.0f64);
            for j in 0..8 {
                for i in 0..8 {
                    let (da, db) = (a[(y + j) * w + x + i] as f64 - ma, b[(y + j) * w + x + i] as f64 - mb);
                    va += da * da;
                    vb += db * db;
                    cov += da * db;
                }
            }
            va /= 63.0;
            vb /= 63.0;
            cov /= 63.0;
            sum += ((2.0 * ma * mb + c1) * (2.0 * cov + c2)) / ((ma * ma + mb * mb + c1) * (va + vb + c2));
            n += 1;
            x += 4;
        }
        y += 4;
    }
    sum / n as f64
}

fn write_bmp(path: &Path, w: u32, h: u32, bgr: &[u8]) {
    let row = (w * 3).div_ceil(4) * 4;
    let size = 54 + row * h;
    let mut f = Vec::with_capacity(size as usize);
    f.extend_from_slice(b"BM");
    f.extend_from_slice(&size.to_le_bytes());
    f.extend_from_slice(&[0; 4]);
    f.extend_from_slice(&54u32.to_le_bytes());
    f.extend_from_slice(&40u32.to_le_bytes());
    f.extend_from_slice(&w.to_le_bytes());
    f.extend_from_slice(&h.to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&24u16.to_le_bytes());
    f.extend_from_slice(&[0; 24]);
    for y in (0..h).rev() {
        f.extend_from_slice(&bgr[(y * w * 3) as usize..((y + 1) * w * 3) as usize]);
        f.extend(std::iter::repeat_n(0u8, (row - w * 3) as usize));
    }
    let _ = fs::write(path, f);
}

fn main() -> Res<()> {
    let (mut device, mut ctx) = (None, None);
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut ctx),
        )?;
    }
    let c = Ctx { device: device.unwrap(), ctx: ctx.unwrap() };
    let out_dir = Path::new("compare_out");
    let _ = fs::create_dir_all(out_dir);

    println!(
        "shader: {}; flags: {}; font: {}; theme: {}",
        std::env::var("PZ_SHADER").unwrap_or_else(|_| "built in".into()),
        pz_flags(),
        std::env::var("PZ_FONT").unwrap_or_else(|_| "Segoe UI".into()),
        if std::env::var("PZ_DARK").is_ok_and(|v| v == "1") { "dark" } else { "light" }
    );
    let src = render_text(&c, 1.0, true)?;
    let (_, _, src_bgr) = read_back(&c, &src)?;
    write_bmp(&out_dir.join("src_1x.bmp"), W, H, &src_bgr);

    if std::env::args().nth(1).as_deref() == Some("scenes") {
        let zooms: Vec<f32> =
            std::env::args().nth(2).map_or(vec![2.0, 2.5, 3.0, 3.5, 4.0], |a| a.split(',').filter_map(|z| z.parse().ok()).collect());
        return scenes_card(&c, &zooms, &[("lanczos3", 3, 0), ("lanczos3+ct+snap(4) [current]", 3, 4)]);
    }

    // `compare bench`: frame cost of the shipped mode at a true 3840x2160 output,
    // from a source tiled with the test card (text, ramps, photo patch).
    if std::env::args().nth(1).as_deref() == Some("bench") {
        for zoom in [2.0f32, 2.5, 4.0, 10.0] {
            let (sw, sh) = ((3840.0 / zoom) as u32, (2160.0 / zoom) as u32);
            let tiled = make_tex(&c, sw, sh, D3D11_BIND_SHADER_RESOURCE.0 as u32)?;
            for ty in (0..sh).step_by(H as usize) {
                for tx in (0..sw).step_by(W as usize) {
                    let bx = D3D11_BOX { left: 0, top: 0, front: 0, right: W.min(sw - tx), bottom: H.min(sh - ty), back: 1 };
                    unsafe { c.ctx.CopySubresourceRegion(&tiled, 0, tx, ty, 0, &src, 0, Some(&bx)) };
                }
            }
            let runs: Vec<f64> = (0..5).map(|_| bench(&c, &tiled, zoom, 200)).collect::<Res<_>>()?;
            let best = runs.iter().cloned().fold(f64::MAX, f64::min);
            println!("{zoom}x: {best:.3} ms per 3840x2160 frame (best of 5 x 200)");
        }
        return Ok(());
    }

    // (label, filter mode, sharpen mode)
    let variants: &[(&str, u32, u32)] = &[
        ("nearest", 0, 0),
        ("bilinear", 1, 0),
        ("lanczos3", 3, 0),
        ("lanczos3+snap(2)", 3, 2),
        ("lanczos3+ct+snap(4) [current]", 3, 4),
        ("sharp-bilinear", 4, 0),
        ("sharp-bilinear+ct+snap(4)", 4, 4),
    ];

    println!(
        "{:<30} {:>5} {:>8} {:>8} {:>7} {:>7} | {:>8} {:>6} | {:>8} {:>6} | {:>9} {:>6}",
        "variant", "zoom", "text MSE", "RGB MSE", "SSIM", "fringe", "ramp MSE", "SSIM", "dark MSE", "SSIM", "photo RGB", "SSIM"
    );
    // Fractional zooms: whole ratios take the shader's exact-block path and ignore the filter.
    let zooms: Vec<f32> = std::env::args().nth(1).map_or(vec![2.5, 3.5, 5.5], |a| a.split(',').filter_map(|z| z.parse().ok()).collect());
    for zoom in zooms {
        let truth = render_text(&c, zoom, false)?;
        let (tw, th, truth_bgr) = read_back(&c, &truth)?;
        write_bmp(&out_dir.join(format!("truth_{zoom}x.bmp")), tw, th, &truth_bgr);
        let tl = luma(&truth_bgr);
        for (name, mode, smode) in variants {
            let out = zoom_pass(&c, &src, zoom, *mode, *smode, 0.8)?;
            let (w, h, bgr) = read_back(&c, &out)?;
            let l = luma(&bgr);
            let tag: String = name.chars().filter(|ch| ch.is_alphanumeric()).collect();
            write_bmp(&out_dir.join(format!("{tag}_{zoom}x.bmp")), w, h, &bgr);
            // Bands of output rows: text, light ramps, dark ramps, photo patch. Each
            // skips 2 source rows next to a strip border, so the hard edge between
            // strips isn't scored as part of a soft ramp.
            let (wu, hu) = (w as usize, h as usize);
            let row = |y: u32| ((y as f32 * zoom).round() as usize).min(hu);
            let bands =
                [(0, row(RAMP_Y - 2)), (row(RAMP_Y + 2), row(DARK_Y - 2)), (row(DARK_Y + 2), row(PHOTO_Y - 2)), (row(PHOTO_Y + 2), hu)];
            let lum = |(a, b): (usize, usize)| (&l[a * wu..b * wu], &tl[a * wu..b * wu], b - a);
            let rgb = |(a, b): (usize, usize)| (&bgr[a * wu * 3..b * wu * 3], &truth_bgr[a * wu * 3..b * wu * 3]);
            let (tx, tt, tr) = lum(bands[0]);
            let (rx, rt, rr) = lum(bands[1]);
            let (dx, dt, dr) = lum(bands[2]);
            let (px, pt, pr) = lum(bands[3]);
            println!(
                "{:<30} {:>4}x {:>8.5} {:>8.5} {:>7.4} {:>7.4} | {:>8.5} {:>6.4} | {:>8.5} {:>6.4} | {:>9.5} {:>6.4}",
                name,
                zoom,
                mse(tx, tt),
                mse_rgb(rgb(bands[0]).0, rgb(bands[0]).1),
                ssim(tx, tt, wu, tr),
                fringe(rgb(bands[0]).0, rgb(bands[0]).1),
                mse(rx, rt),
                ssim(rx, rt, wu, rr),
                mse(dx, dt),
                ssim(dx, dt, wu, dr),
                mse_rgb(rgb(bands[3]).0, rgb(bands[3]).1),
                ssim(px, pt, wu, pr)
            );
        }
        println!();
    }
    Ok(())
}
