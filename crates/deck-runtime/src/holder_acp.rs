//! The holder's ACP client (ADR 0011 §3): the [`Observer`] on the relay's
//! tap, the thread that writes the holder's own lines, the turn file and the
//! `deckd` ping, and the prompt `deckd` asks for. Both holders use it, the
//! local one and the VMM.
//!
//! The design choice: the relay threads only parse and enqueue. Writing to
//! the agent, the turn file and the ping each happen on their own thread, so
//! a slow agent stdin or a busy `deckd` can never stop the agent's stdout
//! from draining.

use crate::message::LastMessage;
use crate::observer::{Event, Observer, PermissionQuestion, Refused, Setup};
use crate::permission::Observation;
use crate::record::Store;
use crate::relay::{Relay, Tap};
use crate::{Turn, wire};
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant};

/// How long a prompt may take to start: setting a session up can replay a
/// long history first.
pub(crate) const START: Duration = Duration::from_secs(60);

/// What the holder answers to a prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Prompted {
    /// The prompt started `turn`; `session` is set when the holder set the
    /// session up itself, so `deckd` records it.
    Started { turn: u64, session: Option<String> },
    /// A keyed prompt received its final response without a new turn event.
    Settled { session: Option<String> },
    /// The task is not idle.
    Busy,
}

pub(crate) struct HolderAcp {
    state: Arc<Mutex<State>>,
    changed: Condvar,
    out: mpsc::Sender<String>,
    recording: Recording,
    run: u64,
}

struct Record {
    observation: Observation,
    message: Option<LastMessage>,
}

struct Recording {
    next: Arc<Mutex<Option<Record>>>,
    wake: mpsc::SyncSender<()>,
}

impl Recording {
    fn submit(&self, mut record: Record) {
        let mut next = self.next.lock().unwrap_or_else(|e| e.into_inner());
        if record.message.is_none() {
            record.message = next.as_mut().and_then(|old| old.message.take());
        }
        *next = Some(record);
        let _ = self.wake.try_send(());
    }
}

struct State {
    observer: Observer,
    outcome: Option<std::result::Result<Outcome, String>>,
    session: Option<String>,
    /// A failed input write may have delivered any prefix, including a full
    /// line; no later ACP-only snapshot can resolve that ambiguity.
    input_uncertain: bool,
    pending_writes: usize,
    fenced: bool,
    revision: u64,
}

impl State {
    fn changed(&mut self) {
        if let Some(revision) = self.revision.checked_add(1) {
            self.revision = revision;
        } else {
            self.input_uncertain = true;
        }
    }
}

/// The holder's writer queue and the state it acknowledges writes against.
pub(crate) struct Outbound {
    lines: mpsc::Receiver<String>,
    state: Arc<Mutex<State>>,
}

/// An ACP-only observation; it excludes host approvals and output that has
/// not yet reached the observer.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FenceSnapshot {
    pub turn: Turn,
    pub questions: Vec<PermissionQuestion>,
}

/// Holds the relay writer while blocking new holder prompt admission. Dropping
/// the guard unblocks both paths without detaching the current window.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the idle-stop caller is not enabled until shutdown and boundary races are solved")
)]
pub(crate) struct InputFence<'a> {
    acp: &'a HolderAcp,
    _writer: MutexGuard<'a, Box<dyn deck_acp::Writer>>,
    revision: u64,
    snapshot: FenceSnapshot,
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the idle-stop caller is not enabled until shutdown and boundary races are solved")
)]
impl InputFence<'_> {
    pub(crate) fn snapshot(&self) -> &FenceSnapshot {
        &self.snapshot
    }

    /// Detect output observed since the snapshot. A true result cannot prove
    /// that the agent has no unread output and does not authorize stopping.
    pub(crate) fn unchanged(&self) -> bool {
        let Ok(state) = self.acp.state.lock() else { return false };
        state.fenced && !state.input_uncertain && state.pending_writes == 0 && state.revision == self.revision
    }
}

impl Drop for InputFence<'_> {
    fn drop(&mut self) {
        self.acp.state.lock().unwrap_or_else(|error| error.into_inner()).fenced = false;
    }
}

enum Outcome {
    Started(u64),
    Settled,
}

