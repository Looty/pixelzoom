//! Settings window, custom-drawn with Direct2D + DirectWrite (follows the system
//! light/dark theme). It only exists while open, so it costs nothing at idle.

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering::Relaxed};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Gdi::{InvalidateRect, ValidateRect};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::*;
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::core::w;
use windows_numerics::Vector2;

use crate::monitors::{self, Monitor};
use crate::settings::*;
use crate::tray;

const WM_MOUSELEAVE: u32 = 0x02A3;
const WM_REC: u32 = WM_APP + 2; // wparam 0 = keys changed, 1 = recording finished

// Logical (96 dpi) size of the client area. The screen picker adds height below
// the base layout, and only when more than one monitor is connected.
const CLIENT_W: f32 = 440.0;
const BASE_H: f32 = 372.0;
const SHOT_TOP: f32 = 408.0;
const SHOT_W: f32 = 120.0;
const SHOT_H: f32 = 118.0;
const SHOT_GAP_X: f32 = 16.0;
const SHOT_GAP_Y: f32 = 12.0;
const SHOT_COLS: usize = 3;
const THUMB_W: i32 = 320;
const THUMB_H: i32 = 180;

fn client_h(monitor_count: usize) -> f32 {
    if monitor_count <= 1 {
        return BASE_H;
    }
    let rows = monitor_count.div_ceil(SHOT_COLS) as f32;
    SHOT_TOP + rows * (SHOT_H + SHOT_GAP_Y) + 8.0
}

fn screen_rect(i: usize) -> R {
    let (col, row) = ((i % SHOT_COLS) as f32, (i / SHOT_COLS) as f32);
    R::new(24.0 + col * (SHOT_W + SHOT_GAP_X), SHOT_TOP + row * (SHOT_H + SHOT_GAP_Y), SHOT_W, SHOT_H)
}

/// Index of the monitor to highlight: the saved one, else the primary, else the first.
fn selected_index(mons: &[Monitor], id: &str) -> usize {
    mons.iter().position(|m| m.id == id).or_else(|| mons.iter().position(|m| m.primary)).unwrap_or(0)
}

thread_local! {
    /// Monitors and their thumbnails, grabbed just before the window opens so the
    /// settings window itself isn't in the previews.
    static PENDING: RefCell<Option<(Vec<Monitor>, Vec<Option<Vec<u8>>>)>> = const { RefCell::new(None) };
}

// ---- recorder state (shared with the keyboard hook) ----------------------

struct Rec {
    listening: bool,
    keys: Vec<u32>,
    down: Vec<u32>,
}

static REC: Mutex<Rec> = Mutex::new(Rec { listening: false, keys: Vec::new(), down: Vec::new() });
static WIN: AtomicIsize = AtomicIsize::new(0);
static KBD_HOOK: AtomicIsize = AtomicIsize::new(0);
static REJECTED: AtomicBool = AtomicBool::new(false);

fn rec() -> std::sync::MutexGuard<'static, Rec> {
    REC.lock().unwrap_or_else(|e| e.into_inner())
}

fn hwnd_of(v: isize) -> HWND {
    HWND(v as *mut c_void)
}

// ---- layout ----------------------------------------------------------------

