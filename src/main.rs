//! pixelzoom — tray app that zooms one monitor (default: primary) with a key + scroll.
//!
//! Settings live in the tray menu and %APPDATA%\pixelzoom\config.ini.
//! Emergency hotkeys: Ctrl+Alt+Q quits. `--selftest` zooms 2x briefly and
//! prints timings; `--debug` adds capture logging. Run from a terminal to see output.

#![windows_subsystem = "windows"]

mod icon;
mod monitors;
mod settings;
mod tray;
mod ui;
mod zoom;

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct3D::Fxc::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentThreadId};
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Interface, PCSTR, w};

use settings::*;
use zoom::step_zoom;

const SHADER: &str = include_str!("shader.hlsl");
const EASE_RATE: f32 = 32.0; // higher = snappier zoom animation

static TARGET: AtomicU32 = AtomicU32::new(1.0f32.to_bits());
static MAIN_THREAD: AtomicU32 = AtomicU32::new(0);
/// Desktop rectangle (left, top, right, bottom) of the zoomed monitor, for the wheel hook.
static SEL_RECT: [AtomicI32; 4] = [AtomicI32::new(0), AtomicI32::new(0), AtomicI32::new(i32::MAX), AtomicI32::new(i32::MAX)];
/// Set when the display setup changed (resolution, HDR, monitors, resume): rebuild GPU state.
pub(crate) static DISPLAY_DIRTY: AtomicBool = AtomicBool::new(false);

fn point_in_sel_rect(x: i32, y: i32) -> bool {
    x >= SEL_RECT[0].load(Ordering::Relaxed)
        && y >= SEL_RECT[1].load(Ordering::Relaxed)
        && x < SEL_RECT[2].load(Ordering::Relaxed)
        && y < SEL_RECT[3].load(Ordering::Relaxed)
}

fn debug_log() -> bool {
    std::env::args().any(|a| a == "--debug")
}

pub(crate) fn target() -> f32 {
    f32::from_bits(TARGET.load(Ordering::Relaxed))
}
pub(crate) fn set_target(z: f32) {
    TARGET.store(z.to_bits(), Ordering::Relaxed);
}

/// True when every key of the configured zoom shortcut is held.
fn combo_down() -> bool {
    let down = |vk: i32| unsafe { GetAsyncKeyState(vk) < 0 };
    // Wheel pseudo-keys only pick the zoom direction; they are never "held".
    let keys: Vec<u32> = combo().into_iter().filter(|k| !is_wheel(*k)).collect();
    !keys.is_empty() && keys.iter().all(|&k| if k == K_WIN { down(0x5B) || down(0x5C) } else { down(k as i32) })
}

#[repr(C)]
struct CB {
    cursor: [f32; 2],
    size: [f32; 2],
    zoom: f32,
    mode: u32,
    sharpen: f32,
    smode: u32,
    /// Bit 0: BGR sub-pixel order. Padded to a whole 16-byte constant-buffer row.
    flags: u32,
    _pad: [u32; 3],
}

/// GPU objects that live as long as the process.
struct Gfx {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    swap: IDXGISwapChain1,
    rtv: ID3D11RenderTargetView,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    cb: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    output: IDXGIOutput1,
    hdr: bool,
    w: u32,
    h: u32,
    /// Top-left of the zoomed monitor on the virtual desktop (physical px).
    origin: (i32, i32),
    /// `MONITOR_GEN` this was built for; a change means the selection moved.
    generation: u32,
    /// Declared last so the window outlives the swapchain it presents to.
    overlay: Overlay,
}

/// The click-through overlay window; destroyed when dropped.
struct Overlay {
    hwnd: HWND,
}

impl Drop for Overlay {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Exists only while zoomed in; dropping it releases the capture.
struct Capture {
    dupl: IDXGIOutputDuplication,
    desktop: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
}

thread_local! {
    static CAPTURE: RefCell<Option<Capture>> = const { RefCell::new(None) };
}

unsafe extern "system" fn wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(h, m, w, l) }
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if code == HC_ACTION as i32 && wparam.0 as u32 == WM_MOUSEWHEEL {
            let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
            let mut notches = (info.mouseData >> 16) as i16 as f32 / 120.0;
            // Settings window is recording a shortcut: it takes the wheel event.
            if ui::recording_wheel(notches) {
                return LRESULT(1);
            }
            // Only react on the zoomed monitor; elsewhere scrolling passes through.
            if ENABLED.load(Ordering::Relaxed) && point_in_sel_rect(info.pt.x, info.pt.y) && combo_down() {
                if wheel_inverted() {
                    notches = -notches; // shortcut says scroll down zooms in
                }
                set_target(step_zoom(target(), notches));
                let _ = PostThreadMessageW(MAIN_THREAD.load(Ordering::Relaxed), WM_APP, WPARAM(0), LPARAM(0));
                return LRESULT(1);
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }
}

