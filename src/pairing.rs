//! First-time pairing without typing secrets.
//!
//! The computer that will share its keyboard and mouse asks another computer
//! to pair. Both show a six-digit confirmation code derived from an ephemeral
//! Noise handshake; when the person accepts on the other computer, its 128-bit
//! pairing secret is sent over that encrypted channel and saved by the
//! requester. Matching codes rule out a machine in the middle.
use crate::{
    config::{Config, Peer},
    network::{read_frame_by, read_handshake_frame, write_frame},
    protocol::{MAX_WIRE_SIZE, PROTOCOL_VERSION},
    shutdown::Signal,
    status::Status,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use snow::{Builder, TransportState};
use std::{
    io::{self, Read},
    net::{TcpListener, TcpStream, ToSocketAddrs},
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

/// Pairing listens on TCP at the listen port plus this offset.
pub const PORT_OFFSET: u16 = 3;
/// How long a request waits for the person at the other computer.
const DECISION_TIMEOUT: Duration = Duration::from_secs(120);
const NOISE: &str = "Noise_NN_25519_ChaChaPoly_BLAKE2s";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Prompt {
    pub id: u64,
    pub name: String,
    pub address: String,
    pub os: String,
    /// Six digits shown on both computers, formatted "123 456".
    pub code: String,
}

#[derive(Serialize, Deserialize)]
enum Message {
    Request {
        version: u16,
        name: String,
        os: String,
    },
    Accepted {
        name: String,
        os: String,
        port: u16,
        code: String,
    },
    Declined {
        reason: String,
    },
}

/// The computer that accepted a pairing request.
#[derive(Clone, Debug)]
pub struct Paired {
    pub name: String,
    pub os: String,
    pub address: String,
    pub port: u16,
    pub code: String,
}

impl Paired {
    pub fn into_peer(self, config: &Config) -> Result<Peer> {
        let side = config
            .free_side()
            .context("Every screen edge already has a computer")?;
        Ok(Peer {
            name: self.name,
            address: self.address,
            port: self.port,
            code: self.code,
            arrangement: crate::config::Arrangement::on(side),
            mac: None,
            sound: false,
        })
    }
}

/// Progress reported to the window while a request is outstanding.
pub enum Event {
    /// The handshake finished; show this code and wait for the other computer.
    Confirm(String),
    Finished(Result<Paired, String>),
}

struct Pending {
    prompt: Prompt,
    decision: Option<bool>,
}

/// At most one request is shown at a time; others are declined as busy.
#[derive(Default)]
pub(crate) struct Inbox {
    pending: Mutex<Option<Pending>>,
    changed: Condvar,
}

impl Inbox {
    pub fn prompt(&self) -> Option<Prompt> {
        self.pending
            .lock()
            .unwrap()
            .as_ref()
            .filter(|pending| pending.decision.is_none())
            .map(|pending| pending.prompt.clone())
    }

    pub fn answer(&self, id: u64, accept: bool) -> Result<()> {
        let mut pending = self.pending.lock().unwrap();
        match pending.as_mut() {
            Some(request) if request.prompt.id == id && request.decision.is_none() => {
                request.decision = Some(accept);
                self.changed.notify_all();
                Ok(())
            }
            _ => bail!("That pairing request is no longer waiting"),
        }
    }

    fn begin(&self, prompt: Prompt) -> bool {
        let mut pending = self.pending.lock().unwrap();
        if pending.is_some() {
            return false;
        }
        *pending = Some(Pending {
            prompt,
            decision: None,
        });
        true
    }

    /// Wait for a decision. `abandoned` reports when the requester went away.
    fn decide(&self, id: u64, mut abandoned: impl FnMut() -> bool) -> Option<bool> {
        let deadline = Instant::now() + DECISION_TIMEOUT;
        let mut pending = self.pending.lock().unwrap();
        let decision = loop {
            if let Some(decision) = pending.as_ref().and_then(|p| p.decision) {
                break Some(decision);
            }
            if Instant::now() >= deadline {
                break None;
            }
            pending = self
                .changed
                .wait_timeout(pending, Duration::from_millis(250))
                .unwrap()
                .0;
            drop(pending);
            let gone = abandoned();
            pending = self.pending.lock().unwrap();
            if gone {
                break None;
            }
        };
        if pending.as_ref().is_some_and(|p| p.prompt.id == id) {
            *pending = None;
        }
        decision
    }
}

struct Channel {
    stream: TcpStream,
    noise: TransportState,
    buffer: Vec<u8>,
}

impl Channel {
    fn handshake(mut stream: TcpStream, initiator: bool) -> Result<(Self, String)> {
        // Reads set their own timeouts: the handshake's deadline here, and
        // each later exchange its own.
        let deadline = Instant::now() + crate::network::HANDSHAKE_TIMEOUT;
        stream.set_nodelay(true)?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let builder = Builder::new(NOISE.parse()?);
        let mut buffer = vec![0u8; MAX_WIRE_SIZE];
        let noise = if initiator {
            let mut noise = builder.build_initiator()?;
            let n = noise.write_message(&[], &mut buffer)?;
            write_frame(&mut stream, &buffer[..n])?;
            noise.read_message(&read_handshake_frame(&mut stream, deadline)?, &mut buffer)?;
            noise
        } else {
            let mut noise = builder.build_responder()?;
            noise.read_message(&read_handshake_frame(&mut stream, deadline)?, &mut buffer)?;
            let n = noise.write_message(&[], &mut buffer)?;
            write_frame(&mut stream, &buffer[..n])?;
            noise
        };
        ensure!(
            noise.is_handshake_finished(),
            "Pairing handshake incomplete"
        );
        let code = confirmation_code(noise.get_handshake_hash());
        let noise = noise.into_transport_mode()?;
        Ok((
            Self {
                stream,
                noise,
                buffer,
            },
            code,
        ))
    }

    fn send(&mut self, message: &Message) -> Result<()> {
        let plain = serde_json::to_vec(message)?;
        ensure!(
            plain.len() + 16 <= MAX_WIRE_SIZE,
            "Pairing message too large"
        );
        let n = self.noise.write_message(&plain, &mut self.buffer)?;
        write_frame(&mut self.stream, &self.buffer[..n])
    }

    /// The opening request is small and must arrive promptly; an
    /// unauthenticated sender may not hold the pairing listener.
    fn receive_request(&mut self) -> Result<Message> {
        let deadline = Instant::now() + crate::network::HANDSHAKE_TIMEOUT;
        let frame = read_frame_by(&mut self.stream, 1024, deadline)?;
        self.decode(&frame)
    }

    fn decode<T: DeserializeOwned>(&mut self, frame: &[u8]) -> Result<T> {
        let n = self.noise.read_message(frame, &mut self.buffer)?;
        Ok(serde_json::from_slice(&self.buffer[..n])?)
    }
}

fn confirmation_code(hash: &[u8]) -> String {
    let key = blake3::derive_key("Seam pairing confirmation v1", hash);
    let value = u32::from_le_bytes(key[..4].try_into().unwrap()) % 1_000_000;
    format!("{:03} {:03}", value / 1000, value % 1000)
}

/// Agent side: offer pairing while this computer does not control others.
pub(crate) fn serve(config: Config, status: Arc<Status>, shutdown: Arc<Signal>) -> Result<()> {
    let port = config.listen_port + PORT_OFFSET;
    let listener = TcpListener::bind(("0.0.0.0", port)).map_err(|error| {
        let problem = crate::ports::unavailable(port, crate::ports::Protocol::Tcp, &error);
        status.report_problem(&problem);
        anyhow::anyhow!(problem)
    })?;
    let admission = crate::network::Admission::new(4);
    for accepted in listener.incoming() {
        if shutdown.is_stopped() {
            break;
        }
        let Ok(stream) = accepted else { continue };
        let Some(admitted) = admission.enter() else {
            continue;
        };
        let (config, status) = (config.clone(), status.clone());
        std::thread::spawn(move || {
            let _admitted = admitted;
            if let Err(error) = answer(stream, &config, &status) {
                log::info!("pairing request ended: {error:#}");
            }
        });
    }
    Ok(())
}

fn answer(stream: TcpStream, config: &Config, status: &Status) -> Result<()> {
    let address = stream.peer_addr()?.ip().to_string();
    let (mut channel, code) = Channel::handshake(stream, false)?;
    let Message::Request { version, name, os } = channel.receive_request()? else {
        bail!("unexpected pairing message");
    };
    let decline = |channel: &mut Channel, reason: &str| {
        channel.send(&Message::Declined {
            reason: reason.into(),
        })
    };
    if version != PROTOCOL_VERSION {
        return decline(
            &mut channel,
            "The two computers run different Seam versions. Update both, then try again.",
        );
    }
    if !config.peers.is_empty() {
        return decline(
            &mut channel,
            "That computer shares its own keyboard and mouse with other computers. Remove them there first.",
        );
    }
    let name: String = name.chars().take(64).collect();
    let id = rand::random();
    let prompt = Prompt {
        id,
        name: name.clone(),
        address,
        os: os.chars().take(16).collect(),
        code,
    };
    if !status.pairing.begin(prompt) {
        return decline(&mut channel, "Another pairing request is waiting there.");
    }
    log::info!("pairing requested by {name}");
    crate::service::show_settings();
    channel
        .stream
        .set_read_timeout(Some(Duration::from_millis(1)))?;
    let decision = status.pairing.decide(id, || {
        // The requester never sends more data; EOF means it cancelled.
        let mut byte = [0u8; 1];
        match channel.stream.peek(&mut byte) {
            Ok(read) => read == 0,
            Err(error) => !matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ),
        }
    });
    channel
        .stream
        .set_read_timeout(Some(Duration::from_secs(5)))?;
    match decision {
        Some(true) => {
            channel.send(&Message::Accepted {
                name: config.name.clone(),
                os: std::env::consts::OS.into(),
                port: config.listen_port,
                code: config.pair_code.clone(),
            })?;
            *status.controller_name.write().unwrap() = name.clone();
            crate::config::edit(|saved| {
                saved.controller_name = name;
                Ok(())
            })?;
            log::info!("pairing accepted");
            Ok(())
        }
        Some(false) => decline(&mut channel, "The request was declined on that computer."),
        None => decline(&mut channel, "Nobody answered on that computer in time."),
    }
}

/// Window side: ask the computer at `address` to pair. Blocks until it answers.
pub fn request(
    address: &str,
    port: u16,
    me: &Config,
    events: &crossbeam_channel::Sender<Event>,
    cancel: &Signal,
) -> Result<Paired> {
    let target = crate::agent::address_with_port(address, port + PORT_OFFSET);
    let socket = target
        .to_socket_addrs()
        .context("Cannot resolve that address")?
        .next()
        .context("Cannot resolve that address")?;
    let stream = TcpStream::connect_timeout(&socket, Duration::from_secs(3)).with_context(|| {
        format!("Cannot reach {address}. Check that Seam is running there and both computers are on the same network.")
    })?;
    let (mut channel, code) = Channel::handshake(stream, true)?;
    channel.send(&Message::Request {
        version: PROTOCOL_VERSION,
        name: me.name.clone(),
        os: std::env::consts::OS.into(),
    })?;
    let _ = events.send(Event::Confirm(code));
    // Read in short slices so the person can cancel from this computer.
    channel
        .stream
        .set_read_timeout(Some(Duration::from_millis(250)))?;
    let deadline = Instant::now() + DECISION_TIMEOUT + Duration::from_secs(10);
    let mut header = [0u8; 2];
    let mut have = 0;
    while have < 2 {
        ensure!(!cancel.is_stopped(), "Pairing cancelled");
        ensure!(
            Instant::now() < deadline,
            "The other computer did not answer"
        );
        match channel.stream.read(&mut header[have..]) {
            Ok(0) => bail!("The other computer closed the connection"),
            Ok(n) => have += n,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(e) => return Err(e.into()),
        }
    }
    channel
        .stream
        .set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut frame = vec![0u8; usize::from(u16::from_be_bytes(header))];
    channel.stream.read_exact(&mut frame)?;
    let n = channel.noise.read_message(&frame, &mut channel.buffer)?;
    match serde_json::from_slice(&channel.buffer[..n])? {
        Message::Accepted {
            name,
            os,
            port,
            code,
        } => Ok(Paired {
            name: name.chars().take(64).collect(),
            os,
            address: address.trim().to_string(),
            port,
            code,
        }),
        Message::Declined { reason } => bail!("{reason}"),
        Message::Request { .. } => bail!("unexpected pairing message"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_request_delivers_the_secret_and_matching_codes() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port() - PORT_OFFSET;
        let receiver = Config {
            name: "Receiver".into(),
            ..Config::default()
        };
        let status = Status::new(&receiver, Vec::new());
        let server_status = status.clone();
        let server_config = receiver.clone();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let address = stream.peer_addr().unwrap().ip().to_string();
            let (mut channel, code) = Channel::handshake(stream, false).unwrap();
            let Message::Request { name, .. } = channel.receive_request().unwrap() else {
                panic!("expected request")
            };
            assert_eq!(name, "Requester");
            assert!(server_status.pairing.begin(Prompt {
                id: 9,
                name,
                address,
                os: "test".into(),
                code: code.clone(),
            }));
            assert_eq!(server_status.pairing.decide(9, || false), Some(true));
            channel
                .send(&Message::Accepted {
                    name: server_config.name.clone(),
                    os: "test".into(),
                    port: server_config.listen_port,
                    code: server_config.pair_code.clone(),
                })
                .unwrap();
            code
        });
        let answering = status.clone();
        let answer = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(prompt) = answering.pairing.prompt() {
                    answering.pairing.answer(prompt.id, true).unwrap();
                    break prompt.code;
                }
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let (events, received) = crossbeam_channel::unbounded();
        let requester = Config {
            name: "Requester".into(),
            ..Config::default()
        };
        let paired = request("127.0.0.1", port, &requester, &events, &Signal::default()).unwrap();
        let server_code = server.join().unwrap();
        let shown = answer.join().unwrap();
        let Event::Confirm(client_code) = received.try_recv().unwrap() else {
            panic!("expected a confirmation code")
        };
        assert_eq!(client_code, server_code);
        assert_eq!(shown, server_code);
        assert_eq!(paired.code, receiver.pair_code);
        assert_eq!(paired.name, "Receiver");
        assert_eq!(paired.address, "127.0.0.1");
        let peer = paired.into_peer(&requester).unwrap();
        assert_eq!(peer.arrangement.side, crate::config::Side::Right);
    }

    #[test]
    fn inbox_holds_one_request_and_rejects_stale_answers() {
        let inbox = Inbox::default();
        let prompt = |id| Prompt {
            id,
            name: "A".into(),
            address: "192.0.2.1".into(),
            os: "windows".into(),
            code: "123 456".into(),
        };
        assert!(inbox.begin(prompt(1)));
        assert!(!inbox.begin(prompt(2)));
        assert!(inbox.answer(2, true).is_err());
        assert_eq!(inbox.decide(1, || true), None);
        assert!(inbox.prompt().is_none());
        assert!(inbox.begin(prompt(3)));
        inbox.answer(3, false).unwrap();
        assert_eq!(inbox.decide(3, || false), Some(false));
    }

    #[test]
    fn confirmation_codes_are_six_digits() {
        let code = confirmation_code(&[7; 32]);
        assert_eq!(code.len(), 7);
        assert!(
            code.chars()
                .filter(|c| *c != ' ')
                .all(|c| c.is_ascii_digit())
        );
        assert_ne!(code, confirmation_code(&[8; 32]));
    }
}