#[derive(Clone, Copy)]
struct R {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl R {
    const fn new(x: f32, y: f32, w: f32, h: f32) -> R {
        R { x, y, w, h }
    }
    fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
    fn d2d(&self) -> D2D_RECT_F {
        D2D_RECT_F { left: self.x, top: self.y, right: self.x + self.w, bottom: self.y + self.h }
    }
    fn rounded(&self, r: f32) -> D2D1_ROUNDED_RECT {
        D2D1_ROUNDED_RECT { rect: self.d2d(), radiusX: r, radiusY: r }
    }
    fn inset(&self, d: f32) -> R {
        R::new(self.x + d, self.y + d, self.w - 2.0 * d, self.h - 2.0 * d)
    }
}

const CARD: R = R::new(24.0, 76.0, 392.0, 100.0);
const ROW1: R = R::new(24.0, 76.0, 392.0, 50.0);
const ROW2: R = R::new(24.0, 126.0, 392.0, 50.0);
const SWITCH1: R = R::new(360.0, 89.0, 44.0, 24.0);
const SWITCH2: R = R::new(360.0, 139.0, 44.0, 24.0);
const BTN_RECORD: R = R::new(24.0, 296.0, 110.0, 36.0);
const BTN_CLEAR: R = R::new(144.0, 296.0, 90.0, 36.0);

const CHIP_W: f32 = 120.0;
const CHIP_GAP: f32 = 32.0; // room for the "+" between boxes

fn chip_rect(i: usize) -> R {
    R::new(24.0 + i as f32 * (CHIP_W + CHIP_GAP), 232.0, CHIP_W, 46.0)
}

#[derive(Clone, Copy, PartialEq)]
enum Hit {
    Enable,
    Autostart,
    Chip,
    Record,
    Clear,
    Screen(usize),
}

fn hit_test(x: f32, y: f32, monitor_count: usize) -> Option<Hit> {
    if monitor_count > 1 {
        if let Some(i) = (0..monitor_count).find(|i| screen_rect(*i).contains(x, y)) {
            return Some(Hit::Screen(i));
        }
    }
    if ROW1.contains(x, y) {
        Some(Hit::Enable)
    } else if ROW2.contains(x, y) {
        Some(Hit::Autostart)
    } else if (0..MAX_COMBO_KEYS).any(|i| chip_rect(i).contains(x, y)) {
        Some(Hit::Chip)
    } else if BTN_RECORD.contains(x, y) {
        Some(Hit::Record)
    } else if BTN_CLEAR.contains(x, y) {
        Some(Hit::Clear)
    } else {
        None
    }
}

// ---- theme -----------------------------------------------------------------

struct Theme {
    bg: u32,
    card: u32,
    border: u32,
    text: u32,
    sub: u32,
    off: u32,
    accent: u32,
}

const DARK: Theme = Theme { bg: 0x1F1F1F, card: 0x2B2B2B, border: 0x3D3D3D, text: 0xFFFFFF, sub: 0x9A9A9A, off: 0x6B6B6B, accent: 0x4A9EFF };
const LIGHT: Theme = Theme { bg: 0xF3F3F3, card: 0xFFFFFF, border: 0xD6D6D6, text: 0x1A1A1A, sub: 0x6B6B6B, off: 0xB4B4B4, accent: 0x2F7FE6 };

fn system_is_dark() -> bool {
    unsafe {
        let mut data = 1u32;
        let mut size = 4u32;
        let r = RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut u32 as *mut c_void),
            Some(&mut size),
        );
        r.is_ok() && data == 0
    }
}

fn color(hex: u32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((hex >> 16) & 0xFF) as f32 / 255.0,
        g: ((hex >> 8) & 0xFF) as f32 / 255.0,
        b: (hex & 0xFF) as f32 / 255.0,
        a,
    }
}

// ---- drawing resources -----------------------------------------------------

