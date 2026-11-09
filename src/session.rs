//! Admission and retirement shared by input and the authenticated workers
//! bound to it: clipboard and sound.
use crate::{
    input::Controller,
    network::{SecureConnection, SecureReader, SecureWriter},
    protocol::PROTOCOL_VERSION,
    shutdown::Signal,
};
use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    net::{Shutdown, TcpStream},
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

/// Once bound, a data write may wait this long for the peer to make room: a
/// receiver saving a large file to a slow disk pauses for seconds. A peer that
/// has gone away ends the input session first, which closes this connection.
const DATA_WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// A finished connection's data worker may take this long to stop, such as
/// a large file closing on a slow disk. After that the next connection's
/// keyboard and mouse go ahead without it: a stuck clipboard must never shut
/// input out until Seam restarts.
const DATA_CLEANUP_LIMIT: Duration = Duration::from_secs(10);

/// A connection bound to an input session besides input itself. Each input
/// session has at most one worker of each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Channel {
    /// Clipboard and file transfers.
    Data,
    /// A controlled computer's sound.
    Sound,
}

impl Channel {
    fn index(self) -> usize {
        self as usize
    }
}

struct DataWorker {
    stop: Arc<Signal>,
    socket: Option<TcpStream>,
}

impl DataWorker {
    fn cancel(&self) {
        self.stop.stop();
        if let Some(socket) = &self.socket {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }
}

struct Active {
    binding: [u8; 32],
    socket: TcpStream,
    input_finished: bool,
    /// When input finished, while its workers may still be stopping.
    finished_at: Option<Instant>,
    retiring: bool,
    /// Indexed by `Channel`.
    workers: [Option<DataWorker>; 2],
}

impl Active {
    fn cancel(&mut self) {
        self.retiring = true;
        let _ = self.socket.shutdown(Shutdown::Both);
        for worker in self.workers.iter().flatten() {
            worker.cancel();
        }
    }

    fn idle(&self) -> bool {
        self.workers.iter().all(Option::is_none)
    }
}

#[derive(Default)]
struct State {
    active: Option<Active>,
    /// A finished session whose workers outlived `DATA_CLEANUP_LIMIT`.
    /// Input goes on without it; no new worker starts until they stop.
    lingering: Option<Active>,
    resetting: bool,
    stopping: bool,
}

pub(crate) struct Control {
    state: Mutex<State>,
    finished: Condvar,
    controller: Arc<Controller>,
    children: Mutex<Vec<Arc<Control>>>,
}

// This preface and its acknowledgement are encrypted on the data connection.
// Input Hello stays wire-compatible so mismatched versions fail explicitly.
#[derive(Serialize, Deserialize)]
struct DataBinding {
    version: u16,
    input: [u8; 32],
}

impl Control {
    pub fn new(controller: Arc<Controller>) -> Arc<Self> {
        Arc::new(Self {
            children: Mutex::new(Vec::new()),
            state: Mutex::new(State::default()),
            finished: Condvar::new(),
            controller,
        })
    }

    pub fn add_peer(&self, peer: Arc<Control>) {
        self.children.lock().unwrap().push(peer);
    }

    pub fn begin(self: &Arc<Self>, connection: &SecureConnection) -> Result<Lease> {
        let mut state = self.state.lock().unwrap();
        if state.lingering.is_none()
            && state
                .active
                .as_ref()
                .and_then(|active| active.finished_at)
                .is_some_and(|finished| finished.elapsed() >= DATA_CLEANUP_LIMIT)
        {
            log::warn!(
                "the last connection's clipboard has not stopped after {} s; \
                 keyboard and mouse go on, clipboard waits until it stops",
                DATA_CLEANUP_LIMIT.as_secs()
            );
            state.lingering = state.active.take();
        }
        if state.stopping {
            bail!("Seam is stopping on this computer");
        }
        if state.resetting {
            bail!("input is being reset on this computer");
        }
        if state.active.is_some() {
            bail!("the last connection is still closing on this computer");
        }
        state.active = Some(Active {
            binding: connection.binding(),
            socket: connection.cancellation_socket()?,
            input_finished: false,
            finished_at: None,
            retiring: false,
            workers: [None, None],
        });
        Ok(Lease(self.clone()))
    }

