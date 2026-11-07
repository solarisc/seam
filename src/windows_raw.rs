//! Receive physical movement for local edge crossing when the desktop cursor
//! is already clamped at its boundary. During remote control, the low-level
//! hook supplies accelerated motion relative to the stationary source cursor.
//! Keyboard reports only count, to notice hooks Windows has removed.

use crate::input::Controller;
use anyhow::{Result, bail};
use std::{
    ffi::c_void,
    mem, ptr,
    sync::atomic::{AtomicPtr, Ordering},
};

const WM_INPUT: u32 = 0x00ff;
const RID_INPUT: u32 = 0x1000_0003;
const RIM_TYPEMOUSE: u32 = 0;
const RIM_TYPEKEYBOARD: u32 = 1;
const RIDEV_REMOVE: u32 = 0x0000_0001;
const RIDEV_INPUTSINK: u32 = 0x0000_0100;
const RIDEV_DEVNOTIFY: u32 = 0x0000_2000;
const MOUSE_MOVE_ABSOLUTE: u16 = 1;

#[repr(C)]
struct RawInputDevice {
    usage_page: u16,
    usage: u16,
    flags: u32,
    target: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RawInputHeader {
    kind: u32,
    size: u32,
    device: *mut c_void,
    wparam: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RawMouse {
    flags: u16,
    _padding: u16,
    buttons: u32,
    raw_buttons: u32,
    last_x: i32,
    last_y: i32,
    extra: u32,
}

#[repr(C)]
struct Point {
    x: i32,
    y: i32,
}

#[link(name = "user32")]
unsafe extern "system" {
    fn CreateWindowExW(
        ex_style: u32,
        class_name: *const u16,
        title: *const u16,
        style: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        parent: *mut c_void,
        menu: *mut c_void,
        instance: *mut c_void,
        param: *mut c_void,
    ) -> *mut c_void;
    fn DestroyWindow(window: *mut c_void) -> i32;
    fn RegisterRawInputDevices(devices: *const RawInputDevice, count: u32, size: u32) -> i32;
    fn GetRawInputData(
        handle: *mut c_void,
        command: u32,
        data: *mut c_void,
        size: *mut u32,
        header_size: u32,
    ) -> u32;
    fn GetCursorPos(point: *mut Point) -> i32;
}

pub fn cursor_position() -> Option<(f64, f64)> {
    let mut point = Point { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut point) } == 0 {
        return None;
    }
    Some((f64::from(point.x), f64::from(point.y)))
}

/// Which device a raw input report came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Report {
    Mouse,
    Keyboard,
}

/// A report's device and, for the mouse, its relative motion unless Seam
/// injected it or it is absolute.
fn read_report(data: isize) -> Option<(Report, Option<(f64, f64)>)> {
    let mut buffer = [0u8; 128];
    let mut size = u32::try_from(buffer.len()).ok()?;
    let read = unsafe {
        GetRawInputData(
            data as *mut c_void,
            RID_INPUT,
            buffer.as_mut_ptr().cast(),
            &mut size,
            mem::size_of::<RawInputHeader>() as u32,
        )
    };
    if read == u32::MAX || (read as usize) < mem::size_of::<RawInputHeader>() {
        return None;
    }
    let header = unsafe { ptr::read_unaligned(buffer.as_ptr().cast::<RawInputHeader>()) };
    match header.kind {
        RIM_TYPEKEYBOARD => Some((Report::Keyboard, None)),
        RIM_TYPEMOUSE
            if read as usize >= mem::size_of::<RawInputHeader>() + mem::size_of::<RawMouse>() =>
        {
            let mouse = unsafe {
                ptr::read_unaligned(
                    buffer
                        .as_ptr()
                        .add(mem::size_of::<RawInputHeader>())
                        .cast::<RawMouse>(),
                )
            };
            let motion = (mouse.extra != crate::windows_input::INJECTED_INPUT_TAG as u32
                && mouse.flags & MOUSE_MOVE_ABSOLUTE == 0)
                .then(|| (f64::from(mouse.last_x), f64::from(mouse.last_y)));
            Some((Report::Mouse, motion))
        }
        _ => None,
    }
}

/// Raw mouse input feeds edge crossing; both mouse and keyboard reports let
/// the capture loop notice when Windows has removed its hooks.
fn devices(window: *mut c_void, remove: bool) -> [RawInputDevice; 2] {
    [
        device(2, RIDEV_INPUTSINK | RIDEV_DEVNOTIFY, window, remove),
        keyboard(window, remove),
    ]
}

fn keyboard(window: *mut c_void, remove: bool) -> RawInputDevice {
    device(6, RIDEV_INPUTSINK, window, remove)
}