struct Ui {
    _factory: ID2D1Factory,
    _dw: IDWriteFactory,
    rt: ID2D1HwndRenderTarget,
    brush: ID2D1SolidColorBrush,
    title: IDWriteTextFormat,
    body: IDWriteTextFormat,
    small: IDWriteTextFormat,
    centered: IDWriteTextFormat,
    theme: &'static Theme,
    hover: Option<Hit>,
    tracking: bool,
    autostart: bool,
    monitors: Vec<Monitor>,
    thumbs: Vec<Option<ID2D1Bitmap>>,
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

fn format(dw: &IDWriteFactory, weight: DWRITE_FONT_WEIGHT, size: f32) -> windows::core::Result<IDWriteTextFormat> {
    unsafe {
        let f = dw.CreateTextFormat(w!("Segoe UI"), None, weight, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, size, w!("en-us"))?;
        f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
        Ok(f)
    }
}

fn create_ui(hwnd: HWND) -> windows::core::Result<Ui> {
    unsafe {
        let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let dpi = GetDpiForWindow(hwnd) as f32;
        let mut rc = RECT::default();
        GetClientRect(hwnd, &mut rc)?;
        let rt = factory.CreateHwndRenderTarget(
            &D2D1_RENDER_TARGET_PROPERTIES { dpiX: dpi, dpiY: dpi, ..Default::default() },
            &D2D1_HWND_RENDER_TARGET_PROPERTIES {
                hwnd,
                pixelSize: D2D_SIZE_U { width: (rc.right - rc.left) as u32, height: (rc.bottom - rc.top) as u32 },
                presentOptions: D2D1_PRESENT_OPTIONS_NONE,
            },
        )?;
        let brush = rt.CreateSolidColorBrush(&color(0, 1.0), None)?;
        let centered = format(&dw, DWRITE_FONT_WEIGHT_SEMI_BOLD, 17.0)?;
        centered.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
        let dark = system_is_dark();
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &(dark as i32) as *const i32 as *const c_void, 4);
        let (monitors, shots) = PENDING.with(|p| p.borrow_mut().take()).unwrap_or_default();
        let thumbs = shots
            .iter()
            .map(|s| {
                s.as_ref().and_then(|px| {
                    rt.CreateBitmap(
                        D2D_SIZE_U { width: THUMB_W as u32, height: THUMB_H as u32 },
                        Some(px.as_ptr() as *const c_void),
                        (THUMB_W * 4) as u32,
                        &D2D1_BITMAP_PROPERTIES {
                            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_IGNORE },
                            dpiX: 96.0,
                            dpiY: 96.0,
                        },
                    )
                    .ok()
                })
            })
            .collect();
        Ok(Ui {
            monitors,
            thumbs,
            title: format(&dw, DWRITE_FONT_WEIGHT_SEMI_BOLD, 24.0)?,
            body: format(&dw, DWRITE_FONT_WEIGHT_NORMAL, 15.0)?,
            small: format(&dw, DWRITE_FONT_WEIGHT_NORMAL, 13.0)?,
            centered,
            _factory: factory,
            _dw: dw,
            rt,
            brush,
            theme: if dark { &DARK } else { &LIGHT },
            hover: None,
            tracking: false,
            autostart: tray::autostart_enabled(),
        })
    }
}

impl Ui {
    fn fill(&self, rr: &D2D1_ROUNDED_RECT, hex: u32, a: f32) {
        unsafe {
            self.brush.SetColor(&color(hex, a));
            self.rt.FillRoundedRectangle(rr, &self.brush);
        }
    }

    fn stroke(&self, rr: &D2D1_ROUNDED_RECT, hex: u32, width: f32) {
        unsafe {
            self.brush.SetColor(&color(hex, 1.0));
            self.rt.DrawRoundedRectangle(rr, &self.brush, width, None);
        }
    }

    fn text(&self, s: &str, fmt: &IDWriteTextFormat, r: R, hex: u32) {
        unsafe {
            self.brush.SetColor(&color(hex, 1.0));
            let s: Vec<u16> = s.encode_utf16().collect();
            self.rt.DrawText(&s, fmt, &r.d2d(), &self.brush, D2D1_DRAW_TEXT_OPTIONS_CLIP, DWRITE_MEASURING_MODE_NATURAL);
        }
    }