    /// A worker from an earlier connection is still stopping, so this
    /// connection's clipboard and sound have to wait for it.
    pub fn data_lingering(&self) -> bool {
        self.state.lock().unwrap().lingering.is_some()
    }

    pub fn reserve(self: &Arc<Self>, binding: [u8; 32], channel: Channel) -> Result<DataLease> {
        let mut state = self.state.lock().unwrap();
        ensure!(!state.stopping && !state.resetting, "session is stopping");
        ensure!(
            state.lingering.is_none(),
            "the last connection's clipboard is still stopping"
        );
        let active = state
            .active
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("no input session"))?;
        ensure!(
            !active.retiring && active.binding == binding,
            "data belongs to a retired input session"
        );
        let slot = &mut active.workers[channel.index()];
        ensure!(
            slot.is_none(),
            "input session already has a {channel:?} worker"
        );
        let stop = Arc::new(Signal::default());
        *slot = Some(DataWorker {
            stop: stop.clone(),
            socket: None,
        });
        Ok(DataLease {
            control: self.clone(),
            binding,
            channel,
            stop,
        })
    }

    pub fn reserve_data(self: &Arc<Self>, binding: [u8; 32]) -> Result<DataLease> {
        self.reserve(binding, Channel::Data)
    }

    pub fn accept_data(
        self: &Arc<Self>,
        connection: SecureConnection,
    ) -> Result<(SecureReader, SecureWriter, DataLease)> {
        self.accept(connection, Channel::Data)
    }

    /// Admit a connection the controlling computer opened for `channel`, once
    /// it proves which input session it belongs to.
    pub fn accept(
        self: &Arc<Self>,
        connection: SecureConnection,
        channel: Channel,
    ) -> Result<(SecureReader, SecureWriter, DataLease)> {
        let socket = connection.cancellation_socket()?;
        let (mut reader, mut writer) = connection.split()?;
        reader.set_timeout(Duration::from_secs(3))?;
        writer.set_timeout(Duration::from_secs(3))?;
        let hello: DataBinding = reader.read()?;
        ensure!(
            hello.version == PROTOCOL_VERSION,
            "data protocol version mismatch"
        );
        let lease = self.reserve(hello.input, channel)?;
        lease.attach_socket(&socket)?;
        writer.write(&hello)?;
        reader.clear_timeout()?;
        writer.set_timeout(DATA_WRITE_TIMEOUT)?;
        Ok((reader, writer, lease))
    }

    /// Wait until native input and data workers have both finished cleanup.
    pub fn retire(&self, stopping: bool) -> Result<()> {
        let mut child_error = None;
        for peer in self.children.lock().unwrap().iter() {
            if let Err(error) = peer.retire(stopping) {
                child_error = Some(error);
            }
        }
        let mut state = self.state.lock().unwrap();
        // Once stopping, an earlier call already waited: whatever it left
        // ends with the process, and waiting again only delays the exit past
        // what the window allows for a stop.
        let wait = if state.stopping {
            Duration::ZERO
        } else {
            Duration::from_secs(4)
        };
        state.stopping |= stopping;
        state.resetting = true;
        self.controller.detach();
        if let Some(active) = &mut state.active {
            active.cancel();
        }
        let deadline = Instant::now() + wait;
        while state.active.is_some() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                // Still give the pointer back: a caller that exits next must
                // not leave it hidden.
                let _ = self.controller.settle_cursor();
                state.resetting = false;
                bail!("session cleanup did not finish; old session remains disabled");
            }
            state = self.finished.wait_timeout(state, remaining).unwrap().0;
        }
        let restored = self.controller.settle_cursor();
        state.resetting = false;
        restored.and(child_error.map_or(Ok(()), Err))
    }

    fn finish_if_retired(&self, state: &mut State) {
        if state
            .active
            .as_ref()
            .is_some_and(|active| active.input_finished && active.idle())
        {
            state.active = None;
            self.finished.notify_all();
        }
    }
}

