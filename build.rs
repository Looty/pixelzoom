//! Embeds the app icon and version info (shown in Task Manager) into the exe.
//! The icon is generated from the same drawing code the app uses at runtime.

#[path = "src/icon.rs"]
mod icon;

use std::{env, fs, path::PathBuf};

/// Build a multi-size .ico (32-bit BMP entries, straight alpha, bottom-up rows).
fn make_ico() -> Vec<u8> {
    const SIZES: [i32; 6] = [16, 24, 32, 48, 64, 256];
    let images: Vec<Vec<u8>> = SIZES
        .iter()
        .map(|&s| {
            let premul = icon::icon_pixels(s);
            let mut img = Vec::new();
            img.extend_from_slice(&40u32.to_le_bytes()); // BITMAPINFOHEADER
            img.extend_from_slice(&s.to_le_bytes());
            img.extend_from_slice(&(s * 2).to_le_bytes()); // XOR + AND heights
            img.extend_from_slice(&1u16.to_le_bytes());
            img.extend_from_slice(&32u16.to_le_bytes());
            img.extend_from_slice(&[0u8; 24]);
            for y in (0..s).rev() {
                for x in 0..s {
                    let i = ((y * s + x) * 4) as usize;
                    let a = premul[i + 3] as f32 / 255.0;
                    for c in 0..3 {
                        let v = if a > 0.0 { premul[i + c] as f32 / a } else { 0.0 };
                        img.push(v.round().min(255.0) as u8);
                    }
                    img.push(premul[i + 3]);
                }
            }
            let mask_row = ((s + 31) / 32 * 4) as usize;
            img.extend(std::iter::repeat_n(0u8, mask_row * s as usize));
            img
        })
        .collect();

    let mut ico = Vec::new();
    ico.extend_from_slice(&[0, 0, 1, 0]);
    ico.extend_from_slice(&(SIZES.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * SIZES.len() as u32;
    for (s, img) in SIZES.iter().zip(&images) {
        let b = if *s >= 256 { 0 } else { *s as u8 };
        ico.extend_from_slice(&[b, b, 0, 0]);
        ico.extend_from_slice(&1u16.to_le_bytes());
        ico.extend_from_slice(&32u16.to_le_bytes());
        ico.extend_from_slice(&(img.len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += img.len() as u32;
    }
    for img in &images {
        ico.extend_from_slice(img);
    }
    ico
}

fn main() {
    println!("cargo:rerun-if-changed=src/icon.rs");
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let ico_path = out.join("pixelzoom.ico");
    fs::write(&ico_path, make_ico()).unwrap();

    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let nums = version.replace('.', ",");
    let rc = format!(
        r#"1 ICON "{ico}"
1 VERSIONINFO
FILEVERSION {nums},0
PRODUCTVERSION {nums},0
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "FileDescription", "Pixel Zoom"
      VALUE "ProductName", "Pixel Zoom"
      VALUE "InternalName", "pixelzoom"
      VALUE "OriginalFilename", "pixelzoom.exe"
      VALUE "FileVersion", "{version}"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        ico = ico_path.display().to_string().replace('\\', "/"),
    );
    let rc_path = out.join("pixelzoom.rc");
    fs::write(&rc_path, rc).unwrap();
    embed_resource::compile(&rc_path, embed_resource::NONE).manifest_optional().unwrap();
}
