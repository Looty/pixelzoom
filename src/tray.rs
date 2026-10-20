//! Tray icon, context menu and start-with-Windows registration.

use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

pub const WM_TRAY: u32 = WM_APP + 1;
const ICON_ID: u32 = 1;
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

// Menu command ids.
const ID_SETTINGS: usize = 1;
const ID_QUIT: usize = 99;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---- start with Windows -------------------------------------------------

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("pixelzoom");

pub fn autostart_enabled() -> bool {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, KEY_READ, &mut key).is_err() {
            return false;
        }
        let found = RegQueryValueExW(key, RUN_VALUE, None, None, None, None).is_ok();
        let _ = RegCloseKey(key);
        found
    }
}

pub fn set_autostart(on: bool) {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, KEY_READ | KEY_WRITE, &mut key).is_err() {
            return;
        }
        if on {
            if let Ok(exe) = std::env::current_exe() {
                let value = wide(&format!("\"{}\"", exe.display()));
                let bytes = std::slice::from_raw_parts(value.as_ptr() as *const u8, value.len() * 2);
                let _ = RegSetValueExW(key, RUN_VALUE, None, REG_SZ, Some(bytes));
            }
        } else {
            let _ = RegDeleteValueW(key, RUN_VALUE);
        }
        let _ = RegCloseKey(key);
    }
}

/// Keep the registered path current if the exe was moved or rebuilt elsewhere.
pub fn refresh_autostart() {
    if autostart_enabled() {
        set_autostart(true);
    }
}

// ---- icon ---------------------------------------------------------------

/// App icon at the requested pixel size (tray, title bar, taskbar).
pub fn make_icon(size: i32) -> HICON {
    let xor = crate::icon::icon_pixels(size);
    let and = vec![0u8; (size * size / 8).max(1) as usize];
    unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None).map(Into::into).unwrap_or_default();
        CreateIcon(Some(hinst), size, size, 1, 32, and.as_ptr(), xor.as_ptr())
            .or_else(|_| LoadIconW(None, IDI_APPLICATION))
            .unwrap_or_default()
    }
}

fn notify_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW { cbSize: size_of::<NOTIFYICONDATAW>() as u32, hWnd: hwnd, uID: ICON_ID, ..Default::default() }
}

pub fn add_icon(hwnd: HWND) {
    unsafe {
        let mut nid = notify_data(hwnd);
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = make_icon(GetSystemMetrics(SM_CXSMICON));
        for (i, c) in "Pixel Zoom".encode_utf16().enumerate() {
            nid.szTip[i] = c;
        }
        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
    }
}

pub fn remove_icon(hwnd: HWND) {
    unsafe {
        let nid = notify_data(hwnd);
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

// ---- window + menu ------------------------------------------------------

/// Hidden window that receives tray and TaskbarCreated messages.
pub fn create_window() -> windows::core::Result<HWND> {
    unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None)?.into();
        let class = w!("pixelzoom_tray");
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(tray_proc),
            hInstance: hinst,
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Relaxed);
        CreateWindowExW(WS_EX_TOOLWINDOW, class, w!("pixelzoom tray"), WS_POPUP, 0, 0, 0, 0, None, None, Some(hinst), None)
    }
}

fn item(menu: HMENU, id: usize, text: &str, checked: bool) {
    let flags = MF_STRING | if checked { MF_CHECKED } else { MF_UNCHECKED };
    unsafe {
        let _ = AppendMenuW(menu, flags, id, PCWSTR(wide(text).as_ptr()));
    }
}

fn show_menu(hwnd: HWND) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };

        item(menu, ID_SETTINGS, "Settings", false);
        item(menu, ID_QUIT, "Exit", false);

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // Required so the menu closes when the user clicks elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, Some(0), hwnd, None);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);

        handle_command(hwnd, cmd.0 as usize);
    }
}

fn handle_command(_hwnd: HWND, id: usize) {
    match id {
        ID_SETTINGS => crate::ui::open(),
        ID_QUIT => unsafe { PostQuitMessage(0) },
        _ => {}
    }
}

unsafe extern "system" fn tray_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_TRAY {
            match lparam.0 as u32 & 0xFFFF {
                WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
                WM_LBUTTONDBLCLK => handle_command(hwnd, ID_SETTINGS),
                _ => {}
            }
            return LRESULT(0);
        }
        // Resolution / HDR / monitor changes and resume from sleep invalidate GPU state.
        const PBT_APMRESUMESUSPEND: usize = 0x7;
        const PBT_APMRESUMEAUTOMATIC: usize = 0x12;
        if msg == WM_DISPLAYCHANGE || (msg == WM_POWERBROADCAST && (wparam.0 == PBT_APMRESUMESUSPEND || wparam.0 == PBT_APMRESUMEAUTOMATIC))
        {
            crate::DISPLAY_DIRTY.store(true, Relaxed);
        }
        let tc = TASKBAR_CREATED.load(Relaxed);
        if tc != 0 && msg == tc {
            // Explorer restarted: put the icon back.
            add_icon(hwnd);
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}
