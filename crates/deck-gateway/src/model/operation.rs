//! One model request's transport and response state. A request owns its ureq
//! pool so shutting an idle provider socket cannot cancel a neighboring turn.
//! Grant and credential revocations share one terminal transition and cannot
//! write competing responses.

use crate::{credential::Refusal, http};
use std::io;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Waiting,
    Streaming,
    Revoked,
    Finished,
}

struct State {
    guest: UnixStream,
    agent: ureq::Agent,
    phase: Mutex<Phase>,
}

pub(super) struct Operation {
    state: Arc<State>,
}

pub(super) enum Revocation {
    Grant,
    Credential(Refusal),
}

impl Operation {
    pub fn new(guest: &UnixStream, config: &ureq::config::Config) -> io::Result<Self> {
        Ok(Self {
            state: Arc::new(State {
                guest: guest.try_clone()?,
                agent: config.new_agent(),
                phase: Mutex::new(Phase::Waiting),
            }),
        })
    }

    pub fn agent(&self) -> &ureq::Agent {
        &self.state.agent
    }

    pub fn revoker(&self, reason: Revocation) -> impl Fn() + Send + Sync + 'static {
        let state = self.state.clone();
        move || {
            let mut phase = state.phase.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(*phase, Phase::Revoked | Phase::Finished) {
                return;
            }
            let waiting = *phase == Phase::Waiting;
            *phase = Phase::Revoked;
            // Wake blocked provider I/O before attempting any guest writes.
            state.agent.clone().shutdown();
            if waiting {
                let _ = state.guest.set_write_timeout(Some(Duration::from_millis(100)));
                match &reason {
                    Revocation::Credential(refusal) => {
                        let _ = refusal.write(&mut &state.guest);
                    }
                    Revocation::Grant => {
                        let _ = http::write_denied(
                            &mut &state.guest,
                            None,
                            false,
                            "the authorizing model grant was revoked or expired",
                        );
                    }
                }
            }
            let _ = state.guest.shutdown(Shutdown::Both);
        }
    }

    /// Serialize an error response with cancellation and make it terminal.
    pub fn refuse(&self, write: impl FnOnce(&mut &UnixStream) -> io::Result<()>) -> io::Result<()> {
        let mut phase = self.state.phase.lock().unwrap_or_else(|e| e.into_inner());
        if *phase == Phase::Revoked {
            return Ok(());
        }
        *phase = Phase::Finished;
        drop(phase);
        write(&mut &self.state.guest)
    }

    /// Commit the head before writing it. Cancellation after commitment
    /// interrupts that response rather than trying to write another head.
    pub fn start(&self, status: u16, content_type: &str) -> io::Result<bool> {
        let mut phase = self.state.phase.lock().unwrap_or_else(|e| e.into_inner());
        if *phase == Phase::Revoked {
            return Ok(false);
        }
        *phase = Phase::Streaming;
        drop(phase);
        http::write_stream_head(&mut &self.state.guest, status, content_type)?;
        Ok(true)
    }

    pub fn active(&self) -> bool {
        *self.state.phase.lock().unwrap_or_else(|e| e.into_inner()) != Phase::Revoked
    }

    pub fn finish(&self) -> io::Result<()> {
        // Even the last five bytes can block behind a full guest buffer.
        // Keep the operation revocable until that write completes.
        http::end_stream(&mut &self.state.guest)?;
        let mut phase = self.state.phase.lock().unwrap_or_else(|e| e.into_inner());
        if *phase == Phase::Revoked {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "model request was revoked"));
        }
        *phase = Phase::Finished;
        Ok(())
    }
}

impl Drop for Operation {
    fn drop(&mut self) {
        // A monitor may have removed a callback just before its guard drops.
        // Completion must make that already-captured callback harmless too.
        *self.state.phase.lock().unwrap_or_else(|e| e.into_inner()) = Phase::Finished;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::sync::mpsc;

    #[test]
    fn revocation_interrupts_the_final_write_to_a_nonreading_guest() {
        let (mut guest, _peer) = UnixStream::pair().unwrap();
        let operation = Operation::new(&guest, &ureq::Agent::config_builder().build()).unwrap();
        operation.start(200, "text/event-stream").unwrap();
        guest.set_nonblocking(true).unwrap();
        while guest.write(&[0]).is_ok() {}
        guest.set_nonblocking(false).unwrap();
        guest.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
        let revoke = operation.revoker(Revocation::Grant);
        let (finished, result) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let outcome = operation.finish();
            finished.send(outcome).unwrap();
        });
        assert!(result.recv_timeout(Duration::from_millis(100)).is_err(), "the final write must be blocked");
        let started = std::time::Instant::now();
        revoke();
        assert!(result.recv_timeout(Duration::from_millis(500)).unwrap().is_err());
        assert!(started.elapsed() < Duration::from_millis(500));
        worker.join().unwrap();
    }

    #[test]
    fn competing_revocations_and_a_late_provider_error_cannot_write_two_responses() {
        use std::io::Read as _;
        let (guest, mut peer) = UnixStream::pair().unwrap();
        let operation = Operation::new(&guest, &ureq::Agent::config_builder().build()).unwrap();
        operation.revoker(Revocation::Grant)();
        operation.revoker(Revocation::Credential(Refusal::Revoked("key".into())))();
        operation.refuse(|_| panic!("a late provider failure must not answer again")).unwrap();
        let mut response = String::new();
        peer.read_to_string(&mut response).unwrap();
        assert_eq!(response.matches("HTTP/1.1").count(), 1);
        assert!(response.contains("deck_boundary_denied"));
        assert!(!response.contains("deck_credential_revoked"));
    }

    #[test]
    fn a_callback_collected_before_completion_cannot_revoke_a_finished_response() {
        let (mut guest, _peer) = UnixStream::pair().unwrap();
        let operation = Operation::new(&guest, &ureq::Agent::config_builder().build()).unwrap();
        let revoke = operation.revoker(Revocation::Grant);
        operation.start(200, "text/event-stream").unwrap();
        operation.finish().unwrap();
        revoke();
        guest.write_all(b"probe: connection was not shut down").unwrap();
    }
}
