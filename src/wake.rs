//! Waking a controlled computer that has gone to sleep, with Wake-on-LAN.
//!
//! While a controlled computer is connected, the hardware address it is
//! reached through is read from this computer's neighbor (ARP) table and
//! saved with the pairing. Pushing the pointer against its edge while it is
//! unreachable asks its connection thread to send a magic packet to every
//! local subnet and to try connecting every second for a while.
use std::{
    io,
    net::{IpAddr, Ipv4Addr, UdpSocket},
    sync::{Condvar, Mutex},
    time::{Duration, Instant},
};

/// Wake-on-LAN listeners use the discard port by convention.
const PORT: u16 = 9;
/// While the pointer stays against the edge, the packet goes out this often.
const RING_INTERVAL: Duration = Duration::from_secs(3);

pub type Mac = [u8; 6];

/// Accepts `04:42:1a:f1:c8:e4`, `04-42-1A-F1-C8-E4` and macOS's unpadded
/// `4:42:1a:f1:c8:e4`. Only a unicast address can be woken.
pub fn parse_mac(text: &str) -> Option<Mac> {
    let mut mac = [0u8; 6];
    let mut parts = text.trim().split([':', '-']);
    for byte in &mut mac {
        let part = parts.next()?;
        if part.is_empty() || part.len() > 2 || !part.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        *byte = u8::from_str_radix(part, 16).ok()?;
    }
    (parts.next().is_none() && unicast(&mac)).then_some(mac)
}

/// Neither empty nor a broadcast or multicast address.
fn unicast(mac: &Mac) -> bool {
    *mac != [0; 6] && mac[0] & 1 == 0
}

pub fn format_mac(mac: &Mac) -> String {
    mac.iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Six 0xFF bytes, then the address sixteen times.
pub fn magic_packet(mac: &Mac) -> [u8; 102] {
    let mut packet = [0xff; 102];
    for copy in packet[6..].as_chunks_mut::<6>().0 {
        *copy = *mac;
    }
    packet
}

/// Broadcast the magic packet on every local subnet, and send it to the
/// computer's last address too, where a router or switch may still know it.
pub fn send(mac: &Mac, last: Option<IpAddr>) -> io::Result<()> {
    let mut targets: Vec<Ipv4Addr> = crate::discovery::broadcast_addresses()
        .into_iter()
        .collect();
    targets.push(Ipv4Addr::BROADCAST);
    if let Some(IpAddr::V4(address)) = last {
        targets.push(address);
    }
    send_to(mac, &targets, PORT)
}

/// Succeeds when the packet left for at least one target.
fn send_to(mac: &Mac, targets: &[Ipv4Addr], port: u16) -> io::Result<()> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.set_broadcast(true)?;
    let packet = magic_packet(mac);
    let mut result = Err(io::Error::other("no network to send on"));
    for &target in targets {
        match socket.send_to(&packet, (target, port)) {
            Ok(_) => result = Ok(()),
            Err(error) if result.is_err() => result = Err(error),
            Err(_) => {}
        }
    }
    result
}

/// A request to wake one controlled computer, raised by the input hooks and
/// served by that computer's connection thread. Hearing the computer announce
/// itself on the network also ends that thread's wait, so it connects at once.
#[derive(Default)]
pub(crate) struct Alarm {
    state: Mutex<AlarmState>,
    changed: Condvar,
}

#[derive(Default)]
struct AlarmState {
    target: Option<(Mac, Option<IpAddr>)>,
    rung: Option<Instant>,
    pending: bool,
    heard: bool,
    stopped: bool,
}

impl Alarm {
    pub fn new(mac: Option<Mac>, address: Option<IpAddr>) -> Self {
        Self {
            state: Mutex::new(AlarmState {
                target: mac.map(|mac| (mac, address)),
                ..AlarmState::default()
            }),
            changed: Condvar::new(),
        }
    }

    /// The computer was reached at `address` through `mac`.
    pub fn aim(&self, mac: Mac, address: IpAddr) {
        self.state.lock().unwrap().target = Some((mac, Some(address)));
    }

    /// From the input hooks: never waits for the network. Asking again
    /// within a few seconds does nothing.
    pub fn ring(&self) {
        let mut state = self.state.lock().unwrap();
        if state.target.is_none() || state.rung.is_some_and(|at| at.elapsed() < RING_INTERVAL) {
            return;
        }
        state.rung = Some(Instant::now());
        state.pending = true;
        self.changed.notify_all();
    }

    /// The computer's beacon was heard after a silence: it is back on the
    /// network, so stop waiting to try it.
    pub fn heard(&self) {
        self.state.lock().unwrap().heard = true;
        self.changed.notify_all();
    }

