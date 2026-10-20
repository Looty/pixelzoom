//! Offline quality harness: renders text natively with DirectWrite at 1x
//! (ClearType) and at kx (grayscale, the "ground truth"), pushes the 1x image
//! through the real zoom shader in several modes, and scores each result
//! against the native kx render (text MSE, RGB MSE, SSIM; plus a strip of soft
//! ramps that sharpening must not harm).
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
const H: u32 = 150;
const FONT_PX: f32 = 13.0;
/// First source row of the soft-ramp strip below the text.
const RAMP_Y: u32 = 138;
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
    unsafe {
        let mut blob: Option<ID3DBlob> = None;
        let mut err: Option<ID3DBlob> = None;
        let entry = format!("{entry}\0");
        let target = format!("{target}\0");
        let r = D3DCompile(
            SHADER.as_ptr() as *const _,
            SHADER.len(),
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
        let fmt = dw.CreateTextFormat(
            w!("Segoe UI"),
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            FONT_PX * scale,
            w!("en-us"),
        )?;
        let black = D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };
        let white = D2D1_COLOR_F { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
        let brush = rt.CreateSolidColorBrush(&black, None)?;
        rt.BeginDraw();
        rt.Clear(Some(&white));
        let s = scale;
        for (i, line) in LINES.iter().enumerate() {
            // Line 3 is coloured text (syntax-highlight style) so colour handling is exercised too.
            brush.SetColor(&if i == 3 { D2D1_COLOR_F { r: 0.0, g: 0.25, b: 0.85, a: 1.0 } } else { black });
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
                let u = px as f32 / s; // source-space position inside the period
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
                rt.FillRectangle(&D2D_RECT_F { left: x, top: ramp_top, right: x + 1.0, bottom: H as f32 * s }, &brush);
            }
        }
        rt.EndDraw(None, None)?;
        Ok(tex)
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
        let (ow, oh) = ((W as f32 * zoom).round() as u32, (H as f32 * zoom).round() as u32);
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
        let data = CB { cursor: [0.0, 0.0], size: [W as f32, H as f32], zoom, mode, sharpen: strength, smode, flags: 0, _pad: [0; 3] };
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

fn luma(bgr: &[u8]) -> Vec<f32> {
    bgr.as_chunks::<3>().0.iter().map(|p| (0.0722 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.2126 * p[2] as f32) / 255.0).collect()
}

/// Per-channel MSE on 0..1 values: unlike luminance scores it sees colour fringes.
fn mse_rgb(a: &[u8], b: &[u8]) -> f64 {
    a.iter().zip(b).map(|(x, y)| ((*x as f64 - *y as f64) / 255.0).powi(2)).sum::<f64>() / a.len() as f64
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

    let src = render_text(&c, 1.0, true)?;
    let (_, _, src_bgr) = read_back(&c, &src)?;
    write_bmp(&out_dir.join("src_1x.bmp"), W, H, &src_bgr);

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

    println!("{:<32} {:>5} {:>10} {:>9} {:>8} {:>9} {:>11}", "variant", "zoom", "text MSE", "RGB MSE", "SSIM", "ramp MSE", "ramp SSIM");
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
            // Rows above the ramp strip are text, rows below are soft ramps.
            let split_rows = (RAMP_Y as f32 * zoom).round() as usize;
            let split = split_rows * w as usize;
            let (wu, hu) = (w as usize, h as usize);
            println!(
                "{:<32} {:>4}x {:>10.5} {:>9.5} {:>8.4} {:>9.5} {:>11.4}",
                name,
                zoom,
                mse(&l[..split], &tl[..split]),
                mse_rgb(&bgr[..split * 3], &truth_bgr[..split * 3]),
                ssim(&l[..split], &tl[..split], wu, split_rows),
                mse(&l[split..], &tl[split..]),
                ssim(&l[split..], &tl[split..], wu, hu - split_rows)
            );
        }
        println!();
    }
    Ok(())
}
