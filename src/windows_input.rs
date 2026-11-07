//! Tagged native injection. Event identity survives coordinate rounding and
//! keyboard layout translation, so capture cannot mistake peer input for HID.
use crate::protocol::MouseButton;
use anyhow::{Result, bail};
use rdev::EventType;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;

/// Marks Seam's own input for its hooks. Apps read the low 7 bits of a mouse
/// message's extra info as the pen or touch contact that produced it (0 is
/// the mouse), so they stay 0: with them set, Firefox took Seam's pointer for
/// another one and its pointer-locked games jumped each time it re-centered.
pub(crate) const INJECTED_INPUT_TAG: usize = 0x5441_4e00;

#[link(name = "user32")]
unsafe extern "system" {
    fn GetSystemMetrics(index: i32) -> i32;
    fn OpenInputDesktop(flags: u32, inherit: i32, access: u32) -> isize;
    fn SetThreadDesktop(desktop: isize) -> i32;
    fn CloseDesktop(desktop: isize) -> i32;
}

const ERROR_ACCESS_DENIED: i32 = 5;
const GENERIC_ALL: u32 = 0x1000_0000;

/// The input desktop a thread attached itself to, closed on the next switch
/// or when the thread ends.
struct AttachedDesktop(std::cell::Cell<isize>);

impl Drop for AttachedDesktop {
    fn drop(&mut self) {
        let desktop = self.0.get();
        if desktop != 0 {
            unsafe { CloseDesktop(desktop) };
        }
    }
}

thread_local! {
    static ATTACHED_DESKTOP: AttachedDesktop = const { AttachedDesktop(std::cell::Cell::new(0)) };
}

/// Attach this thread to the desktop receiving input. Windows rejects input
/// injected from any other desktop, which is what happens to the sign-in
/// helper when the lock screen and the secure sign-in desktop trade places.
fn attach_to_input_desktop() -> bool {
    let desktop = unsafe { OpenInputDesktop(0, 0, GENERIC_ALL) };
    if desktop == 0 {
        return false;
    }
    if unsafe { SetThreadDesktop(desktop) } == 0 {
        unsafe { CloseDesktop(desktop) };
        return false;
    }
    let previous = ATTACHED_DESKTOP.with(|attached| attached.0.replace(desktop));
    if previous != 0 {
        unsafe { CloseDesktop(previous) };
    }
    true
}

fn send(input: INPUT) -> Result<()> {
    let size = std::mem::size_of::<INPUT>() as i32;
    if unsafe { SendInput(1, &input, size) } == 1 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_ACCESS_DENIED)
        && attach_to_input_desktop()
        && unsafe { SendInput(1, &input, size) } == 1
    {
        return Ok(());
    }
    bail!("Windows rejected injected input: {error}");
}

pub(crate) fn mouse(flags: u32, data: u32, dx: i32, dy: i32) -> Result<()> {
    send(mouse_input(flags, data, dx, dy))
}

fn mouse_input(flags: u32, data: u32, dx: i32, dy: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                dwExtraInfo: INJECTED_INPUT_TAG,
                ..MOUSEINPUT::default()
            },
        },
    }
}

/// An absolute move to a desktop pixel. Windows merges moves an app has not
/// read yet, as it does for a mouse plugged in. Delivered one by one
/// (`MOUSEEVENTF_MOVE_NOCOALESCE`), up to a thousand a second, a window being
/// resized redrew for every position: Explorer trailed the pointer by 200 px
/// and kept resizing for 4 s after it stopped.
fn pointer_input(x: f64, y: f64, desktop: (i32, i32, i32, i32)) -> Result<INPUT> {
    let (left, top, width, height) = desktop;
    Ok(mouse_input(
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        0,
        absolute_axis(x, left, width)?,
        absolute_axis(y, top, height)?,
    ))
}

/// The center of the destination pixel in Windows' 16-bit absolute space.
/// Subtract the virtual desktop origin, which can be negative.
fn absolute_axis(position: f64, origin: i32, extent: i32) -> Result<i32> {
    if !position.is_finite() || extent <= 0 {
        bail!("invalid desktop coordinate");
    }
    let pixel = (position.round() - f64::from(origin)).clamp(0.0, f64::from(extent - 1));
    Ok(((pixel + 0.5) * 65536.0 / f64::from(extent))
        .floor()
        .clamp(0.0, 65535.0) as i32)
}

pub(crate) fn simulate(event: &EventType) -> Result<()> {
    match *event {
        EventType::KeyPress(key) | EventType::KeyRelease(key) => keyboard(
            key,
            matches!(event, EventType::KeyPress(_)),
            INJECTED_INPUT_TAG,
        ),
        EventType::MouseMove { x, y } => {
            let desktop = unsafe {
                (
                    GetSystemMetrics(76),
                    GetSystemMetrics(77),
                    GetSystemMetrics(78),
                    GetSystemMetrics(79),
                )
            };
            send(pointer_input(x, y, desktop)?)
        }
        EventType::ButtonPress(_) | EventType::ButtonRelease(_) => {
            bail!("native buttons require a portable mouse-button identity")
        }
        EventType::Wheel { delta_x, delta_y } => crate::windows_scroll::simulate_peer_scroll(
            delta_x,
            delta_y,
            crate::protocol::ScrollUnit::Lines,
        ),
    }
}

/// Relative motion, as a physical mouse reports it. Games reading raw input
/// receive exactly these counts; the cursor follows Windows' pointer speed.
pub(crate) fn simulate_motion(dx: i32, dy: i32) -> Result<()> {
    mouse(MOUSEEVENTF_MOVE | MOUSEEVENTF_MOVE_NOCOALESCE, 0, dx, dy)
}

