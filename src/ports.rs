//! Explains why Seam cannot open one of its network ports, naming the
//! program that holds it where the system can tell.
use std::io;

#[derive(Clone, Copy)]
pub enum Protocol {
    Tcp,
    Udp,
}

/// A message for the person, not a log line: what is wrong and what to do.
pub fn unavailable(port: u16, protocol: Protocol, error: &io::Error) -> String {
    let in_use = matches!(
        error.kind(),
        io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied
    );
    match (
        in_use.then(|| owner(port, protocol)).flatten(),
        error.kind(),
    ) {
        (Some(name), _) if is_seam(&name) => format!(
            "Seam is already using network port {port}, possibly for another person signed in to this computer. Only one person at a time can share."
        ),
        (Some(name), _) => format!(
            "Network port {port} is in use by {name}. Close that program (or uninstall it if it is another keyboard-sharing app), then turn sharing on again."
        ),
        (None, io::ErrorKind::AddrInUse) => format!(
            "Network port {port} is in use by another program. Close it, then turn sharing on again."
        ),
        // With no program holding it, Windows has reserved the port, as it
        // does for ranges used by Hyper-V, WSL and Docker.
        (None, io::ErrorKind::PermissionDenied) => format!(
            "Windows has reserved network port {port} for another feature (often Hyper-V, WSL or Docker). Restarting the computer usually releases it."
        ),
        _ => format!("Cannot open network port {port}: {error}"),
    }
}

fn is_seam(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.starts_with("seam-agent") || name == "seam.exe"
}

#[cfg(windows)]
fn owner(port: u16, protocol: Protocol) -> Option<String> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, GetExtendedUdpTable, MIB_TCPROW_OWNER_PID, MIB_UDPROW_OWNER_PID,
        TCP_TABLE_OWNER_PID_LISTENER, UDP_TABLE_OWNER_PID,
    };
    const AF_INET: u32 = 2;

    // Both tables start with a u32 row count followed by the rows; the size
    // can grow between the two calls, so ask until it fits.
    let mut buffer = Vec::<u32>::new();
    let mut size = 0u32;
    for _ in 0..4 {
        let result = unsafe {
            match protocol {
                Protocol::Tcp => GetExtendedTcpTable(
                    buffer.as_mut_ptr().cast(),
                    &mut size,
                    0,
                    AF_INET,
                    TCP_TABLE_OWNER_PID_LISTENER,
                    0,
                ),
                Protocol::Udp => GetExtendedUdpTable(
                    buffer.as_mut_ptr().cast(),
                    &mut size,
                    0,
                    AF_INET,
                    UDP_TABLE_OWNER_PID,
                    0,
                ),
            }
        };
        if result == 0 {
            break;
        }
        buffer = vec![0; (size as usize).div_ceil(4)];
    }
    let count = *buffer.first()? as usize;
    let rows = buffer.as_ptr().wrapping_add(1).cast::<u8>();
    let pid = (0..count).find_map(|index| unsafe {
        // Ports are stored in network byte order in the low 16 bits.
        let (local_port, pid) = match protocol {
            Protocol::Tcp => {
                let row = rows
                    .cast::<MIB_TCPROW_OWNER_PID>()
                    .add(index)
                    .read_unaligned();
                (row.dwLocalPort, row.dwOwningPid)
            }
            Protocol::Udp => {
                let row = rows
                    .cast::<MIB_UDPROW_OWNER_PID>()
                    .add(index)
                    .read_unaligned();
                (row.dwLocalPort, row.dwOwningPid)
            }
        };
        (u16::from_be(local_port as u16) == port).then_some(pid)
    })?;
    Some(process_name(pid).unwrap_or_else(|| format!("another program (process {pid})")))
}

/// The executable file name of process `pid`, such as `seam.exe`.
#[cfg(windows)]
pub(crate) fn process_name(pid: u32) -> Option<String> {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{
            OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        },
    };
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return None;
    }
    let mut path = [0u16; 1024];
    let mut length = path.len() as u32;
    let found = unsafe {
        QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, path.as_mut_ptr(), &mut length)
    } != 0;
    unsafe { CloseHandle(process) };
    if !found {
        return None;
    }
    let path = String::from_utf16_lossy(&path[..length as usize]);
    Some(path.rsplit('\\').next().unwrap_or(&path).to_string())
}

#[cfg(not(windows))]
fn owner(_port: u16, _protocol: Protocol) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_the_program_holding_a_port() {
        let listener = std::net::TcpListener::bind(("0.0.0.0", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let error = std::net::TcpListener::bind(("0.0.0.0", port)).unwrap_err();
        let message = unavailable(port, Protocol::Tcp, &error);
        assert!(message.contains(&port.to_string()), "{message}");
        #[cfg(windows)]
        assert!(
            owner(port, Protocol::Tcp).is_some_and(|name| name.ends_with(".exe")),
            "{message}"
        );
    }
}
