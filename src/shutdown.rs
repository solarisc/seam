//! Event-driven shutdown shared by workers and native capture loops.
use std::{
    collections::HashMap,
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

#[derive(Default)]
struct State {
    stopped: bool,
    next_id: u64,
    wakers: HashMap<u64, Box<dyn Fn() + Send>>,
}

#[derive(Default)]
pub struct Signal {
    state: Mutex<State>,
    wake: Condvar,
}

impl Signal {
    pub fn stop_on_drop(self: &Arc<Self>) -> StopOnDrop {
        StopOnDrop(self.clone())
    }
    pub fn stop(&self) {
        let mut state = self.state.lock().unwrap();
        if !state.stopped {
            state.stopped = true;
            for wake in state.wakers.values() {
                wake();
            }
            self.wake.notify_all();
        }
    }

    pub fn is_stopped(&self) -> bool {
        self.state.lock().unwrap().stopped
    }

    pub fn wait(&self, duration: Duration) -> bool {
        self.wake
            .wait_timeout_while(self.state.lock().unwrap(), duration, |state| !state.stopped)
            .unwrap()
            .0
            .stopped
    }

    /// Wakers enqueue native work; they must not block or call back into Signal.
    /// Holding the lock while waking makes dropping a registration a barrier
    /// before its native handles may be destroyed.
    pub fn register(self: &Arc<Self>, wake: impl Fn() + Send + 'static) -> Registration {
        let mut state = self.state.lock().unwrap();
        let id = state.next_id;
        state.next_id += 1;
        if state.stopped {
            wake();
        }
        state.wakers.insert(id, Box::new(wake));
        Registration {
            signal: self.clone(),
            id,
        }
    }
}

pub struct StopOnDrop(Arc<Signal>);
impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.stop();
    }
}

pub struct Registration {
    signal: Arc<Signal>,
    id: u64,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.signal.state.lock().unwrap().wakers.remove(&self.id);
    }
}