fn keyboard(key: rdev::Key, down: bool, tag: usize) -> Result<()> {
    let (vk, extended) = crate::windows_capture::key_to_windows(key)
        .ok_or_else(|| anyhow::anyhow!("unsupported Windows key: {key:?}"))?;
    let flags =
        if down { 0 } else { KEYEVENTF_KEYUP } | if extended { KEYEVENTF_EXTENDEDKEY } else { 0 };
    send(INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC) } as u16,
                dwFlags: flags,
                dwExtraInfo: tag,
                ..KEYBDINPUT::default()
            },
        },
    })
}

/// Presses and releases a key no keyboard has (0xE8 is unassigned). Between
/// a modifier's press and release, it keeps Windows from treating them as a
/// lone Alt (which opens the menu bar), a lone Windows key (the Start menu)
/// or Alt+Shift (the keyboard layout switch).
pub(crate) fn mask_modifiers() -> Result<()> {
    for down in [true, false] {
        send(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: 0xe8,
                    dwFlags: if down { 0 } else { KEYEVENTF_KEYUP },
                    dwExtraInfo: INJECTED_INPUT_TAG,
                    ..KEYBDINPUT::default()
                },
            },
        })?;
    }
    Ok(())
}

pub(crate) fn simulate_button(button: MouseButton, down: bool) -> Result<()> {
    send(button_input(button, down)?)
}

fn button_input(button: MouseButton, down: bool) -> Result<INPUT> {
    let (flags, data) = match button {
        MouseButton::Left => (
            if down {
                MOUSEEVENTF_LEFTDOWN
            } else {
                MOUSEEVENTF_LEFTUP
            },
            0,
        ),
        MouseButton::Right => (
            if down {
                MOUSEEVENTF_RIGHTDOWN
            } else {
                MOUSEEVENTF_RIGHTUP
            },
            0,
        ),
        MouseButton::Middle => (
            if down {
                MOUSEEVENTF_MIDDLEDOWN
            } else {
                MOUSEEVENTF_MIDDLEUP
            },
            0,
        ),
        MouseButton::Back => (
            if down {
                MOUSEEVENTF_XDOWN
            } else {
                MOUSEEVENTF_XUP
            },
            1,
        ),
        MouseButton::Forward => (
            if down {
                MOUSEEVENTF_XDOWN
            } else {
                MOUSEEVENTF_XUP
            },
            2,
        ),
        MouseButton::Auxiliary(_) => bail!("Windows supports only five mouse buttons"),
    };
    Ok(INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                mouseData: data,
                dwFlags: flags,
                dwExtraInfo: INJECTED_INPUT_TAG,
                ..MOUSEINPUT::default()
            },
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injected_input_says_it_comes_from_the_mouse() {
        // The pen and touch signature, and the contact ID apps read.
        assert_ne!(INJECTED_INPUT_TAG & 0xFFFF_FF00, 0xFF51_5700);
        assert_eq!(INJECTED_INPUT_TAG & 0x7F, 0);
    }

    #[test]
    fn portable_buttons_encode_the_sendinput_contract() {
        // Independent documented MOUSEEVENTF_* values and X-button data.
        for (button, down_flags, up_flags, data) in [
            (MouseButton::Left, 0x02, 0x04, 0),
            (MouseButton::Right, 0x08, 0x10, 0),
            (MouseButton::Middle, 0x20, 0x40, 0),
            (MouseButton::Back, 0x80, 0x100, 1),
            (MouseButton::Forward, 0x80, 0x100, 2),
        ] {
            for (down, flags) in [(true, down_flags), (false, up_flags)] {
                let input = button_input(button, down).unwrap();
                assert_eq!(input.r#type, 0);
                let mouse = unsafe { input.Anonymous.mi };
                assert_eq!((mouse.dwFlags, mouse.mouseData), (flags, data));
                assert_eq!(mouse.dwExtraInfo, INJECTED_INPUT_TAG);
                assert_eq!((mouse.dx, mouse.dy), (0, 0));
            }
        }
        for ordinal in [0, 1, 5, 6, 32, 255] {
            assert!(button_input(MouseButton::Auxiliary(ordinal), true).is_err());
        }
    }

    #[test]
    fn pointer_moves_merge_like_a_plugged_in_mouse() {
        // Independent documented values: MOVE, VIRTUALDESK and ABSOLUTE, and
        // not MOVE_NOCOALESCE (0x2000).
        let input = pointer_input(-1920.0, 1079.0, (-1920, 0, 3840, 1080)).unwrap();
        let mouse = unsafe { input.Anonymous.mi };
        assert_eq!(mouse.dwFlags, 0x0001 | 0x4000 | 0x8000);
        assert_eq!((mouse.dx, mouse.dy), (8, 65505));
        assert_eq!(mouse.dwExtraInfo, INJECTED_INPUT_TAG);
        assert!(pointer_input(0.0, 0.0, (0, 0, 0, 0)).is_err());
    }

    #[test]
    fn pointer_round_trips_at_every_pixel_with_negative_desktop_origin() {
        for (origin, extent) in [(0, 1920), (-1920, 3840), (-3840, 7680)] {
            for pixel in origin..origin + extent {
                let normalized = absolute_axis(f64::from(pixel), origin, extent).unwrap();
                let actual = i64::from(normalized) * i64::from(extent) / 65536 + i64::from(origin);
                assert_eq!(actual, i64::from(pixel));
            }
        }
        assert!(absolute_axis(f64::NAN, 0, 1920).is_err());
        assert!(absolute_axis(0.0, 0, 0).is_err());
    }
}