pub(crate) struct Lease(Arc<Control>);
impl Drop for Lease {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        if let Some(active) = &mut state.active {
            active.input_finished = true;
            active.finished_at = Some(Instant::now());
            active.cancel();
        }
        self.0.finish_if_retired(&mut state);
    }
}

/// Held through retries, native watcher shutdown and partial-transfer cleanup.
/// Until it drops, a replacement input session cannot admit another worker.
pub(crate) struct DataLease {
    control: Arc<Control>,
    binding: [u8; 32],
    channel: Channel,
    pub stop: Arc<Signal>,
}

impl DataLease {
    pub fn attach_socket(&self, socket: &TcpStream) -> Result<()> {
        let mut state = self.control.state.lock().unwrap();
        let active = state
            .active
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("input session ended"))?;
        ensure!(
            !active.retiring && active.binding == self.binding,
            "input session ended"
        );
        let data = active.workers[self.channel.index()]
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("data worker ended"))?;
        if let Some(old) = data.socket.replace(socket.try_clone()?) {
            let _ = old.shutdown(Shutdown::Both);
        }
        Ok(())
    }

    pub fn connect(&self, socket: TcpStream, code: &str) -> Result<(SecureReader, SecureWriter)> {
        // Register before Noise or binding handshakes so reset can interrupt them.
        self.attach_socket(&socket)?;
        let connection = SecureConnection::connect_socket(socket, code)?;
        let (mut reader, mut writer) = connection.split()?;
        reader.set_timeout(Duration::from_secs(3))?;
        writer.set_timeout(Duration::from_secs(3))?;
        let hello = DataBinding {
            version: PROTOCOL_VERSION,
            input: self.binding,
        };
        writer.write(&hello)?;
        let accepted: DataBinding = reader.read()?;
        ensure!(
            accepted.version == hello.version && accepted.input == hello.input,
            "data binding was not acknowledged"
        );
        reader.clear_timeout()?;
        writer.set_timeout(DATA_WRITE_TIMEOUT)?;
        Ok((reader, writer))
    }
}

