//! Runtime settings (lock-free, shared with the mouse hook) and the config file.

use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};

// Fixed look (no UI): filter 3 = lanczos3, sharpen 4 = cleartype+adaptive.
pub const K_SHIFT: u32 = 0x10;
pub const K_CONTROL: u32 = 0x11;
pub const K_ALT: u32 = 0x12; // Alt
pub const K_WIN: u32 = 0x5B;

/// Pseudo key codes for the wheel direction that zooms in (real VK codes are < 0x100).
pub const K_WHEEL_UP: u32 = 0x1_0001;
pub const K_WHEEL_DOWN: u32 = 0x1_0002;

pub const DEFAULT_STRENGTH: f32 = 0.8;
pub const MAX_COMBO_KEYS: usize = 2;

pub static FILTER: AtomicU32 = AtomicU32::new(3);
pub static SHARPEN: AtomicU32 = AtomicU32::new(4);
pub static STRENGTH: AtomicU32 = AtomicU32::new(DEFAULT_STRENGTH.to_bits());
/// Keys that must be held while scrolling (0 = empty slot). Generic VK codes only.
pub static COMBO: [AtomicU32; MAX_COMBO_KEYS] = [AtomicU32::new(K_CONTROL), AtomicU32::new(0)];
pub static ZOOM_STEP: AtomicU32 = AtomicU32::new(1.12f32.to_bits());
pub static MAX_ZOOM: AtomicU32 = AtomicU32::new(16.0f32.to_bits());
pub static ENABLED: AtomicBool = AtomicBool::new(true);
/// Zoom levels stick to whole ratios (2x, 3x…) where the shader draws exact blocks.
pub static SNAP_ZOOM: AtomicBool = AtomicBool::new(true);
/// Panel sub-pixels run B,G,R left to right instead of the usual R,G,B.
pub static BGR_PANEL: AtomicBool = AtomicBool::new(false);

pub fn getf(a: &AtomicU32) -> f32 {
    f32::from_bits(a.load(Relaxed))
}

/// Collapse left/right variants to the generic code (Win keeps K_WIN).
pub fn normalize_vk(vk: u32) -> u32 {
    match vk {
        0xA0 | 0xA1 => K_SHIFT,
        0xA2 | 0xA3 => K_CONTROL,
        0xA4 | 0xA5 => K_ALT,
        0x5C => K_WIN,
        v => v,
    }
}

pub fn combo() -> Vec<u32> {
    COMBO.iter().map(|a| a.load(Relaxed)).filter(|v| *v != 0).collect()
}

pub fn is_wheel(k: u32) -> bool {
    k == K_WHEEL_UP || k == K_WHEEL_DOWN
}

/// True when the shortcut says scroll *down* zooms in.
pub fn wheel_inverted() -> bool {
    is_inverted(&combo())
}

pub fn set_combo(keys: &[u32]) {
    for (i, slot) in COMBO.iter().enumerate() {
        slot.store(keys.get(i).copied().unwrap_or(0), Relaxed);
    }
}

/// Human-readable key name, also used in the config file.
pub fn vk_label(vk: u32) -> String {
    match vk {
        K_WHEEL_UP => "Scroll ↑".into(),
        K_WHEEL_DOWN => "Scroll ↓".into(),
        K_CONTROL => "Ctrl".into(),
        K_ALT => "Alt".into(),
        K_SHIFT => "Shift".into(),
        K_WIN => "Win".into(),
        0x30..=0x39 | 0x41..=0x5A => (vk as u8 as char).to_string(),
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        0x20 => "Space".into(),
        0x09 => "Tab".into(),
        0x1B => "Esc".into(),
        0x0D => "Enter".into(),
        v => format!("0x{v:02X}"),
    }
}

fn parse_vk(s: &str) -> Option<u32> {
    let s = s.trim();
    [K_WHEEL_UP, K_WHEEL_DOWN].into_iter().chain((1..=0xFE).map(normalize_vk)).find(|vk| vk_label(*vk).eq_ignore_ascii_case(s))
}

fn parse_combo(s: &str) -> Vec<u32> {
    s.split('+').filter_map(parse_vk).take(MAX_COMBO_KEYS).collect()
}

pub fn config_path() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    base.join("pixelzoom").join("config.ini")
}