fn device(usage: u16, flags: u32, window: *mut c_void, remove: bool) -> RawInputDevice {
    RawInputDevice {
        usage_page: 1,
        usage,
        flags: if remove { RIDEV_REMOVE } else { flags },
        // Removing a subscription requires a null target.
        target: if remove { ptr::null_mut() } else { window },
    }
}

fn register(devices: &[RawInputDevice]) -> std::io::Result<()> {
    if unsafe {
        RegisterRawInputDevices(
            devices.as_ptr(),
            devices.len() as u32,
            mem::size_of::<RawInputDevice>() as u32,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// The capture window, while it receives raw keyboard reports.
static KEYBOARD_TARGET: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

/// Stop or resume raw keyboard reports, from any thread. While a window of
/// this process has the foreground, Windows stops calling its low-level
/// keyboard hook if the process also reads the keyboard raw: keys would then
/// reach neither computer, the shortcuts back included.
pub(crate) fn keyboard_reports(enabled: bool) -> Result<()> {
    let window = KEYBOARD_TARGET.load(Ordering::Acquire);
    if window.is_null() {
        return Ok(());
    }
    if let Err(error) = register(&[keyboard(window, !enabled)]) {
        bail!(
            "cannot {} raw keyboard input: {error}",
            if enabled { "resume" } else { "pause" }
        );
    }
    Ok(())
}

/// Owns the raw-input window on the same thread as the low-level hooks. Both
/// event sources use one blocking message loop, avoiding a second wakeup and
/// cross-thread controller contention for each physical mouse report.
pub(crate) struct Capture {
    window: *mut c_void,
}

impl Capture {
    pub(crate) fn new() -> Result<Self> {
        let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
        let window = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                ptr::null(),
                0,
                0,
                0,
                0,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if window.is_null() {
            bail!(
                "cannot create raw-input window: {}",
                std::io::Error::last_os_error()
            );
        }
        if let Err(error) = register(&devices(window, false)) {
            unsafe { DestroyWindow(window) };
            bail!("cannot register Windows raw input: {error}");
        }
        KEYBOARD_TARGET.store(window, Ordering::Release);
        Ok(Self { window })
    }

    /// Handles a raw input message and says which device it came from.
    pub(crate) fn handle_message(
        &self,
        kind: u32,
        data: isize,
        controller: &Controller,
    ) -> Option<Report> {
        // During remote control the hook owns accelerated motion, unless an
        // app on the destination has taken the mouse and needs raw counts.
        // The caller still dispatches WM_INPUT so Windows cleans up the payload.
        if kind != WM_INPUT {
            return None;
        }
        let (report, motion) = read_report(data)?;
        if let Some((dx, dy)) = motion {
            if controller.remote_active.load(Ordering::Acquire) {
                controller.handle_windows_raw_remote(dx, dy);
            } else {
                controller.handle_windows_raw_motion(dx, dy);
            }
        }
        Some(report)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        KEYBOARD_TARGET.store(ptr::null_mut(), Ordering::Release);
        if let Err(error) = register(&devices(self.window, true)) {
            log::error!("cannot unregister raw input: {error}");
        }
        unsafe { DestroyWindow(self.window) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetRegisteredRawInputDevices(
            devices: *mut RawInputDevice,
            count: *mut u32,
            size: u32,
        ) -> u32;
    }

    fn keyboard_registered() -> bool {
        let mut devices: [RawInputDevice; 8] = unsafe { mem::zeroed() };
        let mut count = devices.len() as u32;
        let read = unsafe {
            GetRegisteredRawInputDevices(
                devices.as_mut_ptr(),
                &mut count,
                mem::size_of::<RawInputDevice>() as u32,
            )
        };
        assert_ne!(read, u32::MAX);
        devices[..read as usize]
            .iter()
            .any(|device| device.usage_page == 1 && device.usage == 6)
    }

    #[test]
    fn keyboard_reports_pause_and_resume_from_another_thread() {
        let capture = Capture::new().unwrap();
        assert!(keyboard_registered());
        // The cursor thread pauses them for its foreground lease.
        std::thread::spawn(|| keyboard_reports(false))
            .join()
            .unwrap()
            .unwrap();
        assert!(!keyboard_registered());
        std::thread::spawn(|| keyboard_reports(true))
            .join()
            .unwrap()
            .unwrap();
        assert!(keyboard_registered());
        drop(capture);
        assert!(!keyboard_registered());
        // Without the capture window there is nothing to pause or resume.
        keyboard_reports(true).unwrap();
        assert!(!keyboard_registered());
    }
}