impl HolderAcp {
    /// The client for task `id` under `data`. Its writes queue until
    /// [`HolderAcp::connect`] gives it the relay's writer.
    pub(crate) fn new(data: PathBuf, id: String) -> Result<(Arc<Self>, Outbound)> {
        let store = Store::open(&data)?;
        let run = store.observe(&id)?.launches + 1;
        let tag = format!("{run}-{}", std::process::id());
        let observer = Observer::new(&tag);
        store.write_observation(&id, &Observation { run, turn: None, permissions: observer.permissions() })?;
        let (out, lines) = mpsc::channel();
        let (wake, recorded) = mpsc::sync_channel(1);
        let next = Arc::new(Mutex::new(None));
        let recording = Recording { next: next.clone(), wake };
        std::thread::Builder::new()
            .name("holder-observations".into())
            .spawn(move || record_observations(&data, &id, recorded, next))?;
        let state = Arc::new(Mutex::new(State {
            observer,
            outcome: None,
            session: None,
            input_uncertain: false,
            pending_writes: 0,
            fenced: false,
            revision: 0,
        }));
        Ok((
            Arc::new(Self { state: state.clone(), changed: Condvar::new(), out, recording, run }),
            Outbound { lines, state },
        ))
    }
    /// Write the holder's queued lines to the agent, one whole line per lock.
    pub(crate) fn connect(outbound: Outbound, writer: Arc<Mutex<Box<dyn deck_acp::Writer>>>) {
        std::thread::Builder::new()
            .name("holder-acp-writer".into())
            .spawn(move || {
                for line in outbound.lines {
                    let mut out = writer.lock().unwrap_or_else(|e| e.into_inner());
                    let failed = out.write_all(line.as_bytes()).and_then(|()| out.flush()).is_err();
                    let mut state = outbound.state.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(pending) = state.pending_writes.checked_sub(1) {
                        state.pending_writes = pending;
                    } else {
                        state.input_uncertain = true;
                    }
                    if failed {
                        state.input_uncertain = true;
                        break;
                    }
                }
            })
            .ok();
    }

    /// Try both input fences without blocking on a wedged writer or evicting
    /// the peer. Holder writes already queued make the attempt fail closed.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the idle-stop caller is not enabled until shutdown and boundary races are solved")
    )]
    pub(crate) fn try_fence<'a>(&'a self, relay: &'a Relay) -> Option<InputFence<'a>> {
        let writer = relay.try_writer_for(self)?;
        let mut state = self.state.lock().ok()?;
        if state.fenced || state.input_uncertain || state.pending_writes != 0 {
            return None;
        }
        let (turn, questions) = state.observer.fence_snapshot()?;
        state.fenced = true;
        let revision = state.revision;
        drop(state);
        Some(InputFence { acp: self, _writer: writer, revision, snapshot: FenceSnapshot { turn, questions } })
    }

    /// Prompt the agent and wait, bounded, for the turn to start.
    pub(crate) fn prompt(&self, text: &str, setup: Setup, key: Option<&str>, within: Duration) -> Result<Prompted> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.fenced {
            return Ok(Prompted::Busy);
        }
        state.outcome = None;
        state.session = None;
        let step = match state.observer.prompt_with_key(text, setup, key) {
            Ok(step) => step,
            Err(Refused::Busy) => return Ok(Prompted::Busy),
        };
        self.apply(&mut state, step.write, step.events);
        let deadline = Instant::now() + within;
        loop {
            if let Some(outcome) = state.outcome.take() {
                return match outcome {
                    Ok(Outcome::Started(turn)) => Ok(Prompted::Started { turn, session: state.session.take() }),
                    Ok(Outcome::Settled) => Ok(Prompted::Settled { session: state.session.take() }),
                    Err(error) => anyhow::bail!(error),
                };
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                state.observer.abandon();
                anyhow::bail!("the agent did not start the turn within {} s", within.as_secs());
            }
            state = self.changed.wait_timeout(state, remaining).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    fn apply(&self, state: &mut State, write: Vec<String>, events: Vec<Event>) {
        for line in write {
            if self.out.send(line).is_ok() {
                if let Some(pending) = state.pending_writes.checked_add(1) {
                    state.pending_writes = pending;
                } else {
                    state.input_uncertain = true;
                }
            } else {
                state.input_uncertain = true;
            }
        }
        let mut wake = false;
        let mut record = false;
        for event in events {
            match event {
                Event::Turn(_) | Event::Permissions => record = true,
                Event::Session(session) => state.session = Some(session),
                Event::Prompted(turn) => {
                    state.outcome = Some(Ok(Outcome::Started(turn)));
                    wake = true;
                }
                Event::Settled => {
                    state.outcome = Some(Ok(Outcome::Settled));
                    wake = true;
                }
                Event::Failed(error) => {
                    state.outcome = Some(Err(error));
                    wake = true;
                }
            }
        }
        if record {
            let turn = state.observer.turn().cloned();
            let message =
                if turn.as_ref().is_some_and(|t| t.idle()) { state.observer.message().cloned() } else { None };
            self.recording.submit(Record {
                observation: Observation { run: self.run, turn, permissions: state.observer.permissions() },
                message,
            });
        }
        if wake {
            self.changed.notify_all();
        }
    }
}