/// Apply the saved settings; unknown or invalid lines are ignored.
/// Values found in a config file (each `None` when absent or invalid).
#[derive(Default, Debug, PartialEq)]
pub struct Parsed {
    pub shortcut: Option<Vec<u32>>,
    pub enabled: Option<bool>,
    pub monitor: Option<String>,
    pub snap_zoom: Option<bool>,
    pub bgr_panel: Option<bool>,
}

pub fn parse_config(text: &str) -> Parsed {
    let mut p = Parsed::default();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim());
        match k {
            "shortcut" => {
                let keys = parse_combo(v);
                if !keys.is_empty() {
                    p.shortcut = Some(keys);
                }
            }
            "enabled" => p.enabled = Some(v != "0"),
            "monitor" => p.monitor = Some(v.to_string()),
            "snap_zoom" => p.snap_zoom = Some(v != "0"),
            "bgr_panel" => p.bgr_panel = Some(v != "0"),
            _ => {}
        }
    }
    p
}

pub fn format_config(shortcut: &[u32], enabled: bool, monitor: &str, snap_zoom: bool, bgr_panel: bool) -> String {
    let shortcut = shortcut.iter().map(|v| vk_label(*v)).collect::<Vec<_>>().join("+");
    format!(
        "shortcut={shortcut}\nenabled={}\nmonitor={monitor}\nsnap_zoom={}\nbgr_panel={}\n",
        enabled as u8, snap_zoom as u8, bgr_panel as u8
    )
}

/// True when this key list says scroll *down* zooms in.
pub fn is_inverted(keys: &[u32]) -> bool {
    keys.contains(&K_WHEEL_DOWN)
}

/// Selected monitor as its Windows device name (e.g. `\\.\DISPLAY2`); empty = primary.
pub static MONITOR: Mutex<String> = Mutex::new(String::new());
/// Bumped whenever the selected monitor changes, so the render side can rebuild.
pub static MONITOR_GEN: AtomicU32 = AtomicU32::new(0);

