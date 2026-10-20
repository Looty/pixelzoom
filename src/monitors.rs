//! Connected monitors: device name, model name, position and primary flag.

use std::ffi::c_void;

use windows::Win32::Devices::Display::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::core::BOOL;

const MONITORINFOF_PRIMARY: u32 = 1;

#[derive(Clone, Debug)]
pub struct Monitor {
    /// Windows device name, e.g. `\\.\DISPLAY2`. This is what the config stores.
    pub id: String,
    /// Model name from the monitor's EDID (e.g. "LG ULTRAGEAR"), or "Display N".
    pub name: String,
    /// Position and size on the virtual desktop, in physical pixels.
    pub rect: RECT,
    pub primary: bool,
}

impl Monitor {
    pub fn width(&self) -> i32 {
        self.rect.right - self.rect.left
    }
    pub fn height(&self) -> i32 {
        self.rect.bottom - self.rect.top
    }
}

/// UTF-16 buffer up to the first NUL.
pub fn utf16_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Label shown when Windows gives no model name.
pub fn fallback_name(index: usize) -> String {
    format!("Display {}", index + 1)
}

/// Left-to-right order, so the picker matches how monitors sit on the desk.
fn sort_monitors(v: &mut [Monitor]) {
    v.sort_by_key(|m| (m.rect.left, m.rect.top));
}

pub fn list() -> Vec<Monitor> {
    unsafe extern "system" fn collect(hmon: HMONITOR, _: HDC, _: *mut RECT, lparam: LPARAM) -> BOOL {
        unsafe {
            let v = &mut *(lparam.0 as *mut Vec<Monitor>);
            let mut mi = MONITORINFOEXW::default();
            mi.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
            if GetMonitorInfoW(hmon, &mut mi as *mut MONITORINFOEXW as *mut MONITORINFO).as_bool() {
                v.push(Monitor {
                    id: utf16_to_string(&mi.szDevice),
                    name: String::new(),
                    rect: mi.monitorInfo.rcMonitor,
                    primary: mi.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
                });
            }
            TRUE
        }
    }

    let mut out: Vec<Monitor> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(collect), LPARAM(&mut out as *mut Vec<Monitor> as isize));
    }
    sort_monitors(&mut out);
    let names = friendly_names();
    for (i, m) in out.iter_mut().enumerate() {
        m.name = names
            .iter()
            .find(|(gdi, _)| *gdi == m.id)
            .map(|(_, n)| n.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| fallback_name(i));
    }
    out
}

/// (GDI device name, monitor model name) for every active display path.
fn friendly_names() -> Vec<(String, String)> {
    let mut result = Vec::new();
    unsafe {
        let (mut np, mut nm) = (0u32, 0u32);
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm) != WIN32_ERROR(0) {
            return result;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        if QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None) != WIN32_ERROR(0) {
            return result;
        }
        for p in paths.iter().take(np as usize) {
            let mut src = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            src.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            src.header.size = size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
            src.header.adapterId = p.sourceInfo.adapterId;
            src.header.id = p.sourceInfo.id;
            let mut tgt = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
            tgt.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
            tgt.header.size = size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
            tgt.header.adapterId = p.targetInfo.adapterId;
            tgt.header.id = p.targetInfo.id;
            if DisplayConfigGetDeviceInfo(&mut src.header) == 0 && DisplayConfigGetDeviceInfo(&mut tgt.header) == 0 {
                result.push((utf16_to_string(&src.viewGdiDeviceName), utf16_to_string(&tgt.monitorFriendlyDeviceName)));
            }
        }
    }
    result
}

/// Grab a small BGRA thumbnail of a monitor (top-down, `w` x `h`) for the picker.
/// Uses GDI, so HDR screens look slightly flat, which is fine for a preview.
pub fn thumbnail(m: &Monitor, w: i32, h: i32) -> Option<Vec<u8>> {
    unsafe {
        let screen = GetDC(None);
        if screen.is_invalid() {
            return None;
        }
        let mem = CreateCompatibleDC(Some(screen));
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let bmp = CreateDIBSection(Some(mem), &bmi, DIB_RGB_COLORS, &mut bits, None, 0);
        let result = match bmp {
            Ok(bmp) if !bits.is_null() => {
                let old = SelectObject(mem, bmp.into());
                let _ = SetStretchBltMode(mem, HALFTONE);
                let _ = SetBrushOrgEx(mem, 0, 0, None);
                let ok = StretchBlt(mem, 0, 0, w, h, Some(screen), m.rect.left, m.rect.top, m.width(), m.height(), SRCCOPY).as_bool();
                let px = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize).to_vec();
                SelectObject(mem, old);
                let _ = DeleteObject(bmp.into());
                ok.then_some(px)
            }
            _ => None,
        };
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        result.map(|mut px| {
            for a in px.as_chunks_mut::<4>().0 {
                a[3] = 255; // GDI leaves alpha undefined
            }
            px
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(id: &str, left: i32) -> Monitor {
        Monitor { id: id.into(), name: String::new(), rect: RECT { left, top: 0, right: left + 100, bottom: 50 }, primary: false }
    }

    #[test]
    fn utf16_stops_at_nul() {
        let buf: Vec<u16> = "DISPLAY1\0garbage".encode_utf16().collect();
        assert_eq!(utf16_to_string(&buf), "DISPLAY1");
        assert_eq!(utf16_to_string(&[]), "");
    }

    #[test]
    fn fallback_names_are_one_based() {
        assert_eq!(fallback_name(0), "Display 1");
        assert_eq!(fallback_name(2), "Display 3");
    }

    #[test]
    fn monitors_sort_left_to_right() {
        let mut v = vec![mon("b", 1920), mon("a", -1920), mon("c", 0)];
        sort_monitors(&mut v);
        let ids: Vec<_> = v.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["a", "c", "b"]);
    }

    #[test]
    fn size_helpers() {
        let m = mon("x", 10);
        assert_eq!((m.width(), m.height()), (100, 50));
    }

    #[test]
    fn list_finds_at_least_one_monitor() {
        // Runs on a desktop session; skip silently on headless CI.
        let v = list();
        if !v.is_empty() {
            assert!(v.iter().any(|m| m.primary));
            assert!(v.iter().all(|m| !m.name.is_empty()));
        }
    }
}