    fn switch(&self, r: R, on: bool, hot: bool) {
        let t = self.theme;
        let track = r.rounded(r.h / 2.0);
        self.fill(&track, if on { t.accent } else { t.off }, if hot { 0.85 } else { 1.0 });
        let cx = if on { r.x + r.w - r.h / 2.0 } else { r.x + r.h / 2.0 };
        unsafe {
            self.brush.SetColor(&color(0xFFFFFF, 1.0));
            self.rt.FillEllipse(
                &D2D1_ELLIPSE { point: Vector2::new(cx, r.y + r.h / 2.0), radiusX: 8.0, radiusY: 8.0 },
                &self.brush,
            );
        }
    }

    fn button(&self, r: R, label: &str, primary: bool, hot: bool) {
        let t = self.theme;
        let rr = r.rounded(8.0);
        if primary {
            self.fill(&rr, t.accent, if hot { 0.88 } else { 1.0 });
            self.text(label, &self.centered, r, 0xFFFFFF);
        } else {
            self.fill(&rr, t.card, 1.0);
            if hot {
                self.fill(&rr, t.accent, 0.12);
            }
            self.stroke(&r.inset(0.5).rounded(8.0), if hot { t.accent } else { t.border }, 1.0);
            self.text(label, &self.centered, r, t.text);
        }
    }

    fn draw(&self) {
        let t = self.theme;
        let (listening, keys) = {
            let r = rec();
            (r.listening, if r.listening { r.keys.clone() } else { combo() })
        };
        unsafe {
            self.rt.BeginDraw();
            self.rt.Clear(Some(&color(t.bg, 1.0)));
        }

        self.text("Pixel Zoom", &self.title, R::new(24.0, 22.0, 392.0, 40.0), t.text);

        // Toggles card.
        self.fill(&CARD.rounded(10.0), t.card, 1.0);
        self.stroke(&CARD.inset(0.5).rounded(10.0), t.border, 1.0);
        unsafe {
            self.brush.SetColor(&color(t.border, 1.0));
            self.rt.FillRectangle(&R::new(40.0, 126.0, 360.0, 1.0).d2d(), &self.brush);
        }
        self.text("Enable pixelzoom", &self.body, R::new(40.0, 76.0, 300.0, 50.0), t.text);
        self.text("Start with Windows", &self.body, R::new(40.0, 126.0, 300.0, 50.0), t.text);
        self.switch(SWITCH1, ENABLED.load(Relaxed), self.hover == Some(Hit::Enable));
        self.switch(SWITCH2, self.autostart, self.hover == Some(Hit::Autostart));

        // Shortcut recorder.
        self.text("Zoom shortcut", &self.body, R::new(24.0, 188.0, 392.0, 24.0), t.text);
        self.text("A key plus a scroll direction. That direction zooms in.", &self.small, R::new(24.0, 208.0, 392.0, 20.0), t.sub);
        for i in 0..MAX_COMBO_KEYS {
            let r = chip_rect(i);
            let rr = r.rounded(8.0);
            let key = keys.get(i);
            let waiting = listening && i == keys.len();
            self.fill(&rr, t.card, 1.0);
            if key.is_some() {
                self.fill(&rr, t.accent, 0.14);
            }
            let hot = self.hover == Some(Hit::Chip);
            let border = if key.is_some() || waiting { t.accent } else if hot { t.sub } else { t.border };
            self.stroke(&r.inset(1.0).rounded(8.0), border, if waiting { 2.0 } else { 1.0 });
            match key {
                Some(k) => self.text(&vk_label(*k), &self.centered, r, t.text),
                None if waiting => self.text("…", &self.centered, r, t.accent),
                None => {}
            }
        }
        for i in 0..MAX_COMBO_KEYS - 1 {
            let c = chip_rect(i);
            self.text("+", &self.centered, R::new(c.x + c.w, c.y, CHIP_GAP, c.h), t.sub);
        }

        self.button(BTN_RECORD, if listening { "Listening…" } else { "Record" }, true, self.hover == Some(Hit::Record));
        self.button(BTN_CLEAR, "Clear", false, self.hover == Some(Hit::Clear));
        let status = if listening {
            "Hold a key (e.g. Ctrl), scroll up or down, then release."
        } else if REJECTED.load(Relaxed) {
            "A key is required too, so scrolling alone isn't used. Reset to Ctrl."
        } else {
            "Click a box or Record to change it."
        };
        self.text(status, &self.small, R::new(24.0, 342.0, 392.0, 20.0), t.sub);

        // Screen picker (only with several monitors): thumbnail, model name, resolution.
        if self.monitors.len() > 1 {
            self.text("Screen to zoom", &self.body, R::new(24.0, 376.0, 392.0, 24.0), t.text);
            let sel = selected_index(&self.monitors, &monitor_id());
            for (i, m) in self.monitors.iter().enumerate() {
                let r = screen_rect(i);
                let rr = r.rounded(10.0);
                let chosen = i == sel;
                let hot = self.hover == Some(Hit::Screen(i));
                self.fill(&rr, t.card, 1.0);
                if chosen {
                    self.fill(&rr, t.accent, 0.14);
                }
                let border = if chosen { t.accent } else if hot { t.sub } else { t.border };
                self.stroke(&r.inset(1.0).rounded(10.0), border, if chosen { 2.0 } else { 1.0 });
                let shot = R::new(r.x + 6.0, r.y + 6.0, r.w - 12.0, 61.0);
                match self.thumbs.get(i).and_then(|b| b.as_ref()) {
                    Some(bmp) => unsafe {
                        self.rt.DrawBitmap(bmp, Some(&shot.d2d()), 1.0, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, None);
                    },
                    None => self.fill(&shot.rounded(4.0), t.border, 1.0),
                }
                self.text(&m.name, &self.small, R::new(r.x + 8.0, r.y + 70.0, r.w - 16.0, 22.0), t.text);
                let detail = format!("{}×{}{}", m.width(), m.height(), if m.primary { " · Primary" } else { "" });
                self.text(&detail, &self.small, R::new(r.x + 8.0, r.y + 92.0, r.w - 16.0, 20.0), t.sub);
            }
        }

        unsafe {
            if let Err(e) = self.rt.EndDraw(None, None) {
                if e.code() == D2DERR_RECREATE_TARGET {
                    UI.with(|u| *u.borrow_mut() = None);
                }
            }
        }
    }
}