pub fn monitor_id() -> String {
    MONITOR.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub fn set_monitor(id: &str) {
    *MONITOR.lock().unwrap_or_else(|e| e.into_inner()) = id.to_string();
    MONITOR_GEN.fetch_add(1, Relaxed);
}

pub fn load() {
    // Dev switches for A/B-ing the look without rebuilding. Debug builds only,
    // so a released exe has no hidden knobs.
    #[cfg(debug_assertions)]
    for (var, slot) in [("PIXELZOOM_FILTER", &FILTER), ("PIXELZOOM_SMODE", &SHARPEN)] {
        if let Some(v) = std::env::var(var).ok().and_then(|v| v.parse::<u32>().ok()) {
            slot.store(v, Relaxed);
        }
    }
    let Ok(text) = fs::read_to_string(config_path()) else { return };
    let p = parse_config(&text);
    if let Some(keys) = p.shortcut {
        set_combo(&keys);
    }
    if let Some(on) = p.enabled {
        ENABLED.store(on, Relaxed);
    }
    if let Some(m) = p.monitor {
        set_monitor(&m);
    }
    if let Some(on) = p.snap_zoom {
        SNAP_ZOOM.store(on, Relaxed);
    }
    if let Some(on) = p.bgr_panel {
        BGR_PANEL.store(on, Relaxed);
    }
}

pub fn save() {
    let path = config_path();
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let text = format_config(&combo(), ENABLED.load(Relaxed), &monitor_id(), SNAP_ZOOM.load(Relaxed), BGR_PANEL.load(Relaxed));
    let _ = fs::write(path, text);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_collapses_left_right_variants() {
        assert_eq!(normalize_vk(0xA0), K_SHIFT);
        assert_eq!(normalize_vk(0xA1), K_SHIFT);
        assert_eq!(normalize_vk(0xA2), K_CONTROL);
        assert_eq!(normalize_vk(0xA3), K_CONTROL);
        assert_eq!(normalize_vk(0xA4), K_ALT);
        assert_eq!(normalize_vk(0xA5), K_ALT);
        assert_eq!(normalize_vk(0x5C), K_WIN);
        assert_eq!(normalize_vk(0x41), 0x41);
    }

    #[test]
    fn labels_are_readable() {
        assert_eq!(vk_label(K_CONTROL), "Ctrl");
        assert_eq!(vk_label(K_ALT), "Alt");
        assert_eq!(vk_label(K_SHIFT), "Shift");
        assert_eq!(vk_label(K_WIN), "Win");
        assert_eq!(vk_label(0x41), "A");
        assert_eq!(vk_label(0x35), "5");
        assert_eq!(vk_label(0x70), "F1");
        assert_eq!(vk_label(0x7B), "F12");
        assert_eq!(vk_label(0x20), "Space");
        assert_eq!(vk_label(K_WHEEL_UP), "Scroll ↑");
        assert_eq!(vk_label(K_WHEEL_DOWN), "Scroll ↓");
    }

    #[test]
    fn every_label_parses_back_to_the_same_key() {
        for vk in [K_CONTROL, K_ALT, K_SHIFT, K_WIN, 0x41, 0x5A, 0x30, 0x39, 0x70, 0x7B, 0x20, 0x1B, K_WHEEL_UP, K_WHEEL_DOWN] {
            assert_eq!(parse_vk(&vk_label(vk)), Some(vk), "label {:?}", vk_label(vk));
        }
    }

    #[test]
    fn parsing_is_case_insensitive_and_rejects_nonsense() {
        assert_eq!(parse_vk("ctrl"), Some(K_CONTROL));
        assert_eq!(parse_vk("  SHIFT "), Some(K_SHIFT));
        assert_eq!(parse_vk("scroll ↓"), Some(K_WHEEL_DOWN));
        assert_eq!(parse_vk("banana"), None);
        assert_eq!(parse_vk(""), None);
    }

    #[test]
    fn combo_parsing_caps_length_and_skips_junk() {
        assert_eq!(parse_combo("Ctrl+Scroll ↑"), vec![K_CONTROL, K_WHEEL_UP]);
        assert_eq!(parse_combo("Ctrl+Alt+Shift"), vec![K_CONTROL, K_ALT]); // MAX_COMBO_KEYS = 2
        assert_eq!(parse_combo("Ctrl+nonsense+Alt"), vec![K_CONTROL, K_ALT]);
        assert!(parse_combo("nothing valid").is_empty());
    }

    #[test]
    fn wheel_pseudo_keys_are_not_real_vk_codes() {
        assert!(is_wheel(K_WHEEL_UP) && is_wheel(K_WHEEL_DOWN));
        assert!(!is_wheel(K_CONTROL));
        const { assert!(K_WHEEL_UP > 0xFF && K_WHEEL_DOWN > 0xFF) };
    }

    #[test]
    fn inversion_means_scroll_down_zooms_in() {
        assert!(is_inverted(&[K_CONTROL, K_WHEEL_DOWN]));
        assert!(!is_inverted(&[K_CONTROL, K_WHEEL_UP]));
        assert!(!is_inverted(&[K_CONTROL]));
    }

    #[test]
    fn config_round_trips() {
        let text = format_config(&[K_CONTROL, K_WHEEL_DOWN], false, r"\\.\DISPLAY2", false, true);
        let p = parse_config(&text);
        assert_eq!(p.shortcut, Some(vec![K_CONTROL, K_WHEEL_DOWN]));
        assert_eq!(p.enabled, Some(false));
        assert_eq!(p.monitor.as_deref(), Some(r"\\.\DISPLAY2"));
        assert_eq!(p.snap_zoom, Some(false));
        assert_eq!(p.bgr_panel, Some(true));
    }

    #[test]
    fn old_config_without_the_new_keys_keeps_defaults() {
        let p = parse_config("shortcut=Ctrl+Scroll ↑\nenabled=1\n");
        assert_eq!((p.snap_zoom, p.bgr_panel), (None, None));
    }

    #[test]
    fn config_ignores_comments_blank_lines_and_unknown_keys() {
        let p = parse_config("# comment\n; also comment\n\nfoo=bar\nenabled=1\nno equals sign\n");
        assert_eq!(p, Parsed { shortcut: None, enabled: Some(true), ..Default::default() });
    }

    #[test]
    fn invalid_shortcut_keeps_the_default() {
        assert_eq!(parse_config("shortcut=banana+nonsense\n").shortcut, None);
        assert_eq!(parse_config("shortcut=\n").shortcut, None);
    }

    #[test]
    fn empty_monitor_means_primary() {
        let p = parse_config(&format_config(&[K_CONTROL], true, "", true, false));
        assert_eq!(p.monitor.as_deref(), Some(""));
    }

    #[test]
    fn enabled_flag_parses_zero_as_off() {
        assert_eq!(parse_config("enabled=0").enabled, Some(false));
        assert_eq!(parse_config("enabled=1").enabled, Some(true));
    }
}
