pub use crate::input_traffic::InputTraffic;
use crate::{config::normalize_pair_code, protocol::MAX_WIRE_SIZE};
use anyhow::{Context, Result, bail};
use bincode::Options;
use data_encoding::BASE32_NOPAD;
use serde::{Serialize, de::DeserializeOwned};
use snow::{Builder, StatelessTransportState, params::NoiseParams};
use std::{
    io::{self, BufReader, Read, Write},
    net::{TcpStream, ToSocketAddrs},
    sync::Arc,
    time::{Duration, Instant},
};

fn psk(pair_code: &str) -> Result<[u8; 32]> {
    let raw = BASE32_NOPAD.decode(normalize_pair_code(pair_code).as_bytes())?;
    if raw.len() != 16 {
        bail!("invalid pair code");
    }
    Ok(blake3::derive_key("Seam Noise PSK protocol 1", &raw))
}

fn packet_options() -> impl Options {
    bincode::options()
        .with_fixint_encoding()
        .with_limit(MAX_WIRE_SIZE as u64)
        .reject_trailing_bytes()
}

pub(crate) fn write_frame(stream: &mut TcpStream, bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() || bytes.len() > MAX_WIRE_SIZE {
        bail!("invalid frame size");
    }
    let len = u16::try_from(bytes.len())?;
    let mut frame = Vec::with_capacity(bytes.len() + 2);
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(bytes);
    stream.write_all(&frame)?;
    Ok(())
}

fn read_frame_into(stream: &mut impl Read, bytes: &mut Vec<u8>) -> Result<()> {
    let mut size = [0u8; 2];
    stream.read_exact(&mut size)?;
    let len = usize::from(u16::from_be_bytes(size));
    if len == 0 {
        bail!("empty frame");
    }
    bytes.resize(len, 0);
    stream.read_exact(bytes)?;
    Ok(())
}

/// Caps connections that have not yet proven they know the pairing secret,
/// so unsolicited traffic on the network cannot pile up threads.
#[derive(Clone)]
pub(crate) struct Admission {
    open: Arc<std::sync::atomic::AtomicUsize>,
    limit: usize,
}

pub(crate) struct Admitted(Arc<std::sync::atomic::AtomicUsize>);

impl Admission {
    pub fn new(limit: usize) -> Self {
        Self {
            open: Arc::default(),
            limit,
        }
    }

    pub fn enter(&self) -> Option<Admitted> {
        use std::sync::atomic::Ordering;
        let mut open = self.open.load(Ordering::Acquire);
        while open < self.limit {
            match self.open.compare_exchange_weak(
                open,
                open + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(Admitted(self.open.clone())),
                Err(current) => open = current,
            }
        }
        None
    }
}