/// Compiled shaders are kept so rebuilding the GPU state (monitor switch,
/// display change) doesn't pay the compile cost again.
static SHADER_CACHE: std::sync::Mutex<Vec<(String, Vec<u8>)>> = std::sync::Mutex::new(Vec::new());

fn compile(entry: &str, target: &str) -> windows::core::Result<Vec<u8>> {
    if let Some((_, bytes)) = SHADER_CACHE.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(k, _)| k == entry) {
        return Ok(bytes.clone());
    }
    let bytes = compile_uncached(entry, target)?;
    SHADER_CACHE.lock().unwrap_or_else(|e| e.into_inner()).push((entry.to_string(), bytes.clone()));
    Ok(bytes)
}

fn compile_uncached(entry: &str, target: &str) -> windows::core::Result<Vec<u8>> {
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

fn create_overlay(x: i32, y: i32, w: i32, h: i32) -> windows::core::Result<HWND> {
    unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None)?.into();
        let class = w!("pixelzoom_overlay");
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
            class,
            w!("pixelzoom"),
            WS_POPUP,
            x,
            y,
            w,
            h,
            None,
            None,
            Some(hinst),
            None,
        )?;
        let setup = || -> windows::core::Result<()> {
            SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA)?;
            // Keep our own pixels out of the desktop capture (no feedback loop).
            SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)
        };
        if let Err(e) = setup() {
            let _ = DestroyWindow(hwnd);
            return Err(e);
        }
        Ok(hwnd)
    }
}

/// Find the DXGI output whose device name is `id`; fall back to the primary monitor
/// (the one at the desktop origin), then to the first attached output.
fn find_output(factory: &IDXGIFactory1, id: &str) -> windows::core::Result<(IDXGIAdapter1, IDXGIOutput)> {
    unsafe {
        let mut primary = None;
        let mut first = None;
        let mut ai = 0;
        while let Ok(adapter) = factory.EnumAdapters1(ai) {
            ai += 1;
            let mut oi = 0;
            while let Ok(out) = adapter.EnumOutputs(oi) {
                oi += 1;
                let Ok(d) = out.GetDesc() else { continue };
                if !d.AttachedToDesktop.as_bool() {
                    continue;
                }
                if !id.is_empty() && monitors::utf16_to_string(&d.DeviceName) == id {
                    return Ok((adapter, out));
                }
                if primary.is_none() && d.DesktopCoordinates.left == 0 && d.DesktopCoordinates.top == 0 {
                    primary = Some((adapter.clone(), out.clone()));
                }
                if first.is_none() {
                    first = Some((adapter.clone(), out.clone()));
                }
            }
        }
        primary.or(first).ok_or_else(|| DXGI_ERROR_NOT_FOUND.into())
    }
}