impl Tap for HolderAcp {
    fn client(&self, line: &[u8]) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.changed();
        let events = state.observer.client(line);
        self.apply(&mut state, vec![], events);
    }

    fn client_failed(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.input_uncertain = true;
    }

    fn agent(&self, line: &[u8]) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.changed();
        let step = state.observer.agent(line);
        self.apply(&mut state, step.write, step.events);
        step.forward
    }
}

/// Write each turn (and an ended turn's last message, first) to the record,
/// then tell `deckd` to read it. A missing `deckd` is fine: the files are the
/// truth and the next daemon reads them.
fn record_observations(data: &std::path::Path, id: &str, wake: mpsc::Receiver<()>, next: Arc<Mutex<Option<Record>>>) {
    let Ok(store) = Store::open(data) else { return };
    let control = crate::paths::Paths::for_data(data).ok().map(|paths| paths.control());
    for () in wake {
        let record = next.lock().unwrap_or_else(|e| e.into_inner()).take();
        let Some(record) = record else { continue };
        if let Some(message) = record.message
            && let Err(error) = store.write_message(id, &message)
        {
            eprintln!("recording task {id}'s last message: {error:#}");
        }
        if let Err(error) = store.write_observation(id, &record.observation) {
            eprintln!("recording task {id}'s observation: {error:#}");
            continue;
        }
        if let Some(control) = &control {
            let _ = ping(control, id);
        }
    }
}

fn ping(control: &std::path::Path, id: &str) -> Result<()> {
    let mut socket = UnixStream::connect(control).context("connecting to deckd")?;
    socket.set_read_timeout(Some(Duration::from_secs(1)))?;
    socket.set_write_timeout(Some(Duration::from_secs(1)))?;
    wire::write(&mut socket, &serde_json::json!({ "op": "turn_changed", "id": id }))?;
    let _ = wire::read(&mut BufReader::new(socket))?;
    Ok(())
}

#[cfg(test)]
mod fence_tests {
    use super::*;
    use crate::Stream;
    use serde_json::{Value, json};
    use std::io::{self, Read as _};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn line(value: Value) -> Vec<u8> {
        format!("{value}\n").into_bytes()
    }