fn repaint(hwnd: HWND) {
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

// ---- keyboard recorder -----------------------------------------------------

unsafe extern "system" fn kbd_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if code == HC_ACTION as i32 {
            let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            let vk = normalize_vk(info.vkCode);
            let msg = wparam.0 as u32;
            let mut r = rec();
            if r.listening {
                let target = hwnd_of(WIN.load(Relaxed));
                if msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN {
                    if !r.down.contains(&vk) {
                        r.down.push(vk);
                    }
                    if !r.keys.contains(&vk) && r.keys.len() < MAX_COMBO_KEYS {
                        r.keys.push(vk);
                    }
                    let _ = PostMessageW(Some(target), WM_REC, WPARAM(0), LPARAM(0));
                } else {
                    r.down.retain(|k| *k != vk);
                    if r.down.is_empty() && !r.keys.is_empty() {
                        r.listening = false;
                        let _ = PostMessageW(Some(target), WM_REC, WPARAM(1), LPARAM(0));
                    }
                }
                return LRESULT(1); // swallow while recording
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }
}

/// Called from the wheel hook. While recording, a wheel notch becomes part of the
/// shortcut (its direction is the one that zooms in) and is swallowed.
pub fn recording_wheel(notches: f32) -> bool {
    let mut r = rec();
    if !r.listening {
        return false;
    }
    let wheel = if notches > 0.0 { K_WHEEL_UP } else { K_WHEEL_DOWN };
    if !r.keys.iter().any(|k| is_wheel(*k)) && r.keys.len() < MAX_COMBO_KEYS {
        r.keys.push(wheel);
    }
    let target = hwnd_of(WIN.load(Relaxed));
    if r.down.is_empty() {
        r.listening = false; // nothing held: the wheel alone ends the recording
        let _ = unsafe { PostMessageW(Some(target), WM_REC, WPARAM(1), LPARAM(0)) };
    } else {
        let _ = unsafe { PostMessageW(Some(target), WM_REC, WPARAM(0), LPARAM(0)) };
    }
    true
}

fn unhook() {
    let h = KBD_HOOK.swap(0, Relaxed);
    if h != 0 {
        unsafe {
            let _ = UnhookWindowsHookEx(HHOOK(h as *mut c_void));
        }
    }
}

fn start_listening(hwnd: HWND) {
    REJECTED.store(false, Relaxed);
    {
        let mut r = rec();
        r.listening = true;
        r.keys.clear();
        r.down.clear();
    }
    if KBD_HOOK.load(Relaxed) == 0 {
        unsafe {
            let hinst: HINSTANCE = GetModuleHandleW(None).map(Into::into).unwrap_or_default();
            if let Ok(h) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(kbd_hook), Some(hinst), 0) {
                KBD_HOOK.store(h.0 as isize, Relaxed);
            }
        }
    }
    repaint(hwnd);
}

