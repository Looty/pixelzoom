//! The app icon, drawn procedurally. Pure Rust (no Windows types) so build.rs can
//! reuse it to generate the .ico embedded in the exe.

/// Colour of the icon at (u, v) in 0..1: a blue rounded square with a white
/// magnifier. Returns straight RGBA (alpha 0 outside the square).
fn icon_sample(u: f32, v: f32) -> [f32; 4] {
    let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
    // Rounded-square mask.
    let (px, py) = ((u - 0.5).abs(), (v - 0.5).abs());
    let (half, rad) = (0.47f32, 0.21f32);
    let (qx, qy) = ((px - (half - rad)).max(0.0), (py - (half - rad)).max(0.0));
    if (qx * qx + qy * qy).sqrt() - rad > 0.0 {
        return [0.0; 4];
    }
    // Top-to-bottom blue gradient.
    let mut c = [lerp(0.38, 0.12, v), lerp(0.70, 0.40, v), lerp(1.0, 0.92, v)];
    let mut mix = |w: f32, a: f32| {
        for ch in c.iter_mut() {
            *ch = lerp(*ch, w, a);
        }
    };
    // Lens: ring plus a faint glass fill, with a "+" inside.
    let dl = ((u - 0.43).powi(2) + (v - 0.43).powi(2)).sqrt();
    let (lx, ly) = ((u - 0.43).abs(), (v - 0.43).abs());
    let plus = (lx <= 0.085 && ly <= 0.026) || (ly <= 0.085 && lx <= 0.026);
    if dl < 0.165 {
        mix(1.0, if plus { 1.0 } else { 0.18 });
    } else if dl <= 0.255 {
        mix(1.0, 1.0);
    }
    // Handle: a capsule from (0.60, 0.60) to (0.80, 0.80).
    let (ax, ay, bx, by) = (0.60f32, 0.60f32, 0.80f32, 0.80f32);
    let (dx, dy) = (bx - ax, by - ay);
    let t = (((u - ax) * dx + (v - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    let dh = ((u - (ax + t * dx)).powi(2) + (v - (ay + t * dy)).powi(2)).sqrt();
    if dh <= 0.068 {
        mix(1.0, 1.0);
    }
    [c[0], c[1], c[2], 1.0]
}

/// Anti-aliased (4x4 supersampled) icon of `size` px, as premultiplied BGRA rows, top-down.
pub fn icon_pixels(size: i32) -> Vec<u8> {
    const SS: i32 = 4;
    let mut out = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let (mut r, mut g, mut b, mut a) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let u = (x as f32 + (sx as f32 + 0.5) / SS as f32) / size as f32;
                    let v = (y as f32 + (sy as f32 + 0.5) / SS as f32) / size as f32;
                    let s = icon_sample(u, v);
                    r += s[0] * s[3];
                    g += s[1] * s[3];
                    b += s[2] * s[3];
                    a += s[3];
                }
            }
            let n = (SS * SS) as f32;
            let i = ((y * size + x) * 4) as usize;
            out[i] = (b / n * 255.0 + 0.5) as u8;
            out[i + 1] = (g / n * 255.0 + 0.5) as u8;
            out[i + 2] = (r / n * 255.0 + 0.5) as u8;
            out[i + 3] = (a / n * 255.0 + 0.5) as u8;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(buf: &[u8], size: i32, x: i32, y: i32) -> [u8; 4] {
        let i = ((y * size + x) * 4) as usize;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
    }

    #[test]
    fn buffer_has_the_requested_size() {
        for s in [16, 24, 32, 48, 64, 256] {
            assert_eq!(icon_pixels(s).len(), (s * s * 4) as usize);
        }
    }

    #[test]
    fn corners_are_transparent_and_centre_is_opaque() {
        let s = 64;
        let buf = icon_pixels(s);
        assert_eq!(px(&buf, s, 0, 0)[3], 0);
        assert_eq!(px(&buf, s, s - 1, s - 1)[3], 0);
        assert_eq!(px(&buf, s, s / 2, s / 2)[3], 255);
    }

    #[test]
    fn pixels_are_premultiplied() {
        let buf = icon_pixels(32);
        for p in buf.chunks_exact(4) {
            assert!(p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3], "{p:?}");
        }
    }

    #[test]
    fn handle_sits_bottom_right_and_lens_top_left() {
        let s = 128;
        let buf = icon_pixels(s);
        // The handle (white) is at about (0.7, 0.7); the lens ring is white on its left edge.
        let handle = px(&buf, s, (0.70 * s as f32) as i32, (0.70 * s as f32) as i32);
        assert!(handle[0] > 240 && handle[1] > 240 && handle[2] > 240, "{handle:?}");
        let ring = px(&buf, s, (0.43 * s as f32) as i32 - (0.21 * s as f32) as i32, (0.43 * s as f32) as i32);
        assert!(ring[0] > 240 && ring[1] > 240 && ring[2] > 240, "{ring:?}");
        // Bottom-left should be plain blue background (not white).
        let bg = px(&buf, s, (0.15 * s as f32) as i32, (0.85 * s as f32) as i32);
        assert!(bg[2] < 200, "{bg:?}"); // BGRA: index 2 is red
    }
}