    fn ready(acp: &HolderAcp) {
        let mut state = acp.state.lock().unwrap();
        let observer = &mut state.observer;
        observer.client(&line(json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{}})));
        observer.agent(&line(json!({"jsonrpc":"2.0", "id":1, "result":{
            "agentCapabilities":{"_meta":{"com.sprawlcommons/construct":{"liveAttach":true}}}
        }})));
        observer.client(&line(json!({"jsonrpc":"2.0", "id":2, "method":"session/load", "params":{"sessionId":"s"}})));
        observer.agent(&line(json!({"jsonrpc":"2.0", "id":2, "result":{"_meta":{
            "com.sprawlcommons/construct":{"turn":{"sessionId":"s", "turn":1, "state":"idle"}}
        }}})));
    }

    fn fixture(
        writer: Option<Box<dyn deck_acp::Writer>>,
        connect: bool,
    ) -> (Arc<HolderAcp>, Relay, UnixStream, PathBuf, Option<Outbound>, Scratch) {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("deck-fence-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("relay.sock");
        let (agent, held) = UnixStream::pair().unwrap();
        agent.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let store = Store::open(&root).unwrap();
        let task = store
            .create("local", crate::Spec { argv: vec!["cat".into()], cwd: root.clone(), ..Default::default() })
            .unwrap();
        let (acp, outbound) = HolderAcp::new(root.clone(), task.id).unwrap();
        ready(&acp);
        let stream = Stream {
            reader: Box::new(BufReader::new(held.try_clone().unwrap())),
            writer: writer.unwrap_or_else(|| Box::new(held)),
        };
        let relay = Relay::tapped(&path, stream, Some(acp.clone())).unwrap();
        let pending = if connect {
            HolderAcp::connect(outbound, relay.writer());
            None
        } else {
            Some(outbound)
        };
        (acp, relay, agent, path, pending, Scratch(root))
    }

    /// The agent can read bytes before the relay has flushed and released
    /// its writer lock. Wait for that write to finish before trying a fence.
    fn wait_for_writer(relay: &Relay) {
        let writer = relay.writer();
        drop(writer.lock().unwrap());
    }

    #[test]
    fn aborted_fence_blocks_both_input_paths_without_evicting_the_peer() {
        let (acp, relay, mut agent, path, _, _scratch) = fixture(None, true);
        let mut peer = UnixStream::connect(&path).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        peer.write_all(b"before\n").unwrap();
        let mut before = [0; 7];
        agent.read_exact(&mut before).unwrap();
        assert_eq!(&before, b"before\n");
        wait_for_writer(&relay);
        let fence = acp.try_fence(&relay).unwrap();
        assert_eq!(fence.snapshot().turn.state, crate::Phase::Idle);
        assert!(fence.snapshot().questions.is_empty());
        assert!(fence.unchanged());
        assert_eq!(
            acp.prompt("queued", Setup { cwd: "/w".into(), session: None }, None, Duration::ZERO).unwrap(),
            Prompted::Busy
        );
        peer.write_all(b"after\n").unwrap();
        agent.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        let mut bytes = [0; 6];
        assert!(matches!(
            agent.read(&mut bytes).unwrap_err().kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ));
        assert!(fence.unchanged());
        drop(fence);
        agent.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        agent.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"after\n");
        agent.write_all(b"same peer\n").unwrap();
        let mut output = [0; 10];
        peer.read_exact(&mut output).unwrap();
        assert_eq!(&output, b"same peer\n");
    }

    #[test]
    fn agent_output_keeps_draining_and_invalidates_the_snapshot() {
        let (acp, relay, mut agent, path, _, _scratch) = fixture(None, true);
        let mut peer = UnixStream::connect(&path).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        // Synchronize accept before testing output delivery.
        peer.write_all(b"ready\n").unwrap();
        let mut ready = [0; 6];
        agent.read_exact(&mut ready).unwrap();
        wait_for_writer(&relay);
        let fence = acp.try_fence(&relay).unwrap();
        assert!(fence.unchanged());
        agent.write_all(b"notice\n").unwrap();
        let mut seen = [0; 7];
        peer.read_exact(&mut seen).unwrap();
        assert_eq!(&seen, b"notice\n");
        assert!(!fence.unchanged());
    }

    #[test]
    fn a_queued_holder_write_prevents_the_fence_even_after_a_timeout() {
        let (acp, relay, _agent, _path, _outbound, _scratch) = fixture(None, false);
        assert!(
            acp.prompt("hello", Setup { cwd: "/w".into(), session: Some("s".into()) }, None, Duration::ZERO).is_err()
        );
        assert!(acp.try_fence(&relay).is_none(), "the queued line can still reach the agent");
    }

    #[test]
    fn an_admitted_client_prompt_blocks_a_snapshot_before_its_turn_notice() {
        let (acp, relay, mut agent, path, _, _scratch) = fixture(None, true);
        let mut peer = UnixStream::connect(&path).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let prompt = line(json!({"jsonrpc":"2.0", "id":"p", "method":"session/prompt", "params":{
            "sessionId":"s", "prompt":[{"type":"text", "text":"go"}]
        }}));
        peer.write_all(&prompt).unwrap();
        let mut received = vec![0; prompt.len()];
        agent.read_exact(&mut received).unwrap();
        assert_eq!(received, prompt);
        wait_for_writer(&relay);
        assert!(acp.try_fence(&relay).is_none(), "the agent has not reported the turn yet");
        let response = line(json!({"jsonrpc":"2.0", "id":"p", "result":{"stopReason":"end_turn"}}));
        agent.write_all(&response).unwrap();
        let mut seen = vec![0; response.len()];
        peer.read_exact(&mut seen).unwrap();
        assert_eq!(seen, response);
        assert!(acp.try_fence(&relay).is_some());
    }

    #[test]
    fn a_busy_writer_refuses_without_disconnecting_the_peer() {
        let (acp, relay, mut agent, path, _, _scratch) = fixture(None, true);
        let mut peer = UnixStream::connect(&path).unwrap();
        let writer = relay.writer();
        let held = writer.lock().unwrap();
        assert!(acp.try_fence(&relay).is_none());
        peer.write_all(b"still connected\n").unwrap();
        drop(held);
        let mut seen = [0; 16];
        agent.read_exact(&mut seen).unwrap();
        assert_eq!(&seen, b"still connected\n");
    }

    #[test]
    fn a_holder_cannot_fence_another_holders_relay() {
        let (first, first_relay, _first_agent, _first_path, _, _first_scratch) = fixture(None, true);
        let (second, second_relay, _second_agent, _second_path, _, _second_scratch) = fixture(None, true);
        assert!(first.try_fence(&second_relay).is_none());
        assert!(second.try_fence(&first_relay).is_none());
        assert!(first.try_fence(&first_relay).is_some());
        assert!(second.try_fence(&second_relay).is_some());
    }

    #[test]
    fn a_closed_holder_queue_forbids_a_snapshot() {
        let (acp, relay, _agent, _path, outbound, _scratch) = fixture(None, false);
        drop(outbound);
        assert!(
            acp.prompt("hello", Setup { cwd: "/w".into(), session: Some("s".into()) }, None, Duration::ZERO).is_err()
        );
        assert!(acp.try_fence(&relay).is_none());
    }

    struct PartialWriter(bool);
    impl io::Write for PartialWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.0 {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "failed"));
            }
            self.0 = true;
            Ok(bytes.len().min(1))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl deck_acp::Writer for PartialWriter {
        fn close(&mut self) {}
    }

    fn wait_uncertain(acp: &HolderAcp) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if acp.state.lock().unwrap().input_uncertain {
                return;
            }
            assert!(Instant::now() < deadline, "write failure was not recorded");
            std::thread::yield_now();
        }
    }

    #[test]
    fn partial_client_answer_forbids_a_snapshot() {
        let (acp, relay, _agent, path, _, _scratch) = fixture(Some(Box::new(PartialWriter(false))), true);
        {
            let mut state = acp.state.lock().unwrap();
            state.observer.agent(&line(json!({"jsonrpc":"2.0", "method":deck_acp::names::TURN_METHOD,
                "params":{"sessionId":"s", "turn":2, "state":"waiting"}})));
            state.observer.agent(&line(json!({"jsonrpc":"2.0", "id":9, "method":"session/request_permission",
                "params":{"toolCall":{"title":"Read"}, "options":[{"optionId":"yes", "name":"Allow", "kind":"allow_once"}]}})));
        }
        let mut peer = UnixStream::connect(&path).unwrap();
        peer.write_all(&line(json!({"jsonrpc":"2.0", "id":9,
            "result":{"outcome":{"outcome":"selected", "optionId":"yes"}}})))
            .unwrap();
        wait_uncertain(&acp);
        assert!(acp.try_fence(&relay).is_none(), "the answer may have reached the agent");
    }

    #[test]
    fn partial_holder_prompt_forbids_a_snapshot() {
        let (acp, relay, _agent, _path, _, _scratch) = fixture(Some(Box::new(PartialWriter(false))), true);
        assert!(
            acp.prompt(
                "hello",
                Setup { cwd: "/w".into(), session: Some("s".into()) },
                None,
                Duration::from_millis(100)
            )
            .is_err()
        );
        wait_uncertain(&acp);
        assert!(acp.try_fence(&relay).is_none(), "the prompt may have reached the agent");
    }
}