fn stop_listening() {
    rec().listening = false;
    unhook();
}

// ---- window ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(id: &str, primary: bool) -> Monitor {
        Monitor { id: id.into(), name: id.into(), rect: RECT { left: 0, top: 0, right: 10, bottom: 10 }, primary }
    }

    #[test]
    fn selection_prefers_saved_then_primary_then_first() {
        let v = vec![mon("A", false), mon("B", true), mon("C", false)];
        assert_eq!(selected_index(&v, "C"), 2);
        assert_eq!(selected_index(&v, ""), 1);
        assert_eq!(selected_index(&v, "gone"), 1);
        let no_primary = vec![mon("A", false), mon("B", false)];
        assert_eq!(selected_index(&no_primary, "gone"), 0);
        assert_eq!(selected_index(&[], "x"), 0);
    }

    #[test]
    fn window_grows_only_with_several_monitors() {
        assert_eq!(client_h(0), BASE_H);
        assert_eq!(client_h(1), BASE_H);
        assert!(client_h(2) > BASE_H);
        assert_eq!(client_h(2), client_h(3)); // one row up to three monitors
        assert!(client_h(4) > client_h(3)); // then a second row
    }

    #[test]
    fn screen_cards_form_a_grid_inside_the_window() {
        for i in 0..6 {
            let r = screen_rect(i);
            assert!(r.x >= 24.0 && r.x + r.w <= CLIENT_W - 24.0 + 0.5, "card {i} x-range");
            assert!(r.y + r.h <= client_h(6), "card {i} fits vertically");
        }
        assert_eq!(screen_rect(0).y, screen_rect(2).y);
        assert!(screen_rect(3).y > screen_rect(0).y);
        assert_eq!(screen_rect(3).x, screen_rect(0).x);
    }

    #[test]
    fn hit_testing_finds_controls() {
        assert!(matches!(hit_test(100.0, 100.0, 1), Some(Hit::Enable)));
        assert!(matches!(hit_test(100.0, 150.0, 1), Some(Hit::Autostart)));
        assert!(matches!(hit_test(chip_rect(0).x + 5.0, chip_rect(0).y + 5.0, 1), Some(Hit::Chip)));
        assert!(matches!(hit_test(BTN_RECORD.x + 5.0, BTN_RECORD.y + 5.0, 1), Some(Hit::Record)));
        assert!(matches!(hit_test(BTN_CLEAR.x + 5.0, BTN_CLEAR.y + 5.0, 1), Some(Hit::Clear)));
        assert!(hit_test(5.0, 5.0, 1).is_none());
    }

    #[test]
    fn screen_cards_only_hit_with_several_monitors() {
        let (x, y) = (screen_rect(1).x + 5.0, screen_rect(1).y + 5.0);
        assert!(hit_test(x, y, 1).is_none());
        assert!(matches!(hit_test(x, y, 3), Some(Hit::Screen(1))));
        assert!(hit_test(x, y, 1).is_none());
    }

    #[test]
    fn rect_helpers() {
        let r = R::new(10.0, 20.0, 30.0, 40.0);
        assert!(r.contains(10.0, 20.0) && r.contains(39.9, 59.9));
        assert!(!r.contains(40.0, 20.0) && !r.contains(9.9, 30.0));
        let i = r.inset(5.0);
        assert_eq!((i.x, i.y, i.w, i.h), (15.0, 25.0, 20.0, 30.0));
    }

    #[test]
    fn colors_split_into_channels() {
        let c = color(0xFF8000, 0.5);
        assert!((c.r - 1.0).abs() < 1e-6 && (c.g - 128.0 / 255.0).abs() < 1e-6 && c.b == 0.0 && c.a == 0.5);
    }
}