impl Drop for DataLease {
    fn drop(&mut self) {
        let mut state = self.control.state.lock().unwrap();
        let index = self.channel.index();
        if let Some(old) = &mut state.lingering
            && old.binding == self.binding
        {
            old.workers[index] = None;
            if old.idle() {
                log::info!("the last connection finished stopping");
                state.lingering = None;
            }
        }
        if let Some(active) = &mut state.active
            && active.binding == self.binding
            && let Some(worker) = active.workers[index].take()
        {
            worker.cancel();
        }
        self.control.finish_if_retired(&mut state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, protocol::Data};
    use std::{net::TcpListener, thread};

    fn assert_peer_closed(error: anyhow::Error) {
        assert!(
            error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                matches!(
                    error.kind(),
                    std::io::ErrorKind::UnexpectedEof
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                )
            }),
            "expected peer closure, not a timeout or protocol error: {error:#}"
        );
    }

    fn encrypted_pair(code: &str) -> (SecureConnection, SecureConnection) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server_code = code.to_owned();
        let server = thread::spawn(move || {
            SecureConnection::accept(listener.accept().unwrap().0, &server_code).unwrap()
        });
        let client = SecureConnection::connect(&address.to_string(), code).unwrap();
        (client, server.join().unwrap())
    }

    fn data_attempt(
        control: &Arc<Control>,
        code: &str,
        binding: [u8; 32],
    ) -> Result<(SecureReader, SecureWriter, DataLease)> {
        let (client, server) = encrypted_pair(code);
        let (mut reader, mut writer) = client.split().unwrap();
        reader.set_timeout(Duration::from_secs(1)).unwrap();
        writer
            .write(&DataBinding {
                version: PROTOCOL_VERSION,
                input: binding,
            })
            .unwrap();
        let accepted = control.accept_data(server);
        if accepted.is_ok() {
            let acknowledgement: DataBinding = reader.read().unwrap();
            assert_eq!(acknowledgement.version, PROTOCOL_VERSION);
            assert_eq!(acknowledgement.input, binding);
        } else {
            assert_peer_closed(reader.read::<DataBinding>().err().unwrap());
        }
        accepted
    }

    #[test]
    fn encrypted_data_admission_tracks_one_live_input_session_until_worker_cleanup() {
        // Native clipboard watchers are deliberately absent: this owns the
        // encrypted admission/cancellation contract, not clipboard contents.
        let config = Config::default();
        let control = Control::new(Controller::new(&config));
        let (client_input, server_input) = encrypted_pair(&config.pair_code);
        let binding = client_input.binding();
        assert_eq!(binding, server_input.binding());
        assert!(data_attempt(&control, &config.pair_code, binding).is_err());

        let input = control.begin(&server_input).unwrap();
        let client_control = Control::new(Controller::new(&config));
        let _client_input = client_control.begin(&client_input).unwrap();
        let client_data = client_control.reserve_data(binding).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server_control = control.clone();
        let code = config.pair_code.clone();
        let server = thread::spawn(move || {
            let connection = SecureConnection::accept(listener.accept().unwrap().0, &code).unwrap();
            server_control.accept_data(connection).unwrap()
        });
        let (mut client_reader, mut client_writer) = client_data
            .connect(TcpStream::connect(address).unwrap(), &config.pair_code)
            .unwrap();
        let (mut reader, _writer, data) = server.join().unwrap();
        client_writer
            .write(&Data::Text("session bound".into()))
            .unwrap();
        assert!(
            matches!(reader.read::<Data>().unwrap(), Data::Text(text) if text == "session bound")
        );
        assert!(data_attempt(&control, &config.pair_code, binding).is_err());

        let (_next_client, next_input) = encrypted_pair(&config.pair_code);
        let next_binding = next_input.binding();
        assert_ne!(binding, next_binding);
        drop(input);
        assert!(control.begin(&next_input).is_err());
        assert!(data_attempt(&control, &config.pair_code, binding).is_err());
        // Input retirement must close an idle data socket, not merely change
        // bookkeeping; a new input waits until its native worker scope exits.
        client_reader.set_timeout(Duration::from_secs(1)).unwrap();
        assert_peer_closed(client_reader.read::<Data>().unwrap_err());
        drop(data);

        let next = control.begin(&next_input).unwrap();
        assert!(data_attempt(&control, &config.pair_code, binding).is_err());
        let (_, _, next_data) = data_attempt(&control, &config.pair_code, next_binding).unwrap();
        drop(next_data);
        // A data-only failure can retry within the same input session.
        let (_, _, retried) = data_attempt(&control, &config.pair_code, next_binding).unwrap();
        drop(retried);
        drop(next);
    }

    #[test]
    fn a_stuck_data_worker_does_not_shut_out_the_next_input_session() {
        let config = Config::default();
        let control = Control::new(Controller::new(&config));
        let (_client, first) = encrypted_pair(&config.pair_code);
        let input = control.begin(&first).unwrap();
        let stuck = control.reserve_data(first.binding()).unwrap();
        drop(input);

        let (_next_client, next_input) = encrypted_pair(&config.pair_code);
        // A worker that is still stopping holds the next connection briefly.
        assert!(control.begin(&next_input).is_err());
        control
            .state
            .lock()
            .unwrap()
            .active
            .as_mut()
            .unwrap()
            .finished_at = Instant::now().checked_sub(DATA_CLEANUP_LIMIT);
        // One stuck past the limit no longer does, but keeps data waiting.
        let next = control.begin(&next_input).unwrap();
        assert!(control.data_lingering());
        assert!(control.reserve_data(next_input.binding()).is_err());

        drop(stuck);
        assert!(!control.data_lingering());
        let data = control.reserve_data(next_input.binding()).unwrap();
        drop(data);
        drop(next);
        assert!(control.state.lock().unwrap().active.is_none());
    }
}
