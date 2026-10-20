//! Zoom stepping: how far one wheel notch (or keyboard step) moves the zoom
//! target, including the magnet that stops on whole ratios.

use std::sync::atomic::Ordering;

use crate::settings::*;

/// New zoom target after `notches` wheel notches (or keyboard steps), using the
/// configured step size, maximum and magnet setting.
pub(crate) fn step_zoom(t: f32, notches: f32) -> f32 {
    let (step, max) = (getf(&ZOOM_STEP), getf(&MAX_ZOOM));
    if SNAP_ZOOM.load(Ordering::Relaxed) { step_zoom_with(t, notches, step, max) } else { step_zoom_raw(t, notches, step, max) }
}

fn step_zoom_with(t: f32, notches: f32, step: f32, max: f32) -> f32 {
    let z = step_zoom_raw(t, notches, step, max);
    if z == 1.0 { z } else { snap_to_whole(t, z, notches, max) }
}

/// The same step without the whole-ratio magnet.
fn step_zoom_raw(t: f32, notches: f32, step: f32, max: f32) -> f32 {
    let z = (t * step.powf(notches)).clamp(1.0, max);
    if z < 1.02 { 1.0 } else { z }
}

/// Magnet: land exactly on whole zoom ratios, where the shader can map source
/// pixels to whole blocks. A single notch that crosses a whole ratio stops on
/// it, and any step that ends within 4% of one while approaching it snaps to
/// it. A step that moves away from where it stands never snaps, so even tiny
/// touchpad steps leave a whole ratio again.
fn snap_to_whole(t: f32, z: f32, notches: f32, max: f32) -> f32 {
    let valid = |w: f32| (2.0..=max).contains(&w);
    // Several notches in one event are a deliberate flick: only the landing counts.
    // Crossing is checked first so the nearest ratio in the travel direction wins.
    if notches.abs() <= 1.5 {
        let crossed = if z > t { t.floor() + 1.0 } else { t.ceil() - 1.0 };
        if valid(crossed) && (crossed - t) * (crossed - z) <= 0.0 {
            return crossed;
        }
    }
    let near = z.round();
    if valid(near) && (z - near).abs() < (t - near).abs() && (z - near).abs() / near < 0.04 {
        return near;
    }
    z
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_steps_multiply_per_notch() {
        let z = step_zoom_with(1.0, 1.0, 1.12, 16.0);
        assert!((z - 1.12).abs() < 1e-6);
        let z = step_zoom_with(2.0, 2.0, 1.12, 16.0);
        assert!((z - 2.0 * 1.12 * 1.12).abs() < 1e-5);
    }

    #[test]
    fn zoom_is_clamped_to_the_maximum() {
        assert_eq!(step_zoom_with(15.9, 5.0, 1.12, 16.0), 16.0);
        assert_eq!(step_zoom_with(4.0, 50.0, 1.12, 4.0), 4.0);
    }

    #[test]
    fn zooming_out_snaps_back_to_exactly_one() {
        assert_eq!(step_zoom_with(1.05, -1.0, 1.12, 16.0), 1.0);
        assert_eq!(step_zoom_with(1.0, -3.0, 1.12, 16.0), 1.0);
        // Just above the snap threshold stays zoomed.
        assert!(step_zoom_with(1.2, -1.0, 1.12, 16.0) > 1.02);
    }

    #[test]
    fn zoom_sticks_to_whole_ratios_when_approaching() {
        assert_eq!(step_zoom_with(1.8, 1.0, 1.12, 16.0), 2.0); // 2.016 is within the magnet
        assert_eq!(step_zoom_with(4.3, -0.5, 1.12, 16.0), 4.0); // 4.06 approaches 4 from above
        assert_eq!(step_zoom_with(2.2, -1.0, 1.12, 16.0), 2.0); // 1.964 approaches 2 from above
        assert!((step_zoom_with(1.0, 1.0, 1.12, 16.0) - 1.12).abs() < 1e-6); // 1x is not a magnet
    }

    #[test]
    fn every_whole_ratio_is_a_stop_scrolling_in_and_out() {
        let mut z = 1.0;
        let mut stops = Vec::new();
        for _ in 0..200 {
            z = step_zoom_with(z, 1.0, 1.12, 16.0);
            if z.fract() == 0.0 {
                stops.push(z as u32);
            }
        }
        assert_eq!(stops.first(), Some(&2));
        stops.dedup();
        assert_eq!(stops, (2..=16).collect::<Vec<_>>(), "scrolling in");

        let mut stops = Vec::new();
        for _ in 0..200 {
            z = step_zoom_with(z, -1.0, 1.12, 16.0);
            if z.fract() == 0.0 && z >= 2.0 {
                stops.push(z as u32);
            }
        }
        stops.dedup();
        assert_eq!(stops, (2..=15).rev().collect::<Vec<_>>(), "scrolling out");
    }

    #[test]
    fn a_big_flick_is_not_stopped_by_the_first_whole_ratio() {
        assert!(step_zoom_with(1.0, 12.0, 1.12, 16.0) > 3.5);
        assert!(step_zoom_with(8.0, -12.0, 1.12, 16.0) < 2.5);
    }

    #[test]
    fn crossing_a_whole_ratio_in_one_notch_stops_on_it() {
        assert_eq!(step_zoom_with(2.8, 1.0, 1.12, 16.0), 3.0); // would be 3.136
        assert_eq!(step_zoom_with(3.2, -1.0, 1.12, 16.0), 3.0); // would be 2.857
    }

    #[test]
    fn zoom_leaves_a_whole_ratio_even_with_tiny_steps() {
        let mut z = 2.0;
        for _ in 0..40 {
            z = step_zoom_with(z, 0.05, 1.12, 16.0);
        }
        assert!(z > 2.1, "stuck at {z}");
        let mut z = 2.0;
        for _ in 0..40 {
            z = step_zoom_with(z, -0.05, 1.12, 16.0);
        }
        assert!(z < 1.9, "stuck at {z}");
    }

    #[test]
    fn raw_steps_never_snap_to_whole_ratios() {
        // 1.76 * 1.12 = 1.97, which the magnet would pull to 2.0.
        assert_eq!(step_zoom_with(1.76, 1.0, 1.12, 16.0), 2.0);
        let z = step_zoom_raw(1.76, 1.0, 1.12, 16.0);
        assert!((z - 1.76 * 1.12).abs() < 1e-5);
        assert_eq!(step_zoom_raw(1.05, -1.0, 1.12, 16.0), 1.0); // still snaps back to 1x
        assert_eq!(step_zoom_raw(15.9, 5.0, 1.12, 16.0), 16.0);
    }

    #[test]
    fn fractional_notches_from_touchpads_work() {
        let z = step_zoom_with(1.0, 0.25, 1.12, 16.0);
        assert!(z > 1.0 && z < 1.12);
    }
}