/// Show the settings window (create it the first time).
pub fn open() {
    unsafe {
        let existing = hwnd_of(WIN.load(Relaxed));
        if !existing.0.is_null() && IsWindow(Some(existing)).as_bool() {
            let _ = ShowWindow(existing, SW_SHOW);
            let _ = SetForegroundWindow(existing);
            return;
        }
        let Ok(hinst) = GetModuleHandleW(None) else { return };
        let hinst: HINSTANCE = hinst.into();
        // Grab monitor previews before our own window exists.
        let mons = monitors::list();
        let shots = if mons.len() > 1 { mons.iter().map(|m| monitors::thumbnail(m, THUMB_W, THUMB_H)).collect() } else { Vec::new() };
        let monitor_count = mons.len();
        PENDING.with(|p| *p.borrow_mut() = Some((mons, shots)));
        let class = w!("pixelzoom_settings");
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(proc),
            hInstance: hinst,
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hIcon: tray::make_icon(GetSystemMetrics(SM_CXICON)),
            hIconSm: tray::make_icon(GetSystemMetrics(SM_CXSMICON)),
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU;
        let Ok(h) = CreateWindowExW(WINDOW_EX_STYLE(0), class, w!("Pixel Zoom"), style, 0, 0, 100, 100, None, None, Some(hinst), None) else {
            return;
        };
        WIN.store(h.0 as isize, Relaxed);

        // Size the client area for this monitor's DPI and centre the window.
        let dpi = GetDpiForWindow(h).max(96);
        let scale = dpi as f32 / 96.0;
        let mut rc = RECT { left: 0, top: 0, right: (CLIENT_W * scale) as i32, bottom: (client_h(monitor_count) * scale) as i32 };
        let _ = AdjustWindowRectExForDpi(&mut rc, style, false, WINDOW_EX_STYLE(0), dpi);
        let (ww, wh) = (rc.right - rc.left, rc.bottom - rc.top);
        let x = (GetSystemMetrics(SM_CXSCREEN) - ww) / 2;
        let y = (GetSystemMetrics(SM_CYSCREEN) - wh) / 2;
        let _ = SetWindowPos(h, None, x, y, ww, wh, SWP_NOZORDER);
        let _ = ShowWindow(h, SW_SHOW);
        let _ = SetForegroundWindow(h);
    }
}

fn monitor_count() -> usize {
    UI.with(|u| u.borrow().as_ref().map_or(0, |ui| ui.monitors.len()))
}

