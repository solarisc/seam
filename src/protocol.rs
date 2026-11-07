use rdev::Key;
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 9;
pub const DEFAULT_PORT: u16 = 47843;
pub const MAX_WIRE_SIZE: usize = 65_535;
pub const FILE_CHUNK_SIZE: usize = 60 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Monitor {
    pub id: u32,
    /// Persistent OS identity; `id` is only valid in the current desktop session.
    #[serde(default)]
    pub identity: Option<String>,
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

impl Monitor {
    pub fn right(&self) -> f64 {
        self.x as f64 + self.width as f64
    }
    pub fn bottom(&self) -> f64 {
        self.y as f64 + self.height as f64
    }
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x as f64 && x < self.right() && y >= self.y as f64 && y < self.bottom()
    }
}

pub fn monitors() -> anyhow::Result<Vec<Monitor>> {
    #[cfg(target_os = "macos")]
    let identities = crate::mac_displays::identities()?;
    Ok(display_info::DisplayInfo::all()?
        .into_iter()
        .map(|d| Monitor {
            id: d.id,
            #[cfg(target_os = "macos")]
            identity: identities.get(&d.id).cloned(),
            #[cfg(target_os = "windows")]
            identity: crate::windows_displays::identity(&d.name),
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            identity: None,
            name: if d.friendly_name.is_empty() {
                d.name
            } else {
                d.friendly_name
            },
            x: d.x,
            y: d.y,
            width: d.width,
            height: d.height,
            primary: d.is_primary,
        })
        .collect())
}

/// Geometry refresh is safe off the UI thread. On macOS, avoid NSScreen (used
/// by display-info for friendly names); CoreGraphics uses the same coordinate
/// space as native pointer events. Keep known names without querying AppKit.
pub fn monitor_geometry(known: &[Monitor]) -> anyhow::Result<Vec<Monitor>> {
    #[cfg(target_os = "macos")]
    {
        #[repr(C)]
        struct Point {
            x: f64,
            y: f64,
        }
        #[repr(C)]
        struct Size {
            width: f64,
            height: f64,
        }
        #[repr(C)]
        struct Rect {
            origin: Point,
            size: Size,
        }
        #[link(name = "CoreGraphics", kind = "framework")]
        unsafe extern "C" {
            fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
            fn CGDisplayBounds(id: u32) -> Rect;
            fn CGDisplayIsMain(id: u32) -> u32;
        }
        let mut count = 0;
        anyhow::ensure!(
            unsafe { CGGetActiveDisplayList(0, std::ptr::null_mut(), &mut count) } == 0,
            "Cannot count active displays"
        );
        // The inventory can grow between the two calls; the next refresh picks
        // up any additional display. The API writes at most the given capacity.
        let mut displays = vec![0; count as usize];
        anyhow::ensure!(
            unsafe { CGGetActiveDisplayList(count, displays.as_mut_ptr(), &mut count) } == 0,
            "Cannot read active displays"
        );
        displays.truncate(count as usize);
        let identities = crate::mac_displays::identities()?;
        Ok(displays
            .into_iter()
            .map(|id| {
                let bounds = unsafe { CGDisplayBounds(id) };
                Monitor {
                    id,
                    identity: identities.get(&id).cloned(),
                    name: known
                        .iter()
                        .find(|m| m.id == id)
                        .map(|m| m.name.clone())
                        .unwrap_or_else(|| format!("Display {id}")),
                    x: bounds.origin.x as i32,
                    y: bounds.origin.y as i32,
                    width: bounds.size.width as u32,
                    height: bounds.size.height as u32,
                    primary: unsafe { CGDisplayIsMain(id) } != 0,
                }
            })
            .collect())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = known;
        monitors()
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ScrollUnit {
    /// Conventional mouse-wheel notches.
    Lines,
    /// Trackpad pixel movement.
    Pixels,
}

/// Physical button identities, independent of native OS button numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
    /// One-based HID button ordinal beyond the five named buttons.
    Auxiliary(u8),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Input {
    Pointer {
        x: f64,
        y: f64,
    },
    Button {
        button: MouseButton,
        down: bool,
    },
    Wheel {
        dx: i64,
        dy: i64,
        unit: ScrollUnit,
    },
    Key {
        key: Key,
        down: bool,
    },
    ReleaseAll,
    Ping(u64),
    Pong(u64),
    Hello {
        version: u16,
        name: String,
        os: String,
        monitors: Vec<Monitor>,
        /// The sender controls several computers and owns the keyboard and mouse.
        hub: bool,
    },
    Layout(crate::layout::Control),
    /// Normal edge return: release input and hide the destination cursor.
    Leave,
    // Appended, so builds of this version that never send `PointerLock` are
    // never sent `Motion` and still decode everything they receive.
    /// Raw mouse motion for an app that has taken the destination's mouse.
    Motion {
        dx: i32,
        dy: i32,
    },
    /// From the destination: an app there took the mouse, as games do for
    /// mouse-look, or let go. While it holds, motion is raw and no edge
    /// returns. On release, `x` and `y` are the cursor position to resume from.
    PointerLock {
        locked: bool,
        x: f64,
        y: f64,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Data {
    Text(String),
    TextStart {
        id: u64,
        bytes: u64,
    },
    TextChunk {
        id: u64,
        bytes: Vec<u8>,
    },
    TextEnd {
        id: u64,
    },
    ImageStart {
        id: u64,
        bytes: u64,
    },
    ImageChunk {
        id: u64,
        bytes: Vec<u8>,
    },
    ImageEnd {
        id: u64,
    },
    BatchStart {
        id: u64,
        count: u32,
        bytes: u64,
    },
    Directory {
        id: u64,
        path: String,
    },
    FileStart {
        id: u64,
        path: String,
        bytes: u64,
    },
    FileChunk {
        id: u64,
        bytes: Vec<u8>,
    },
    FileEnd {
        id: u64,
    },
    BatchEnd {
        id: u64,
    },
    TransferReceipt {
        id: u64,
    },
    /// Text copied together with the image being sent, such as the cells
    /// behind a spreadsheet selection's picture. Sent after `ImageStart`.
    ImageTextChunk {
        id: u64,
        bytes: Vec<u8>,
    },
    /// The sender stopped this batch. The receiver discards what arrived.
    BatchAbort {
        id: u64,
    },
    /// The receiver could not take this transfer, for the reason given in
    /// words for the sender's transfer list. The sender stops sending it.
    TransferFailed {
        id: u64,
        reason: String,
    },
}