impl Drop for Admitted {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// The whole handshake, not each socket read, must finish within this time,
/// so a peer sending a byte at a time cannot hold a connection slot.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
/// Handshake messages are a key and a tag; anything larger is not Seam.
const MAX_HANDSHAKE_FRAME: usize = 128;
/// Encrypted by the connecting side with the new session key. NNpsk0's first
/// message can be replayed by anyone who recorded it; only the computer that
/// created it can follow up with this.
const KEY_CONFIRMATION: &[u8] = b"Seam key confirmation";
/// Sent in place of the handshake reply when the pairing secret is unknown.
/// A real reply is a key and a tag, never a single byte.
const KEY_REJECTED: u8 = 0;
/// The settings window looks for this text to say a computer is not recognized.
pub const KEY_MISMATCH: &str = "pair code did not match";
const VERSION_MISMATCH: &str =
    "The other computer runs a different Seam version. Install the same version on both.";

/// Read one handshake frame, failing at `deadline` however slowly it arrives.
pub(crate) fn read_handshake_frame(stream: &mut TcpStream, deadline: Instant) -> Result<Vec<u8>> {
    read_frame_by(stream, MAX_HANDSHAKE_FRAME, deadline)
}

/// Read one frame of at most `max_len` bytes, failing at `deadline`.
pub(crate) fn read_frame_by(
    stream: &mut TcpStream,
    max_len: usize,
    deadline: Instant,
) -> Result<Vec<u8>> {
    let mut size = [0u8; 2];
    read_exact_by(stream, &mut size, deadline)?;
    let len = usize::from(u16::from_be_bytes(size));
    if len == 0 || len > max_len {
        bail!("invalid message size");
    }
    let mut bytes = vec![0; len];
    read_exact_by(stream, &mut bytes, deadline)?;
    Ok(bytes)
}

fn read_exact_by(stream: &mut TcpStream, mut buffer: &mut [u8], deadline: Instant) -> Result<()> {
    while !buffer.is_empty() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("the other computer did not finish connecting in time");
        }
        stream.set_read_timeout(Some(remaining))?;
        match stream.read(buffer) {
            Ok(0) => bail!("the other computer closed the connection"),
            Ok(read) => buffer = &mut buffer[read..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub struct SecureConnection {
    stream: TcpStream,
    crypto: Arc<StatelessTransportState>,
    binding: [u8; 32],
    /// First transport nonces; key confirmation uses the initiator's nonce 0.
    read_nonce: u64,
    write_nonce: u64,
}

impl SecureConnection {
    pub fn accept(mut stream: TcpStream, code: &str) -> Result<Self> {
        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        stream.set_nodelay(true)?;
        stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
        let params: NoiseParams = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s".parse()?;
        let key = psk(code)?;
        let mut noise = Builder::new(params).psk(0, &key)?.build_responder()?;
        let mut buffer = [0u8; MAX_WIRE_SIZE];
        let first = read_handshake_frame(&mut stream, deadline).context("reading handshake")?;
        if noise.read_message(&first, &mut buffer).is_err() {
            // Say so instead of just closing, so the other computer can tell
            // "not recognized" apart from "unreachable".
            let _ = write_frame(&mut stream, &[KEY_REJECTED]);
            bail!(KEY_MISMATCH);
        }
        let n = noise.write_message(&[], &mut buffer)?;
        write_frame(&mut stream, &buffer[..n])?;
        let binding = noise.get_handshake_hash().try_into()?;
        let crypto = Arc::new(noise.into_stateless_transport_mode()?);
        let confirmation =
            read_handshake_frame(&mut stream, deadline).context("reading key confirmation")?;
        let n = crypto
            .read_message(0, &confirmation, &mut buffer)
            .context("the connection was not confirmed")?;
        if &buffer[..n] != KEY_CONFIRMATION {
            // It knows the secret but speaks an older protocol.
            bail!(VERSION_MISMATCH);
        }
        stream.set_read_timeout(None)?;
        stream.set_write_timeout(None)?;
        Ok(Self {
            stream,
            crypto,
            binding,
            read_nonce: 1,
            write_nonce: 0,
        })
    }

    pub fn connect(address: &str, code: &str) -> Result<Self> {
        let mut last_error = None;
        let deadline = Instant::now() + Duration::from_secs(5);
        for socket in address
            .to_socket_addrs()
            .context("resolving peer address")?
        {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match TcpStream::connect_timeout(&socket, remaining.min(Duration::from_secs(2))) {
                Ok(stream) => return Self::connect_socket(stream, code),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error
            .unwrap_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AddrNotAvailable,
                    "No reachable address for the paired computer",
                )
            })
            .into())
    }

    pub(crate) fn connect_socket(mut stream: TcpStream, code: &str) -> Result<Self> {
        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        stream.set_nodelay(true)?;
        stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
        let params: NoiseParams = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s".parse()?;
        let key = psk(code)?;
        let mut noise = Builder::new(params).psk(0, &key)?.build_initiator()?;
        let mut buffer = [0u8; MAX_WIRE_SIZE];
        let n = noise.write_message(&[], &mut buffer)?;
        write_frame(&mut stream, &buffer[..n])?;
        let response = read_handshake_frame(&mut stream, deadline).context("reading handshake")?;
        if response == [KEY_REJECTED] {
            bail!(KEY_MISMATCH);
        }
        noise
            .read_message(&response, &mut buffer)
            .context(KEY_MISMATCH)?;
        let binding = noise.get_handshake_hash().try_into()?;
        let crypto = Arc::new(noise.into_stateless_transport_mode()?);
        let n = crypto.write_message(0, KEY_CONFIRMATION, &mut buffer)?;
        write_frame(&mut stream, &buffer[..n])?;
        stream.set_read_timeout(None)?;
        stream.set_write_timeout(None)?;
        Ok(Self {
            stream,
            crypto,
            binding,
            read_nonce: 0,
            write_nonce: 1,
        })
    }

    /// Noise's final transcript hash binds auxiliary channels to this exact
    /// authenticated connection; a reconnect produces a different value.
    pub(crate) fn binding(&self) -> [u8; 32] {
        self.binding
    }

    pub(crate) fn peer_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.stream.peer_addr()
    }