fn dips(hwnd: HWND, lparam: LPARAM) -> (f32, f32) {
    unsafe {
        let scale = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;
        let x = (lparam.0 & 0xFFFF) as i16 as f32;
        let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
        (x / scale, y / scale)
    }
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_PAINT => {
                UI.with(|u| {
                    let mut u = u.borrow_mut();
                    if u.is_none() {
                        *u = create_ui(hwnd).ok();
                    }
                    if let Some(ui) = u.as_ref() {
                        ui.draw();
                    }
                });
                let _ = ValidateRect(Some(hwnd), None);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_SIZE => {
                UI.with(|u| {
                    if let Some(ui) = u.borrow().as_ref() {
                        let _ = ui.rt.Resize(&D2D_SIZE_U { width: (lparam.0 & 0xFFFF) as u32, height: ((lparam.0 >> 16) & 0xFFFF) as u32 });
                    }
                });
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let (x, y) = dips(hwnd, lparam);
                let hit = hit_test(x, y, monitor_count());
                let mut changed = false;
                UI.with(|u| {
                    if let Some(ui) = u.borrow_mut().as_mut() {
                        if ui.hover != hit {
                            ui.hover = hit;
                            changed = true;
                        }
                        if !ui.tracking {
                            ui.tracking = true;
                            let mut t = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                            let _ = TrackMouseEvent(&mut t);
                        }
                    }
                });
                if changed {
                    repaint(hwnd);
                }
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                UI.with(|u| {
                    if let Some(ui) = u.borrow_mut().as_mut() {
                        ui.hover = None;
                        ui.tracking = false;
                    }
                });
                repaint(hwnd);
                LRESULT(0)
            }
            WM_SETCURSOR => {
                let over = UI.with(|u| u.borrow().as_ref().and_then(|ui| ui.hover).is_some());
                if over && (lparam.0 & 0xFFFF) as u32 == HTCLIENT {
                    SetCursor(LoadCursorW(None, IDC_HAND).ok());
                    return LRESULT(1);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_LBUTTONDOWN => {
                let (x, y) = dips(hwnd, lparam);
                match hit_test(x, y, monitor_count()) {
                    Some(Hit::Screen(i)) => {
                        let id = UI.with(|u| u.borrow().as_ref().and_then(|ui| ui.monitors.get(i).map(|m| m.id.clone())));
                        if let Some(id) = id {
                            set_monitor(&id);
                            save();
                        }
                    }
                    Some(Hit::Enable) => {
                        let on = !ENABLED.load(Relaxed);
                        ENABLED.store(on, Relaxed);
                        if !on {
                            crate::set_target(1.0);
                        }
                        save();
                    }
                    Some(Hit::Autostart) => {
                        let on = UI.with(|u| u.borrow().as_ref().map(|ui| !ui.autostart).unwrap_or(false));
                        tray::set_autostart(on);
                        UI.with(|u| {
                            if let Some(ui) = u.borrow_mut().as_mut() {
                                ui.autostart = tray::autostart_enabled();
                            }
                        });
                    }
                    Some(Hit::Chip) | Some(Hit::Record) => start_listening(hwnd),
                    Some(Hit::Clear) => {
                        set_combo(&[]);
                        start_listening(hwnd);
                    }
                    None => {}
                }
                repaint(hwnd);
                LRESULT(0)
            }
            WM_REC => {
                if wparam.0 == 1 {
                    let keys = rec().keys.clone();
                    // A bare wheel would hijack all scrolling, so a real key is required.
                    if keys.iter().any(|k| !is_wheel(*k)) {
                        REJECTED.store(false, Relaxed);
                        set_combo(&keys);
                        save();
                    } else {
                        REJECTED.store(true, Relaxed);
                        set_combo(&[K_CONTROL]);
                        save();
                    }
                    unhook();
                }
                repaint(hwnd);
                LRESULT(0)
            }
            WM_CLOSE => {
                stop_listening();
                if combo().is_empty() {
                    set_combo(&[K_CONTROL]); // never leave the app without a shortcut
                    save();
                }
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                WIN.store(0, Relaxed);
                UI.with(|u| *u.borrow_mut() = None); // release Direct2D resources
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
