use rdev::Key;
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 10;
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
        let displays = crate::mac_displays::active()?;
        let identities = crate::mac_displays::identities()?;
        Ok(displays
            .into_iter()
            .map(|display| Monitor {
                id: display.id,
                identity: identities.get(&display.id).cloned(),
                name: known
                    .iter()
                    .find(|m| m.id == display.id)
                    .map(|m| m.name.clone())
                    .unwrap_or_else(|| format!("Display {}", display.id)),
                x: display.x,
                y: display.y,
                width: display.width,
                height: display.height,
                primary: display.main,
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

/// A file or folder among offered files.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct OfferedEntry {
    /// Its path within the copy, folders separated by `/`.
    pub path: String,
    /// The file's size; `None` for a folder.
    pub bytes: Option<u64>,
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
    /// Files were copied on the sender, listed in walk order: each folder
    /// before what it holds. A long list takes several messages, the last
    /// marked `last`. None of the files is sent until a paste on the
    /// receiver asks for them.
    FilesOffer {
        id: u64,
        entries: Vec<OfferedEntry>,
        last: bool,
    },
    /// A paste on the receiver wants offered files `id`. The sender answers
    /// with a batch of the same `id`, or `BatchAbort` when it can't.
    FilesWanted {
        id: u64,
    },
}
