use crate::{
    config::Config,
    status::{NearbyPeer, Status},
};
use anyhow::Result;
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    net::{IpAddr, UdpSocket},
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Serialize, Deserialize)]
struct Beacon {
    magic: String,
    instance: u64,
    name: String,
    port: u16,
    #[serde(default)]
    pairing_id: String,
    #[serde(default)]
    os: String,
    #[serde(default)]
    accepts_pairing: bool,
    #[serde(default)]
    controller: String,
}

// A discovery hint only: the Noise handshake still authenticates every peer.
// Hash a 128-bit pairing secret so discovery never broadcasts the secret itself.
pub(crate) fn pairing_id(code: &str) -> String {
    blake3::derive_key(
        "Seam LAN pairing hint v1",
        crate::config::normalize_pair_code(code).as_bytes(),
    )
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect()
}

/// The broadcast address of every active IPv4 subnet. A wildcard limited
/// broadcast can choose a VPN or Hyper-V NIC; sending to each subnet keeps
/// the paired LAN reachable.
pub(crate) fn broadcast_addresses() -> std::collections::HashSet<std::net::Ipv4Addr> {
    let mut broadcasts = std::collections::HashSet::new();
    if let Ok(interfaces) = if_addrs::get_if_addrs() {
        for interface in interfaces {
            if !interface.is_loopback()
                && let if_addrs::IfAddr::V4(address) = interface.addr
                && let Some(broadcast) = address.broadcast
            {
                broadcasts.insert(broadcast);
            }
        }
    }
    broadcasts
}

/// Every computer listens for beacons on this port, whatever its own ports.
pub const PORT: u16 = crate::protocol::DEFAULT_PORT + 3;
/// More computers than anyone shares a keyboard with; the rest are ignored.
const MAX_NEARBY: usize = 32;

/// Announce this computer every few seconds and collect others' beacons.
/// With `listen` false (the Windows sign-in helper, which shares the machine
/// with a running agent) beacons are sent but none are received.
pub fn run(config: Config, status: Arc<Status>, listen: bool) -> Result<()> {
    let socket = UdpSocket::bind(("0.0.0.0", if listen { PORT } else { 0 })).map_err(|error| {
        let problem = crate::ports::unavailable(PORT, crate::ports::Protocol::Udp, &error);
        status.report_problem(&problem);
        anyhow::anyhow!(problem)
    })?;
    socket.set_broadcast(true)?;
    let instance = OsRng.next_u64();
    // Only a computer that accepts connections advertises its pairing hint.
    let listening = config.peers.is_empty();
    let pairable = listening && listen;
    let beacon = |controller: String| {
        serde_json::to_vec(&Beacon {
            magic: "Seam/1".into(),
            instance,
            name: config.name.clone(),
            pairing_id: if listening {
                pairing_id(&config.pair_code)
            } else {
                String::new()
            },
            port: config.listen_port,
            os: std::env::consts::OS.into(),
            accepts_pairing: pairable,
            controller,
        })
    };
    let mut peers: HashMap<(IpAddr, u16), (NearbyPeer, Instant)> = HashMap::new();
    let mut last_send = Instant::now() - Duration::from_secs(4);
    let mut buffer = [0u8; 2048];
    loop {
        if last_send.elapsed() >= Duration::from_secs(3) {
            let beacon = beacon(status.controller_name.read().unwrap().clone())?;
            for broadcast in broadcast_addresses() {
                let _ = socket.send_to(&beacon, (broadcast, PORT));
            }
            let _ = socket.send_to(&beacon, ("255.255.255.255", PORT));
            last_send = Instant::now();
        }
        if !listen {
            std::thread::sleep(Duration::from_secs(3));
            continue;
        }
        // New peer beacons wake recv immediately. With no peers, sleep until
        // the next announcement instead of polling an idle socket each second.
        socket.set_read_timeout(Some(
            Duration::from_secs(3)
                .saturating_sub(last_send.elapsed())
                .max(Duration::from_millis(1)),
        ))?;
        if let Ok((size, from)) = socket.recv_from(&mut buffer)
            && let Ok(found) = serde_json::from_slice::<Beacon>(&buffer[..size])
            && found.magic == "Seam/1"
            && found.instance != instance
            && found.name.len() <= 128
            && (1024..=65532).contains(&found.port)
            && found.pairing_id.len() <= 64
            && found.os.len() <= 16
            && found.controller.len() <= 128
            // Beacons are unauthenticated: bound what a flood can add.
            && (peers.len() < MAX_NEARBY || peers.contains_key(&(from.ip(), found.port)))
        {
            peers.insert(
                (from.ip(), found.port),
                (
                    NearbyPeer {
                        name: found.name,
                        address: from.ip().to_string(),
                        port: found.port,
                        pairing_id: found.pairing_id,
                        os: found.os,
                        accepts_pairing: found.accepts_pairing,
                        controller: found.controller,
                    },
                    Instant::now(),
                ),
            );
        }
        peers.retain(|_, (_, seen)| seen.elapsed() < Duration::from_secs(12));
        let mut listed: Vec<_> = peers.values().map(|(p, _)| p.clone()).collect();
        listed.sort_by(|a, b| a.name.cmp(&b.name).then(a.address.cmp(&b.address)));
        let appeared = {
            let mut nearby = status.nearby.write().unwrap();
            if *nearby == listed {
                continue;
            }
            let appeared = appeared(&nearby, &listed);
            *nearby = listed;
            appeared
        };
        for hint in appeared {
            status.heard(&hint);
        }
    }
}

/// Pairing hints in `now` that `before` lacked: computers heard again after
/// a silence of 12 s, or for the first time.
fn appeared(before: &[NearbyPeer], now: &[NearbyPeer]) -> Vec<String> {
    let mut hints: Vec<String> = now
        .iter()
        .map(|peer| &peer.pairing_id)
        .filter(|hint| !hint.is_empty() && !before.iter().any(|old| &old.pairing_id == *hint))
        .cloned()
        .collect();
    hints.sort();
    hints.dedup();
    hints
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(name: &str, port: u16, pairing_id: &str) -> NearbyPeer {
        NearbyPeer {
            name: name.into(),
            address: "192.168.1.9".into(),
            port,
            pairing_id: pairing_id.into(),
            os: "macos".into(),
            accepts_pairing: false,
            controller: String::new(),
        }
    }

    #[test]
    fn only_computers_not_heard_just_before_count_as_back() {
        let mac = peer("Mac", 47843, "a1");
        let pc = peer("PC", 47843, "b2");
        // A sign-in helper announces the same computer on its own port.
        let helper = peer("PC", 47853, "b2");
        let controller = peer("Hub", 47843, "");
        let none = Vec::<String>::new();
        assert_eq!(appeared(&[], std::slice::from_ref(&mac)), ["a1"]);
        let both = [pc.clone(), helper.clone()];
        assert_eq!(appeared(std::slice::from_ref(&pc), &both), none);
        let all = [mac.clone(), pc.clone(), helper];
        assert_eq!(appeared(std::slice::from_ref(&mac), &all), ["b2"]);
        assert_eq!(appeared(&[], &[controller]), none);
        assert_eq!(appeared(&[mac.clone(), pc], &[mac]), none);
    }
}
