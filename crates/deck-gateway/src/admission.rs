//! Process-local admission and drain for all gateway connections.
//!
//! A frozen gate refuses new connections before dispatch, including before an
//! extension kind is read. Existing handlers finish normally. This primitive
//! has no external control channel and does not enable automatic stopping.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Default)]
struct State {
    active: usize,
    frozen: bool,
}

/// One gateway's shared admission state. All listeners use the same gate.
#[derive(Default)]
pub struct Gate {
    state: Mutex<State>,
    changed: Condvar,
}

/// A connection's admission. It stays live until its complete handler exits.
pub struct Admission {
    gate: Arc<Gate>,
}

/// An exclusive, fully drained freeze. Dropping it reopens admission.
pub struct Freeze {
    gate: Arc<Gate>,
}

/// Why the requested freeze could not be established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreezeError {
    AlreadyFrozen,
    TimedOut,
    Poisoned,
}

impl Gate {
    /// Admit one connection, or refuse it while a freeze is being acquired or
    /// held. A poisoned gate refuses work rather than guessing at its state.
    pub fn admit(self: &Arc<Self>) -> Option<Admission> {
        let mut state = self.state.lock().ok()?;
        if state.frozen {
            return None;
        }
        state.active = state.active.checked_add(1)?;
        Some(Admission { gate: self.clone() })
    }

    /// Close admission and wait for all previously admitted handlers. On
    /// timeout, reopen admission without cancelling or altering those handlers.
    pub fn freeze(self: &Arc<Self>, within: Duration) -> Result<Freeze, FreezeError> {
        let started = Instant::now();
        let mut state = self.state.lock().map_err(|_| FreezeError::Poisoned)?;
        if state.frozen {
            return Err(FreezeError::AlreadyFrozen);
        }
        state.frozen = true;
        while state.active != 0 {
            let left = within.saturating_sub(started.elapsed());
            if left.is_zero() {
                state.frozen = false;
                self.changed.notify_all();
                return Err(FreezeError::TimedOut);
            }
            state = self.changed.wait_timeout(state, left).map_err(|_| FreezeError::Poisoned)?.0;
        }
        Ok(Freeze { gate: self.clone() })
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        if let Ok(mut state) = self.gate.state.lock() {
            state.active -= 1;
            self.gate.changed.notify_all();
        }
    }
}

impl Drop for Freeze {
    fn drop(&mut self) {
        if let Ok(mut state) = self.gate.state.lock() {
            state.frozen = false;
            self.gate.changed.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;

    #[test]
    fn freeze_refuses_new_work_and_waits_for_every_admitted_connection() {
        let gate = Arc::new(Gate::default());
        let first = gate.admit().unwrap();
        let second = gate.admit().unwrap();
        let (frozen, received) = mpsc::channel();
        let worker_gate = gate.clone();
        let worker = thread::spawn(move || {
            let lease = worker_gate.freeze(Duration::from_secs(5)).unwrap();
            frozen.send(lease).unwrap();
        });
        let until = Instant::now() + Duration::from_secs(2);
        while gate.admit().is_some() {
            assert!(Instant::now() < until, "freeze never closed admission");
            thread::yield_now();
        }
        drop(first);
        assert!(received.recv_timeout(Duration::from_millis(50)).is_err(), "one active handler remains");
        drop(second);
        let lease = received.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(gate.admit().is_none());
        assert!(matches!(gate.freeze(Duration::ZERO), Err(FreezeError::AlreadyFrozen)));
        drop(lease);
        assert!(gate.admit().is_some(), "dropping the lease reopens admission");
        worker.join().unwrap();
    }

    #[test]
    fn failed_drain_reopens_admission_without_cancelling_active_work() {
        let gate = Arc::new(Gate::default());
        let active = gate.admit().unwrap();
        assert!(matches!(gate.freeze(Duration::ZERO), Err(FreezeError::TimedOut)));
        let next = gate.admit().expect("timeout must reopen admission");
        assert!(matches!(gate.freeze(Duration::ZERO), Err(FreezeError::TimedOut)));
        drop(next);
        assert!(matches!(gate.freeze(Duration::ZERO), Err(FreezeError::TimedOut)), "original work is still active");
        drop(active);
        assert!(gate.freeze(Duration::ZERO).is_ok());
    }
}