    pub(crate) fn cancellation_socket(&self) -> std::io::Result<TcpStream> {
        self.stream.try_clone()
    }

    /// Request native input traffic treatment. Keep the returned owner alive
    /// through reader/writer teardown; never use this for the bulk data channel.
    pub fn classify_input(&self) -> io::Result<InputTraffic> {
        InputTraffic::new(&self.stream)
    }

    pub fn split(self) -> Result<(SecureReader, SecureWriter)> {
        Ok((
            SecureReader {
                stream: BufReader::new(self.stream.try_clone()?),
                timeout: None,
                socket_timeout: None,
                cancellation: None,
                crypto: self.crypto.clone(),
                nonce: self.read_nonce,
                cipher: Vec::new(),
                plain: Vec::new(),
            },
            SecureWriter {
                stream: self.stream,
                crypto: self.crypto,
                nonce: self.write_nonce,
                plain: Vec::new(),
                frame: Vec::new(),
            },
        ))
    }
}

pub struct SecureReader {
    stream: BufReader<TcpStream>,
    timeout: Option<Duration>,
    socket_timeout: Option<Duration>,
    cancellation: Option<Arc<crate::shutdown::Signal>>,
    crypto: Arc<StatelessTransportState>,
    nonce: u64,
    cipher: Vec<u8>,
    plain: Vec<u8>,
}

impl SecureReader {
    /// Include complete frames waiting in the kernel, without waiting for a
    /// missing header/body or changing socket flags shared with the writer.
    #[cfg(target_os = "macos")]
    pub(crate) fn read_ready<T: DeserializeOwned>(&mut self) -> Result<Option<T>> {
        use std::os::fd::AsRawFd;
        if let Some(message) = self.read_buffered()? {
            return Ok(Some(message));
        }
        let fd = self.stream.get_ref().as_raw_fd();
        let mut available: libc::c_int = 0;
        if unsafe { libc::ioctl(fd, libc::FIONREAD, &mut available) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        let buffered = self.stream.buffer();
        let copied = buffered.len().min(2);
        let mut header = [0; 2];
        header[..copied].copy_from_slice(&buffered[..copied]);
        if copied < 2 {
            if available < (2 - copied) as i32 {
                return Ok(None);
            }
            let read = unsafe {
                libc::recv(
                    fd,
                    header[copied..].as_mut_ptr().cast(),
                    2 - copied,
                    libc::MSG_PEEK | libc::MSG_DONTWAIT,
                )
            };
            if read < 0 {
                let error = io::Error::last_os_error();
                return if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) {
                    Ok(None)
                } else {
                    Err(error.into())
                };
            }
            if read as usize != 2 - copied {
                return Ok(None);
            }
        }
        let size = usize::from(u16::from_be_bytes(header)) + 2;
        if buffered.len() + (available.max(0) as usize) < size {
            return Ok(None);
        }
        // This reader alone consumes socket bytes. The entire authenticated
        // frame is available now; partial frames remain untouched for read().
        self.read().map(Some)
    }