    /// Whether the computer was heard since the last call.
    pub fn take_heard(&self) -> bool {
        std::mem::take(&mut self.state.lock().unwrap().heard)
    }

    /// Wait up to `timeout` for a request, or until the computer is heard;
    /// returns where to send the packet when one was requested.
    pub fn wait(&self, timeout: Duration) -> Option<(Mac, Option<IpAddr>)> {
        let state = self.state.lock().unwrap();
        let mut state = self
            .changed
            .wait_timeout_while(state, timeout, |state| {
                !state.pending && !state.heard && !state.stopped
            })
            .unwrap()
            .0;
        if !std::mem::take(&mut state.pending) {
            return None;
        }
        state.target
    }

    /// Asked to wake the computer within `period`, so it may be starting up.
    pub fn rung_within(&self, period: Duration) -> bool {
        self.state
            .lock()
            .unwrap()
            .rung
            .is_some_and(|at| at.elapsed() < period)
    }

    /// Ends a wait at once, for shutdown. Never calls back into a `Signal`.
    pub fn stop(&self) {
        self.state.lock().unwrap().stopped = true;
        self.changed.notify_all();
    }
}

/// The hardware address this computer reaches `address` through, if it is on
/// a local subnet and was heard from recently. A routed address has no entry
/// of its own: the router's address is never mistaken for it.
#[cfg(windows)]
pub fn neighbor_mac(address: IpAddr) -> Option<Mac> {
    use windows_sys::Win32::{
        NetworkManagement::IpHelper::{FreeMibTable, GetIpNetTable2, MIB_IPNET_TABLE2},
        Networking::WinSock::{
            AF_INET, AF_INET6, AF_UNSPEC, NlnsIncomplete, NlnsUnreachable, SOCKADDR_INET,
        },
    };
    fn ip(address: &SOCKADDR_INET) -> Option<IpAddr> {
        // SAFETY: the family says which member of the union is set.
        unsafe {
            match address.si_family {
                AF_INET => Some(IpAddr::from(
                    address.Ipv4.sin_addr.S_un.S_addr.to_ne_bytes(),
                )),
                AF_INET6 => Some(IpAddr::from(address.Ipv6.sin6_addr.u.Byte)),
                _ => None,
            }
        }
    }
    let mut table: *mut MIB_IPNET_TABLE2 = std::ptr::null_mut();
    if unsafe { GetIpNetTable2(AF_UNSPEC, &mut table) } != 0 || table.is_null() {
        return None;
    }
    // SAFETY: the table holds `NumEntries` rows until it is freed below.
    let rows = unsafe {
        std::slice::from_raw_parts((*table).Table.as_ptr(), (*table).NumEntries as usize)
    };
    let found = rows
        .iter()
        .filter(|row| {
            row.PhysicalAddressLength == 6
                && row.State != NlnsUnreachable
                && row.State != NlnsIncomplete
                && ip(&row.Address) == Some(address)
        })
        .find_map(|row| Mac::try_from(&row.PhysicalAddress[..6]).ok())
        .filter(unicast);
    unsafe { FreeMibTable(table.cast()) };
    found
}