fn init_gfx(monitor_id: &str, generation: u32) -> windows::core::Result<Gfx> {
    unsafe {
        let factory1: IDXGIFactory1 = CreateDXGIFactory1()?;
        let (adapter, out0) = find_output(&factory1, monitor_id)?;
        let rect = out0.GetDesc()?.DesktopCoordinates;
        let (w, h) = ((rect.right - rect.left) as u32, (rect.bottom - rect.top) as u32);
        let hwnd = create_overlay(rect.left, rect.top, w as i32, h as i32)?;
        let overlay = Overlay { hwnd }; // from here on, errors tear the window down via Drop

        for (slot, v) in SEL_RECT.iter().zip([rect.left, rect.top, rect.right, rect.bottom]) {
            slot.store(v, Ordering::Relaxed);
        }
        let mut device = None;
        let mut ctx = None;
        let base: IDXGIAdapter = adapter.cast()?;
        D3D11CreateDevice(
            Some(&base),
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut ctx),
        )?;
        let device: ID3D11Device = device.unwrap();
        let ctx: ID3D11DeviceContext = ctx.unwrap();

        let dxgi: IDXGIDevice = device.cast()?;
        let hdr = out0
            .cast::<IDXGIOutput6>()
            .and_then(|o| o.GetDesc1())
            .map(|d| d.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020)
            .unwrap_or(false);
        println!("zoom monitor {w}x{h} at ({}, {}), HDR: {hdr}", rect.left, rect.top);
        // Keep at most one frame queued: less lag between cursor and picture.
        dxgi.cast::<IDXGIDevice1>()?.SetMaximumFrameLatency(1)?;
        let output: IDXGIOutput1 = out0.cast()?;
        let factory: IDXGIFactory2 = adapter.GetParent()?;

        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: w,
            Height: h,
            Format: if hdr { DXGI_FORMAT_R16G16B16A16_FLOAT } else { DXGI_FORMAT_B8G8R8A8_UNORM },
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            ..Default::default()
        };
        let swap = factory.CreateSwapChainForHwnd(&device, hwnd, &desc, None, None)?;
        if hdr {
            // scRGB: linear, Rec.709 primaries, 1.0 = 80 nits — same as the desktop's own surface.
            swap.cast::<IDXGISwapChain3>()?.SetColorSpace1(DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709)?;
        }
        let back: ID3D11Texture2D = swap.GetBuffer(0)?;
        let mut rtv = None;
        device.CreateRenderTargetView(&back, None, Some(&mut rtv))?;

        let mut vs = None;
        device.CreateVertexShader(&compile("vs_main", "vs_5_0")?, None, Some(&mut vs))?;
        let mut ps = None;
        device.CreatePixelShader(&compile("ps_main", "ps_5_0")?, None, Some(&mut ps))?;

        let mut cb = None;
        device.CreateBuffer(
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
        device.CreateSamplerState(
            &D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                ..Default::default()
            },
            Some(&mut sampler),
        )?;

        Ok(Gfx {
            device,
            ctx,
            swap,
            rtv: rtv.unwrap(),
            vs: vs.unwrap(),
            ps: ps.unwrap(),
            cb: cb.unwrap(),
            sampler: sampler.unwrap(),
            output,
            hdr,
            w,
            h,
            origin: (rect.left, rect.top),
            generation,
            overlay,
        })
    }
}

/// Start desktop duplication and wait for the first frame.
fn start_capture(g: &Gfx) -> windows::core::Result<Capture> {
    unsafe {
        let dupl = if g.hdr {
            g.output.cast::<IDXGIOutput5>()?.DuplicateOutput1(&g.device, 0, &[DXGI_FORMAT_R16G16B16A16_FLOAT])?
        } else {
            g.output.DuplicateOutput(&g.device)?
        };
        // Build the persistent texture from the first real frame so its format
        // always matches what the duplication delivers (e.g. HDR / FP16).
        for _ in 0..30 {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res: Option<IDXGIResource> = None;
            match dupl.AcquireNextFrame(100, &mut info, &mut res) {
                Ok(()) => {
                    let mut made = None;
                    if let (Some(res), true) = (res, info.LastPresentTime != 0) {
                        let frame: ID3D11Texture2D = res.cast()?;
                        let mut d = D3D11_TEXTURE2D_DESC::default();
                        frame.GetDesc(&mut d);
                        if debug_log() {
                            println!("capture format {:?} {}x{}", d.Format, d.Width, d.Height);
                        }
                        d.Usage = D3D11_USAGE_DEFAULT;
                        d.BindFlags = D3D11_BIND_SHADER_RESOURCE.0 as u32;
                        d.CPUAccessFlags = 0;
                        d.MiscFlags = 0;
                        let mut desktop = None;
                        g.device.CreateTexture2D(&d, None, Some(&mut desktop))?;
                        let desktop: ID3D11Texture2D = desktop.unwrap();
                        let mut srv = None;
                        g.device.CreateShaderResourceView(&desktop, None, Some(&mut srv))?;
                        g.ctx.CopyResource(&desktop, &frame);
                        made = Some((desktop, srv.unwrap()));
                    }
                    dupl.ReleaseFrame()?;
                    if let Some((desktop, srv)) = made {
                        return Ok(Capture { dupl, desktop, srv });
                    }
                }
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {}
                Err(e) => return Err(e),
            }
        }
        Err(DXGI_ERROR_WAIT_TIMEOUT.into())
    }
}

