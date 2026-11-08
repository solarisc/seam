//! Match physical displays across Windows sessions. HMONITOR is only a runtime handle.
use std::collections::BTreeMap;
use windows_sys::Win32::{
    Foundation::{LPARAM, RECT},
    Graphics::Gdi::{
        DISPLAY_DEVICE_ACTIVE, DISPLAY_DEVICEW, EnumDisplayDevicesW, EnumDisplayMonitors,
        GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
    },
};

/// One display as `layout` sees it: its rectangle, flags (primary) and
/// adapter name.
type Placement = ([i32; 4], u32, [u16; 32]);

/// What a display being added, removed, moved, resized or made primary
/// changes. A few user32 calls, where a full read (`protocol::monitors`)
/// asks the display driver about every display: about 1.7 ms for two, and on
/// a laptop it can wake the second graphics card. `None` if Windows refuses.
pub(crate) fn layout() -> Option<Vec<Placement>> {
    unsafe extern "system" fn add(
        monitor: HMONITOR,
        _: HDC,
        _: *mut RECT,
        found: LPARAM,
    ) -> windows_sys::core::BOOL {
        unsafe { &mut *(found as *mut Vec<HMONITOR>) }.push(monitor);
        1
    }
    let mut monitors: Vec<HMONITOR> = Vec::new();
    let listed = unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(add),
            &mut monitors as *mut Vec<HMONITOR> as LPARAM,
        )
    };
    if listed == 0 {
        return None;
    }
    monitors
        .into_iter()
        .map(|monitor| {
            let mut info: MONITORINFOEXW = unsafe { std::mem::zeroed() };
            info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
            let read = unsafe {
                GetMonitorInfoW(
                    monitor,
                    &mut info as *mut MONITORINFOEXW as *mut MONITORINFO,
                )
            };
            (read != 0).then(|| {
                let r = info.monitorInfo.rcMonitor;
                (
                    [r.left, r.top, r.right, r.bottom],
                    info.monitorInfo.dwFlags,
                    info.szDevice,
                )
            })
        })
        .collect()
}

pub(crate) fn identity(device_name: &str) -> Option<String> {
    let name: Vec<u16> = device_name.encode_utf16().chain(Some(0)).collect();
    let mut identities = Vec::new();
    for index in 0..64 {
        let mut device = DISPLAY_DEVICEW {
            cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        // EDD_GET_DEVICE_INTERFACE_NAME returns the monitor device interface,
        // not the session-dependent adapter name (such as \\.\DISPLAY1).
        if unsafe { EnumDisplayDevicesW(name.as_ptr(), index, &mut device, 1) } == 0 {
            break;
        }
        if device.StateFlags & DISPLAY_DEVICE_ACTIVE == 0 {
            continue;
        }
        let end = device
            .DeviceID
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(device.DeviceID.len());
        let value = String::from_utf16_lossy(&device.DeviceID[..end]);
        if !value.is_empty() {
            identities.push(format!("windows:{}", value.to_ascii_lowercase()));
        }
    }
    // Mirrored/ambiguous targets must not silently select an arbitrary display.
    (identities.len() == 1).then(|| identities.remove(0))
}

pub(crate) fn identities() -> anyhow::Result<BTreeMap<u32, String>> {
    Ok(display_info::DisplayInfo::all()?
        .into_iter()
        .filter_map(|display| identity(&display.name).map(|identity| (display.id, identity)))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_layout_lists_every_display() {
        // The computer running the tests has at least one display.
        let layout = layout().expect("layout");
        assert!(!layout.is_empty());
        assert_eq!(layout.len(), crate::protocol::monitors().unwrap().len());
    }
}