#[cfg(target_os = "macos")]
pub fn neighbor_mac(address: IpAddr) -> Option<Mac> {
    // macOS keeps only IPv4 neighbors in its ARP table.
    let IpAddr::V4(address) = address else {
        return None;
    };
    let output = std::process::Command::new("/usr/sbin/arp")
        .args(["-n", &address.to_string()])
        .output()
        .ok()?;
    arp_entry(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn neighbor_mac(_address: IpAddr) -> Option<Mac> {
    None
}

/// The address in `arp -n` output such as
/// `? (192.168.1.116) at 32:e4:a8:69:95:19 on en0 ifscope [ethernet]`.
#[cfg(any(target_os = "macos", test))]
fn arp_entry(output: &str) -> Option<Mac> {
    let (_, rest) = output.split_once(") at ")?;
    parse_mac(rest.split_whitespace().next()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_parse_in_every_notation_and_only_unicast_ones_count() {
        let mac = [0x04, 0x42, 0x1a, 0xf1, 0xc8, 0xe4];
        assert_eq!(parse_mac("04:42:1a:f1:c8:e4"), Some(mac));
        assert_eq!(parse_mac(" 04-42-1A-F1-C8-E4 "), Some(mac));
        assert_eq!(parse_mac("4:42:1a:f1:c8:e4"), Some(mac));
        assert_eq!(format_mac(&mac), "04:42:1a:f1:c8:e4");
        for bad in [
            "",
            "04:42:1a:f1:c8",
            "04:42:1a:f1:c8:e4:00",
            "04:42:1a:f1:c8:+e",
            "004:42:1a:f1:c8:e4",
            "00:00:00:00:00:00",
            "ff:ff:ff:ff:ff:ff",
            "01:00:5e:00:00:fb",
        ] {
            assert_eq!(parse_mac(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_magic_packet_repeats_the_address_sixteen_times() {
        let mac = [0x32, 0xe4, 0xa8, 0x69, 0x95, 0x19];
        let packet = magic_packet(&mac);
        assert_eq!(packet[..6], [0xff; 6]);
        assert!(packet[6..].chunks(6).all(|copy| copy == mac));
        assert_eq!(packet[6..].chunks(6).count(), 16);
    }

    #[test]
    fn arp_output_gives_the_address_or_nothing() {
        assert_eq!(
            arp_entry("? (192.168.1.116) at 32:e4:a8:69:95:19 on en0 ifscope [ethernet]\n"),
            Some([0x32, 0xe4, 0xa8, 0x69, 0x95, 0x19])
        );
        assert_eq!(
            arp_entry("? (192.168.1.5) at 0:15:5d:1:2:3 on en0 ifscope [ethernet]"),
            Some([0, 0x15, 0x5d, 1, 2, 3])
        );
        assert_eq!(
            arp_entry("? (192.168.1.9) at (incomplete) on en0 ifscope [ethernet]"),
            None
        );
        assert_eq!(arp_entry("192.168.1.9 (192.168.1.9) -- no entry"), None);
    }

    #[test]
    fn an_alarm_asks_once_per_interval_and_only_with_an_address() {
        let silent = Alarm::default();
        silent.ring();
        assert_eq!(silent.wait(Duration::ZERO), None);

        let mac = [0x04, 0x42, 0x1a, 0xf1, 0xc8, 0xe4];
        let alarm = Alarm::new(Some(mac), None);
        assert!(!alarm.rung_within(Duration::from_secs(60)));
        alarm.ring();
        assert_eq!(alarm.wait(Duration::ZERO), Some((mac, None)));
        assert!(alarm.rung_within(Duration::from_secs(60)));
        // Held against the edge, the pointer keeps asking; most are dropped.
        alarm.ring();
        assert_eq!(alarm.wait(Duration::ZERO), None);
        let address = IpAddr::from([192, 168, 1, 241]);
        alarm.aim(mac, address);
        alarm.state.lock().unwrap().rung = Instant::now().checked_sub(RING_INTERVAL);
        alarm.ring();
        assert_eq!(alarm.wait(Duration::ZERO), Some((mac, Some(address))));
    }

    #[test]
    fn stopping_ends_a_wait_without_a_request() {
        let alarm = std::sync::Arc::new(Alarm::default());
        let waiter = alarm.clone();
        let waiting = std::thread::spawn(move || {
            let started = Instant::now();
            (waiter.wait(Duration::from_secs(30)), started.elapsed())
        });
        std::thread::sleep(Duration::from_millis(50));
        alarm.stop();
        let (request, waited) = waiting.join().unwrap();
        assert_eq!(request, None);
        assert!(waited < Duration::from_secs(5));
    }

    #[test]
    fn hearing_the_computer_ends_a_wait_once() {
        let alarm = std::sync::Arc::new(Alarm::default());
        let waiter = alarm.clone();
        let waiting = std::thread::spawn(move || {
            let started = Instant::now();
            (waiter.wait(Duration::from_secs(30)), started.elapsed())
        });
        std::thread::sleep(Duration::from_millis(50));
        alarm.heard();
        let (request, waited) = waiting.join().unwrap();
        assert_eq!(request, None, "hearing it is no request to wake it");
        assert!(waited < Duration::from_secs(5));
        assert!(alarm.take_heard());
        assert!(!alarm.take_heard());
        let started = Instant::now();
        alarm.wait(Duration::from_millis(100));
        assert!(started.elapsed() >= Duration::from_millis(100));
    }

    #[test]
    fn the_packet_arrives_whole() {
        // Never broadcast from a test: that would reach the real network.
        let listener = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let mac = [0x02, 0, 0, 0, 0, 0x01];
        send_to(&mac, &[Ipv4Addr::LOCALHOST], port).unwrap();
        let mut buffer = [0u8; 256];
        let (size, _) = listener.recv_from(&mut buffer).unwrap();
        assert_eq!(buffer[..size], magic_packet(&mac));
    }
}