    /// Decode only a complete frame already in the userspace receive buffer.
    /// Never read ahead from the socket or wait for another input sample.
    #[cfg(any(target_os = "macos", test))]
    pub(crate) fn read_buffered<T: DeserializeOwned>(&mut self) -> Result<Option<T>> {
        let bytes = self.stream.buffer();
        if bytes.len() < 2 {
            return Ok(None);
        }
        let size = usize::from(u16::from_be_bytes([bytes[0], bytes[1]]));
        if bytes.len() < size + 2 {
            return Ok(None);
        }
        self.read().map(Some)
    }

    /// Bound the entire frame, not each partial socket read. Only a complete,
    /// authenticated message lets the caller start a new deadline.
    pub fn set_timeout(&mut self, timeout: Duration) -> io::Result<()> {
        self.stream.get_ref().set_read_timeout(Some(timeout))?;
        self.timeout = Some(timeout);
        self.socket_timeout = Some(timeout);
        Ok(())
    }

    pub(crate) fn clear_timeout(&mut self) -> io::Result<()> {
        self.stream.get_ref().set_read_timeout(None)?;
        self.timeout = None;
        self.socket_timeout = None;
        Ok(())
    }
    pub(crate) fn set_cancellation(
        &mut self,
        signal: Arc<crate::shutdown::Signal>,
    ) -> io::Result<()> {
        // Windows can leave a blocked recv pending after another socket handle
        // calls shutdown. A short data-channel timeout bounds retirement, while
        // FrameReader preserves partial frames across idle timeouts.
        self.stream
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(250)))?;
        self.cancellation = Some(signal);
        Ok(())
    }

    pub fn read<T: DeserializeOwned>(&mut self) -> Result<T> {
        let deadline = self.timeout.map(|timeout| Instant::now() + timeout);
        read_frame_into(
            &mut FrameReader {
                stream: &mut self.stream,
                deadline,
                socket_timeout: &mut self.socket_timeout,
                cancellation: self.cancellation.as_deref(),
            },
            &mut self.cipher,
        )?;
        // A process can resume after its deadline with bytes already queued in
        // the kernel. Do not admit that stale frame just because read succeeded.
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(io::Error::from(io::ErrorKind::TimedOut).into());
        }
        self.plain.resize(self.cipher.len(), 0);
        let n = self
            .crypto
            .read_message(self.nonce, &self.cipher, &mut self.plain)?;
        self.nonce = self
            .nonce
            .checked_add(1)
            .context("receive nonce exhausted")?;
        packet_options()
            .deserialize(&self.plain[..n])
            .context("invalid encrypted message")
    }

    pub fn shutdown(&self) -> io::Result<()> {
        self.stream.get_ref().shutdown(std::net::Shutdown::Both)
    }

    pub(crate) fn cancellation_socket(&self) -> io::Result<TcpStream> {
        self.stream.get_ref().try_clone()
    }
}

struct FrameReader<'a> {
    stream: &'a mut BufReader<TcpStream>,
    deadline: Option<Instant>,
    socket_timeout: &'a mut Option<Duration>,
    cancellation: Option<&'a crate::shutdown::Signal>,
}

impl Read for FrameReader<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        loop {
            if self
                .cancellation
                .is_some_and(crate::shutdown::Signal::is_stopped)
            {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "data session cancelled",
                ));
            }
            if let Some(deadline) = self.deadline {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(io::ErrorKind::TimedOut.into());
                }
                if self.stream.buffer().is_empty() {
                    // Most frames fit in one buffered receive. Millisecond
                    // precision also avoids a setsockopt syscall on every event
                    // merely because a few nanoseconds elapsed since its start.
                    let timeout = Duration::from_millis(
                        remaining.as_millis().clamp(1, u128::from(u64::MAX)) as u64,
                    );
                    if *self.socket_timeout != Some(timeout) {
                        self.stream.get_ref().set_read_timeout(Some(timeout))?;
                        *self.socket_timeout = Some(timeout);
                    }
                }
            }
            match self.stream.read(bytes) {
                Err(error)
                    if (self.deadline.is_some() || self.cancellation.is_some())
                        && matches!(
                            error.kind(),
                            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                        ) =>
                {
                    // A rounded OS timeout can expire just before the absolute
                    // deadline. Retry only for its remaining duration.
                }
                result => return result,
            }
        }
    }
}

