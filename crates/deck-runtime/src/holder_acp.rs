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
use crate::observer::{Event, Observer, Refused, Setup};
use crate::permission::Observation;
use crate::record::Store;
use crate::relay::Tap;
use crate::wire;
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, mpsc};
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
    /// The task is not idle.
    Busy,
}

pub(crate) struct HolderAcp {
    state: Mutex<State>,
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
    outcome: Option<std::result::Result<u64, String>>,
    session: Option<String>,
}

impl HolderAcp {
    /// The client for task `id` under `data`. Its writes queue until
    /// [`HolderAcp::connect`] gives it the relay's writer.
    pub(crate) fn new(data: PathBuf, id: String) -> Result<(Arc<Self>, mpsc::Receiver<String>)> {
        let store = Store::open(&data)?;
        // The inherited exclusive task lock proves the previous writer exited.
        // Started is appended only after this holder reports readiness.
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
        let state = State { observer, outcome: None, session: None };
        Ok((Arc::new(Self { state: Mutex::new(state), changed: Condvar::new(), out, recording, run }), lines))
    }

    /// Write the holder's queued lines to the agent, one whole line per lock.
    pub(crate) fn connect(lines: mpsc::Receiver<String>, writer: Arc<Mutex<Box<dyn deck_acp::Writer>>>) {
        std::thread::Builder::new()
            .name("holder-acp-writer".into())
            .spawn(move || {
                for line in lines {
                    let mut out = writer.lock().unwrap_or_else(|e| e.into_inner());
                    if out.write_all(line.as_bytes()).and_then(|()| out.flush()).is_err() {
                        break;
                    }
                }
            })
            .ok();
    }

    /// Prompt the agent and wait, bounded, for the turn to start.
    pub(crate) fn prompt(&self, text: &str, setup: Setup, within: Duration) -> Result<Prompted> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.outcome = None;
        state.session = None;
        let step = match state.observer.prompt(text, setup) {
            Ok(step) => step,
            Err(Refused::Busy) => return Ok(Prompted::Busy),
        };
        self.apply(&mut state, step.write, step.events);
        let deadline = Instant::now() + within;
        loop {
            if let Some(outcome) = state.outcome.take() {
                return match outcome {
                    Ok(turn) => Ok(Prompted::Started { turn, session: state.session.take() }),
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
            let _ = self.out.send(line);
        }
        let mut wake = false;
        let mut record = false;
        for event in events {
            match event {
                Event::Turn(_) | Event::Permissions => record = true,
                Event::Session(session) => state.session = Some(session),
                Event::Prompted(turn) => {
                    state.outcome = Some(Ok(turn));
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
        let events = state.observer.client(line);
        self.apply(&mut state, vec![], events);
    }

    fn agent(&self, line: &[u8]) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
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