/// Copy a new desktop frame into our persistent texture. Ok(true) if one arrived.
fn grab(g: &Gfx, cap: &Capture, timeout_ms: u32) -> windows::core::Result<bool> {
    unsafe {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut res: Option<IDXGIResource> = None;
        match cap.dupl.AcquireNextFrame(timeout_ms, &mut info, &mut res) {
            Ok(()) => {
                let mut copied = false;
                // Frames with LastPresentTime == 0 are cursor-only: no desktop image.
                if let (Some(res), true) = (res, info.LastPresentTime != 0) {
                    let frame: ID3D11Texture2D = res.cast()?;
                    g.ctx.CopyResource(&cap.desktop, &frame);
                    copied = true;
                }
                cap.dupl.ReleaseFrame()?;
                if debug_log() {
                    println!("acquire: copied={copied} lastPresent={} accumulated={}", info.LastPresentTime, info.AccumulatedFrames);
                }
                Ok(copied)
            }
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => Ok(false),
            Err(e) => Err(e),
        }
    }
}

/// Debug: read back one pixel (BGRA) from a texture.
fn probe_px(g: &Gfx, tex: &ID3D11Texture2D, x: u32, y: u32) -> windows::core::Result<[u8; 4]> {
    unsafe {
        let mut d = D3D11_TEXTURE2D_DESC::default();
        tex.GetDesc(&mut d);
        d.Width = 1;
        d.Height = 1;
        d.Usage = D3D11_USAGE_STAGING;
        d.BindFlags = 0;
        d.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        d.MiscFlags = 0;
        let mut st = None;
        g.device.CreateTexture2D(&d, None, Some(&mut st))?;
        let st: ID3D11Texture2D = st.unwrap();
        let b = D3D11_BOX { left: x, top: y, front: 0, right: x + 1, bottom: y + 1, back: 1 };
        g.ctx.CopySubresourceRegion(&st, 0, 0, 0, 0, tex, 0, Some(&b));
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        g.ctx.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
        let p = std::slice::from_raw_parts(m.pData as *const u8, 4);
        let out = [p[0], p[1], p[2], p[3]];
        g.ctx.Unmap(&st, 0);
        Ok(out)
    }
}

fn draw(g: &Gfx, cap: &Capture, zoom: f32, probe: bool) -> windows::core::Result<()> {
    unsafe {
        let mut pt = POINT::default();
        GetCursorPos(&mut pt)?;
        let cb = CB {
            // Relative to the zoomed monitor and kept on it, so the view stays valid
            // even while the pointer is over another screen.
            cursor: [((pt.x - g.origin.0) as f32).clamp(0.0, g.w as f32), ((pt.y - g.origin.1) as f32).clamp(0.0, g.h as f32)],
            size: [g.w as f32, g.h as f32],
            zoom,
            mode: FILTER.load(Ordering::Relaxed),
            sharpen: getf(&STRENGTH),
            smode: SHARPEN.load(Ordering::Relaxed),
            flags: BGR_PANEL.load(Ordering::Relaxed) as u32,
            _pad: [0; 3],
        };
        g.ctx.UpdateSubresource(&g.cb, 0, None, &cb as *const _ as *const _, 0, 0);
        g.ctx.OMSetRenderTargets(Some(&[Some(g.rtv.clone())]), None);
        g.ctx.RSSetViewports(Some(&[D3D11_VIEWPORT { Width: g.w as f32, Height: g.h as f32, MaxDepth: 1.0, ..Default::default() }]));
        g.ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        g.ctx.VSSetShader(&g.vs, None);
        g.ctx.PSSetShader(&g.ps, None);
        g.ctx.PSSetConstantBuffers(0, Some(&[Some(g.cb.clone())]));
        g.ctx.PSSetShaderResources(0, Some(&[Some(cap.srv.clone())]));
        g.ctx.PSSetSamplers(0, Some(&[Some(g.sampler.clone())]));
        g.ctx.Draw(3, 0);
        if probe {
            let back: ID3D11Texture2D = g.swap.GetBuffer(0)?;
            let (x, y) = (g.w / 2, g.h / 2);
            println!("probe center: desktop {:?} backbuffer {:?}", probe_px(g, &cap.desktop, x, y)?, probe_px(g, &back, x, y)?);
        }
        g.swap.Present(1, DXGI_PRESENT(0)).ok()
    }
}

fn build_gfx() -> Option<Gfx> {
    let gen_now = MONITOR_GEN.load(Ordering::Relaxed);
    match init_gfx(&monitor_id(), gen_now) {
        Ok(g) => Some(g),
        Err(e) => {
            eprintln!("could not set up the display: {e}");
            None
        }
    }
}

