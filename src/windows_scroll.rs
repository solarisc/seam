//! Preserve high-resolution trackpad scrolling when injecting on Windows.
//! rdev's wheel API multiplies every input by a full 120-unit wheel notch,
//! which makes macOS pixel deltas scroll far too quickly.

use crate::protocol::ScrollUnit;
use anyhow::Result;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{MOUSEEVENTF_HWHEEL, MOUSEEVENTF_WHEEL};

const WHEEL_UNITS_PER_LINE: i64 = 120;
const WHEEL_UNITS_PER_PIXEL: i64 = 3;

fn wheel_units(delta: i64, unit: ScrollUnit) -> i32 {
    let scale = match unit {
        ScrollUnit::Lines => WHEEL_UNITS_PER_LINE,
        ScrollUnit::Pixels => WHEEL_UNITS_PER_PIXEL,
    };
    delta
        .saturating_mul(scale)
        .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

fn send_axis(delta: i32, horizontal: bool) -> Result<()> {
    if delta == 0 {
        return Ok(());
    }
    crate::windows_input::mouse(
        if horizontal {
            MOUSEEVENTF_HWHEEL
        } else {
            MOUSEEVENTF_WHEEL
        },
        delta as u32,
        0,
        0,
    )
}

pub fn simulate_peer_scroll(dx: i64, dy: i64, unit: ScrollUnit) -> Result<()> {
    send_axis(wheel_units(dx, unit), true)?;
    send_axis(wheel_units(dy, unit), false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_trackpad_pixels_to_fractional_wheel_notches() {
        assert_eq!(wheel_units(16, ScrollUnit::Pixels), 48);
        assert_eq!(wheel_units(-16, ScrollUnit::Pixels), -48);
        assert_eq!(wheel_units(1, ScrollUnit::Lines), 120);
    }
}