pub struct SecureWriter {
    stream: TcpStream,
    crypto: Arc<StatelessTransportState>,
    nonce: u64,
    plain: Vec<u8>,
    frame: Vec<u8>,
}

impl SecureWriter {
    pub fn set_timeout(&self, timeout: Duration) -> std::io::Result<()> {
        self.stream.set_write_timeout(Some(timeout))
    }
    pub fn write<T: Serialize>(&mut self, message: &T) -> Result<()> {
        // Retain bounded scratch storage across events. Clear the logical
        // lengths each time so a short key frame cannot retain pointer or bulk
        // bytes from a previous message. No batching or extra wait is added.
        self.plain.clear();
        packet_options().serialize_into(&mut self.plain, message)?;
        if self.plain.len() + 16 > MAX_WIRE_SIZE {
            bail!("message too large");
        }
        // Keep the length and ciphertext in one buffer and one TCP write.
        // With TCP_NODELAY, separate writes turn every input event into two
        // packets and can add a receive-side scheduling delay.
        self.frame.resize(self.plain.len() + 16 + 2, 0);
        let n = self
            .crypto
            .write_message(self.nonce, &self.plain, &mut self.frame[2..])?;
        self.frame[0..2].copy_from_slice(&u16::try_from(n)?.to_be_bytes());
        self.stream.write_all(&self.frame[..n + 2])?;
        self.nonce = self.nonce.checked_add(1).context("send nonce exhausted")?;
        Ok(())
    }