/// Run the low-level mouse hook on its own thread, so a busy render thread
/// (or a slow capture start) can never make Windows time the hook out.
fn spawn_mouse_hook() -> Option<u32> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || unsafe {
        let hook = GetModuleHandleW(None).ok().and_then(|h| SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), Some(h.into()), 0).ok());
        let _ = tx.send((hook.is_some(), GetCurrentThreadId()));
        if hook.is_none() {
            return;
        }
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {}
        if let Some(h) = hook {
            let _ = UnhookWindowsHookEx(h);
        }
    });
    match rx.recv() {
        Ok((true, tid)) => Some(tid),
        _ => None,
    }
}

fn main() -> windows::core::Result<()> {
    unsafe {
        // Lets println! reach the terminal when launched from one (we are a GUI-subsystem exe).
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)?;
        let selftest = std::env::args().any(|a| a == "--selftest");

        // Single instance.
        let _instance = CreateMutexW(None, true, w!("Local\\pixelzoom_single_instance"))?;
        if !selftest && GetLastError() == ERROR_ALREADY_EXISTS {
            println!("pixelzoom is already running");
            return Ok(());
        }

        settings::load();
        MAIN_THREAD.store(GetCurrentThreadId(), Ordering::Relaxed);
        // GPU state for the selected monitor. If it can't be built now (no display
        // yet, driver reset) the loop retries instead of the app dying.
        let mut gfx: Option<Gfx> = build_gfx();
        let mut next_retry = Instant::now();

        // Hotkeys are conveniences: another app owning one must not stop us starting.
        let hk = |id: i32, mods: HOT_KEY_MODIFIERS, vk: u32| {
            if RegisterHotKey(None, id, mods, vk).is_err() {
                println!("hotkey {id} unavailable (already taken)");
            }
        };
        let cam = MOD_CONTROL | MOD_ALT;
        hk(1, cam | MOD_NOREPEAT, 'Q' as u32); // emergency quit
        hk(3, cam, VK_OEM_PLUS.0 as u32); // keyboard zoom in
        hk(4, cam, VK_OEM_MINUS.0 as u32); // keyboard zoom out

        let tray_hwnd = tray::create_window()?;
        if selftest {
            set_target(2.0);
        } else {
            tray::add_icon(tray_hwnd);
            tray::refresh_autostart();
        }
        let hook_thread = if selftest {
            None
        } else {
            println!("running (tray icon). Emergency quit: Ctrl+Alt+Q");
            let t = spawn_mouse_hook();
            if t.is_none() {
                eprintln!("could not install the mouse hook; use Ctrl+Alt+= / - to zoom");
            }
            t
        };

        let mut zoom = 1.0f32;
        let mut shown = false;
        let mut last = Instant::now();
        let mut frames = 0u32;
        let mut frame_time = Duration::ZERO;
        let test_start = Instant::now();
        let mut msg = MSG::default();

        // Leave zoom mode: reset the target, hide the overlay, release the capture.
        let leave_zoom = |zoom: &mut f32, shown: &mut bool, gfx: &Option<Gfx>| {
            set_target(1.0);
            *zoom = 1.0;
            if let Some(g) = gfx {
                let _ = ShowWindow(g.overlay.hwnd, SW_HIDE);
            }
            *shown = false;
            CAPTURE.with(|c| *c.borrow_mut() = None);
        };
        let mut errors = 0u32;

        'outer: loop {
            // Display setup changed (resolution, HDR, monitors, resume) or another
            // monitor was picked: drop everything and rebuild for the new state.
            let stale =
                DISPLAY_DIRTY.load(Ordering::Relaxed) || gfx.as_ref().is_some_and(|g| g.generation != MONITOR_GEN.load(Ordering::Relaxed));
            if stale || (gfx.is_none() && Instant::now() >= next_retry) {
                DISPLAY_DIRTY.store(false, Ordering::Relaxed);
                leave_zoom(&mut zoom, &mut shown, &gfx);
                drop(gfx.take()); // drops the swapchain, then the overlay window
                gfx = build_gfx();
                next_retry = Instant::now() + Duration::from_secs(2);
                errors = 0;
            }
            let active = zoom > 1.0 || target() > 1.0;
            // Idle: block in GetMessage so we cost nothing. Active: poll and render.
            let got = if active {
                PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool()
            } else {
                // GetMessage returns 0 for WM_QUIT (the tray's Exit) instead of
                // handing it over, so it has to end the loop here.
                match GetMessageW(&mut msg, None, 0, 0).0 {
                    0 => break 'outer,
                    r => r > 0,
                }
            };
            if got {
                match msg.message {
                    WM_QUIT => break 'outer,
                    WM_HOTKEY if msg.wParam.0 == 1 => break 'outer,
                    WM_HOTKEY if msg.wParam.0 == 3 || msg.wParam.0 == 4 => {
                        let notches = if msg.wParam.0 == 3 { 1.0 } else { -1.0 };
                        set_target(step_zoom(target(), notches));
                        println!("zoom target {:.2}", target());
                    }
                    WM_APP => println!("wheel: zoom target {:.2}", target()),
                    _ => {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
                continue;
            }
            if !active {
                continue;
            }

            let now = Instant::now();
            let dt = now.duration_since(last).as_secs_f32().min(0.1);
            last = now;
            let t = target();
            zoom += (t - zoom) * (1.0 - (-dt * EASE_RATE).exp());
            if (t - zoom).abs() < 0.003 {
                zoom = t;
            }

            let Some(g) = gfx.as_ref() else {
                // No usable display state: refuse to zoom rather than spin.
                leave_zoom(&mut zoom, &mut shown, &gfx);
                continue;
            };

            if zoom <= 1.0 && t <= 1.0 {
                leave_zoom(&mut zoom, &mut shown, &gfx);
                if selftest {
                    break 'outer;
                }
                continue;
            }

            let t0 = Instant::now();
            let r: windows::core::Result<()> = CAPTURE.with(|c| {
                let mut c = c.borrow_mut();
                if c.is_none() {
                    *c = Some(start_capture(g)?);
                }
                let cap = c.as_ref().unwrap();
                match grab(g, cap, 0) {
                    Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                        *c = None; // recreated next frame
                        return Ok(());
                    }
                    Err(e) => return Err(e),
                    Ok(_) => {}
                }
                draw(g, cap, zoom, selftest && frames == 20)
            });
            if let Err(e) = r {
                eprintln!("render error: {e}");
                CAPTURE.with(|c| *c.borrow_mut() = None);
                errors += 1;
                // GPU lost or reset: everything must be rebuilt.
                if e.code() == DXGI_ERROR_DEVICE_REMOVED || e.code() == DXGI_ERROR_DEVICE_RESET {
                    DISPLAY_DIRTY.store(true, Ordering::Relaxed);
                }
                // Failsafe: capture keeps failing (secure desktop, protected content,
                // lock screen). Drop out of zoom instead of leaving a stuck overlay.
                if errors >= 5 {
                    eprintln!("giving up on this zoom attempt");
                    leave_zoom(&mut zoom, &mut shown, &gfx);
                    errors = 0;
                }
                std::thread::sleep(Duration::from_millis(40));
                continue;
            }
            errors = 0;
            if !shown {
                let _ = SetWindowPos(
                    g.overlay.hwnd,
                    Some(HWND_TOPMOST),
                    g.origin.0,
                    g.origin.1,
                    g.w as i32,
                    g.h as i32,
                    SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
                shown = true;
                println!("overlay shown");
            }
            frames += 1;
            frame_time += t0.elapsed();

            if selftest && test_start.elapsed() > Duration::from_millis(1500) && target() > 1.0 {
                set_target(1.0);
            }
        }

        if let Some(tid) = hook_thread {
            let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        tray::remove_icon(tray_hwnd);
        let _ = DestroyWindow(tray_hwnd);
        drop(gfx);
        if selftest && frames > 0 {
            println!(
                "selftest ok: {frames} frames, avg {:.2} ms/frame (includes vsync wait)",
                frame_time.as_secs_f64() * 1000.0 / frames as f64
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_buffer_is_whole_rows() {
        assert_eq!(size_of::<CB>() % 16, 0);
    }

    // The HLSL is only compiled at runtime, so a typo would otherwise first
    // show up as an app that cannot start. Compile both entry points here.
    #[test]
    fn shader_entry_points_compile() {
        for (entry, target) in [("vs_main", "vs_5_0"), ("ps_main", "ps_5_0")] {
            let bytes = compile_uncached(entry, target).unwrap_or_else(|e| panic!("{entry} ({target}) failed to compile: {e}"));
            assert!(!bytes.is_empty(), "{entry} produced no bytecode");
        }
    }
}
