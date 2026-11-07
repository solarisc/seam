//! Classify only the small interactive stream. Bulk transfers keep the OS
//! default, and classification performs no per-event work or traffic shaping.
use std::{io, net::TcpStream};

/// Keeps a native input classification alive until the input session ends.
#[must_use = "keep the classification alive for the input session"]
pub struct InputTraffic {
    #[cfg(target_os = "windows")]
    handle: windows_sys::Win32::Foundation::HANDLE,
}

impl InputTraffic {
    pub(crate) fn new(stream: &TcpStream) -> io::Result<Self> {
        #[cfg(target_os = "windows")]
        {
            use std::{os::windows::io::AsRawSocket, ptr};
            use windows_sys::Win32::NetworkManagement::QoS::{
                QOS_NON_ADAPTIVE_FLOW, QOS_VERSION, QOSAddSocketToFlow, QOSCreateHandle,
                QOSTrafficTypeControl,
            };
            let version = QOS_VERSION {
                MajorVersion: 1,
                MinorVersion: 0,
            };
            let mut handle = ptr::null_mut();
            if unsafe { QOSCreateHandle(&version, &mut handle) } == 0 {
                return Err(io::Error::last_os_error());
            }
            // Construct the owner immediately so a rejected flow also closes
            // its handle. Closing it removes every associated flow.
            let traffic = Self { handle };
            let mut flow = 0;
            if unsafe {
                QOSAddSocketToFlow(
                    handle,
                    stream.as_raw_socket() as usize,
                    ptr::null(),
                    QOSTrafficTypeControl,
                    QOS_NON_ADAPTIVE_FLOW,
                    &mut flow,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(traffic)
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = stream;
            Ok(Self {})
        }
    }
}

#[cfg(target_os = "windows")]
impl Drop for InputTraffic {
    fn drop(&mut self) {
        if unsafe { windows_sys::Win32::NetworkManagement::QoS::QOSCloseHandle(self.handle) } == 0 {
            log::warn!(
                "cannot close input traffic classification: {}",
                io::Error::last_os_error()
            );
        }
    }
}