    pub fn shutdown(&self) -> std::io::Result<()> {
        self.stream.shutdown(std::net::Shutdown::Both)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn a_replayed_first_handshake_message_is_not_authenticated() {
        let code = crate::config::random_secret();
        // What a passive observer records from a genuine connection attempt.
        let params: NoiseParams = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s".parse().unwrap();
        let mut initiator = Builder::new(params)
            .psk(0, &psk(&code).unwrap())
            .unwrap()
            .build_initiator()
            .unwrap();
        let mut buffer = [0u8; MAX_WIRE_SIZE];
        let n = initiator.write_message(&[], &mut buffer).unwrap();
        let recorded = buffer[..n].to_vec();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            SecureConnection::accept(stream, &code)
        });
        let mut replay = TcpStream::connect(address).unwrap();
        write_frame(&mut replay, &recorded).unwrap();
        read_handshake_frame(&mut replay, Instant::now() + HANDSHAKE_TIMEOUT).unwrap();
        // Without the initiator's ephemeral key the replayer cannot confirm.
        write_frame(&mut replay, &[0x55; 37]).unwrap();
        assert!(server.join().unwrap().is_err());
    }

    #[test]
    fn a_handshake_sent_a_byte_at_a_time_is_cut_off() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let started = Instant::now();
            let result = SecureConnection::accept(stream, &crate::config::random_secret());
            (result.is_err(), started.elapsed())
        });
        let mut slow = TcpStream::connect(address).unwrap();
        slow.write_all(&48u16.to_be_bytes()).unwrap();
        let drip = std::thread::spawn(move || {
            for _ in 0..48 {
                if slow.write_all(&[0]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        });
        let (failed, elapsed) = server.join().unwrap();
        assert!(failed);
        assert!(
            elapsed < HANDSHAKE_TIMEOUT + Duration::from_secs(1),
            "{elapsed:?}"
        );
        drip.join().unwrap();
    }

    #[test]
    fn new_messages_leave_existing_encodings_unchanged() {
        // Builds of the same protocol version decode messages by variant index.
        let index = |input: &crate::protocol::Input| {
            let bytes = packet_options().serialize(input).unwrap();
            u32::from_le_bytes(bytes[..4].try_into().unwrap())
        };
        assert_eq!(
            index(&crate::protocol::Input::Pointer { x: 0.0, y: 0.0 }),
            0
        );
        assert_eq!(index(&crate::protocol::Input::Leave), 9);
        assert_eq!(index(&crate::protocol::Input::Motion { dx: 1, dy: 2 }), 10);
        let lock = crate::protocol::Input::PointerLock {
            locked: true,
            x: f64::NAN,
            y: 1.0,
        };
        assert_eq!(index(&lock), 11);
        let decoded: crate::protocol::Input = packet_options()
            .deserialize(&packet_options().serialize(&lock).unwrap())
            .unwrap();
        assert!(matches!(
            decoded,
            crate::protocol::Input::PointerLock { locked: true, x, y: 1.0 } if x.is_nan()
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ready_read_spans_socket_and_buffer_without_waiting_for_missing_bytes() {
        use std::io::BufRead;
        let code = BASE32_NOPAD.encode(&[9u8; 16]);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server_code = code.clone();
        let (sent, ready) = crossbeam_channel::bounded(1);
        let (next, wait) = crossbeam_channel::bounded(1);
        let task = std::thread::spawn(move || {
            let (_, mut writer) =
                SecureConnection::accept(listener.accept().unwrap().0, &server_code)
                    .unwrap()
                    .split()
                    .unwrap();
            let plain = packet_options().serialize(&42u32).unwrap();
            let mut frame = vec![0; plain.len() + 18];
            let len = writer
                .crypto
                .write_message(0, &plain, &mut frame[2..])
                .unwrap();
            frame[..2].copy_from_slice(&(len as u16).to_be_bytes());
            for chunk in [&frame[..1], &frame[1..3], &frame[3..]] {
                writer.stream.write_all(chunk).unwrap();
                sent.send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(2)).unwrap();
            }
            writer.nonce = 1;
            writer.write(&43u32).unwrap();
            sent.send(()).unwrap();
            wait.recv_timeout(Duration::from_secs(2)).unwrap();
        });
        let (mut reader, _) = SecureConnection::connect(&address.to_string(), &code)
            .unwrap()
            .split()
            .unwrap();
        reader.set_timeout(Duration::from_secs(1)).unwrap();
        reader.stream = BufReader::with_capacity(1, reader.stream.into_inner());
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        reader.stream.fill_buf().unwrap();
        for _ in 0..2 {
            let start = Instant::now();
            assert_eq!(reader.read_ready::<u32>().unwrap(), None);
            assert!(start.elapsed() < Duration::from_millis(100));
            next.send(()).unwrap();
            ready.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        assert_eq!(reader.read_buffered::<u32>().unwrap(), None);
        let read_arriving = |reader: &mut SecureReader| {
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                if let Some(value) = reader.read_ready::<u32>().unwrap() {
                    break value;
                }
                assert!(
                    Instant::now() < deadline,
                    "complete frame never became readable"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        assert_eq!(read_arriving(&mut reader), 42);
        next.send(()).unwrap();
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(read_arriving(&mut reader), 43);
        assert_eq!(reader.read_ready::<u32>().unwrap(), None);
        next.send(()).unwrap();
        task.join().unwrap();
    }

    #[test]
    fn buffered_read_does_not_wait_for_a_partial_frame() {
        use std::io::BufRead;
        let code = BASE32_NOPAD.encode(&[7u8; 16]);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server_code = code.clone();
        let (sent, ready) = crossbeam_channel::bounded(1);
        let (finish, wait) = crossbeam_channel::bounded(1);
        let task = std::thread::spawn(move || {
            let (_, mut writer) =
                SecureConnection::accept(listener.accept().unwrap().0, &server_code)
                    .unwrap()
                    .split()
                    .unwrap();
            writer.write(&42u32).unwrap();
            writer.write(&43u32).unwrap();
            sent.send(()).unwrap();
            wait.recv_timeout(Duration::from_secs(2)).unwrap();
        });
        let (mut reader, _) = SecureConnection::connect(&address.to_string(), &code)
            .unwrap()
            .split()
            .unwrap();
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        // Even if the socket has data, this API only inspects the existing buffer.
        assert_eq!(reader.read_buffered::<u32>().unwrap(), None);
        reader.stream = BufReader::with_capacity(1, reader.stream.into_inner());
        assert_eq!(reader.stream.fill_buf().unwrap().len(), 1);
        assert_eq!(reader.read_buffered::<u32>().unwrap(), None);
        assert_eq!(reader.read::<u32>().unwrap(), 42);
        // Both ordinary and buffered reads share the authenticated nonce sequence.
        reader.stream = BufReader::new(reader.stream.into_inner());
        reader.stream.fill_buf().unwrap();
        assert_eq!(reader.read_buffered::<u32>().unwrap(), Some(43));
        assert_eq!(reader.read_buffered::<u32>().unwrap(), None);
        finish.send(()).unwrap();
        task.join().unwrap();
    }

    #[test]
    fn encrypted_round_trip() {
        let code = BASE32_NOPAD.encode(&[1u8; 16]);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server_code = code.clone();
        let task = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            let (mut reader, mut writer) = SecureConnection::accept(socket, &server_code)
                .unwrap()
                .split()
                .unwrap();
            for _ in 0..6 {
                let message: String = reader.read().unwrap();
                writer.write(&message).unwrap();
            }
        });
        let (mut reader, mut writer) = SecureConnection::connect(&address.to_string(), &code)
            .unwrap()
            .split()
            .unwrap();
        // Exercise growing and shrinking frames, including the largest String
        // that fits beside the authentication tag and its encoded length.
        // A rejected oversized frame must not poison the next frame or nonce.
        for message in [
            String::new(),
            "hello".into(),
            "x".repeat(8 * 1024),
            "world".into(),
            "y".repeat(MAX_WIRE_SIZE - 16 - 8),
            "end".into(),
        ] {
            for length in [MAX_WIRE_SIZE - 8, MAX_WIRE_SIZE] {
                assert!(writer.write(&"z".repeat(length)).is_err());
            }
            writer.write(&message).unwrap();
            let response: String = reader.read().unwrap();
            assert_eq!(response, message);
        }
        task.join().unwrap();
    }

    #[test]
    fn idle_data_reads_survive_timeouts_and_cancel_without_waiting_for_peer() {
        let code = BASE32_NOPAD.encode(&[3u8; 16]);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server_code = code.clone();
        let (release, wait) = crossbeam_channel::bounded(1);
        let server = std::thread::spawn(move || {
            let (_, mut writer) =
                SecureConnection::accept(listener.accept().unwrap().0, &server_code)
                    .unwrap()
                    .split()
                    .unwrap();
            std::thread::sleep(Duration::from_millis(650));
            writer.write(&"still alive").unwrap();
            wait.recv_timeout(Duration::from_secs(3)).unwrap();
        });
        let (mut reader, _writer) = SecureConnection::connect(&address.to_string(), &code)
            .unwrap()
            .split()
            .unwrap();
        let stop = Arc::new(crate::shutdown::Signal::default());
        reader.set_cancellation(stop.clone()).unwrap();
        assert_eq!(reader.read::<String>().unwrap(), "still alive");
        let (finished, result) = crossbeam_channel::bounded(1);
        let task = std::thread::spawn(move || {
            finished.send(reader.read::<String>().is_err()).unwrap();
        });
        stop.stop();
        assert!(result.recv_timeout(Duration::from_secs(1)).unwrap());
        release.send(()).unwrap();
        task.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn admission_limits_concurrent_handshakes() {
        let admission = Admission::new(2);
        let first = admission.enter().unwrap();
        let _second = admission.enter().unwrap();
        assert!(admission.enter().is_none());
        drop(first);
        assert!(admission.enter().is_some());
    }

    #[test]
    fn wrong_pair_code_is_rejected() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let task = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            SecureConnection::accept(socket, &BASE32_NOPAD.encode(&[1u8; 16])).is_err()
        });
        let error =
            SecureConnection::connect(&address.to_string(), &BASE32_NOPAD.encode(&[2u8; 16]))
                .err()
                .unwrap();
        // The window tells the person the other computer doesn't know this one.
        assert!(format!("{error:#}").contains(KEY_MISMATCH), "{error:#}");
        assert!(task.join().unwrap());
    }
}
