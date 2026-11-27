//! Durable questions interrupted by a stopped task.
//!
//! A decision here is context for a resumed agent, not a gateway grant and
//! not an answer to the old ACP request. One writer owns a task's recovery
//! file. A prompt key is persisted with the first answer, before delivery.

use crate::approvals::{Approval, Decision, State as ApprovalState};
use anyhow::{Context as _, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

const FILE: &str = "interrupted-decisions.json";
const MAX_ITEM: usize = 32 * 1024;
const MAX_ITEMS: usize = 128;
const MAX_FILE: u64 = (MAX_ITEM * MAX_ITEMS * 2 + 64 * 1024) as u64;

/// Another owner currently holds the recovery transaction lock.
#[derive(Debug)]
pub(crate) struct Busy;

impl std::fmt::Display for Busy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("interrupted decisions already have a writer")
    }
}

impl std::error::Error for Busy {}

/// An option offered by the agent when it asked its original question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptionItem {
    pub id: String,
    pub name: String,
    pub kind: String,
}

/// A snapshot of a question whose original waiter will no longer exist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Question {
    Agent {
        title: String,
        options: Vec<OptionItem>,
    },
    /// The approval is descriptive. Recording or answering it grants nothing.
    Boundary {
        approval: Box<Approval>,
    },
}
/// A question a caller captured before the agent that owned it went away.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Captured {
    pub id: String,
    pub question: Question,
}

/// A task and the unique, durable identity of one holder launch. A PID alone
/// is not a run identity because the operating system can reuse it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunIdentity {
    pub task: String,
    pub run: String,
}

/// Evidence supplied by the caller after it confirmed the matching holder and
/// its writers have exited. Construct this only after backend reconciliation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmedExit {
    pub run: RunIdentity,
}

/// Evidence supplied by the caller that the matching holder is alive and its
/// stop has been cancelled, with no stop request still able to take effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmedAbort {
    pub run: RunIdentity,
}

/// A staged stop is not visible as interrupted decisions until promotion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopPhase {
    Prepared,
    Stopping,
    Committed,
    Aborted,
}

/// The durable transaction for the most recent staged stop of a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedStop {
    pub id: String,
    pub run: RunIdentity,
    pub questions: Vec<Captured>,
    pub phase: StopPhase,
}

/// A user's choice. A boundary decision stored here is not an authorization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Answer {
    Agent {
        option_id: String,
    },
    /// The user cancelled the agent's question instead of choosing an option.
    AgentCancelled,
    Boundary {
        decision: Decision,
    },
}

/// Progress of delivering the answer as a new prompt to the resumed agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Delivery {
    Pending,
    Answered {
        answer: Answer,
        key: String,
    },
    /// Written before attempting delivery. After a crash it is uncertain.
    Sending {
        answer: Answer,
        key: String,
    },
    Uncertain {
        answer: Answer,
        key: String,
    },
    Delivered {
        answer: Answer,
        key: String,
    },
    /// The user reviewed an uncertain delivery and accepted that the answer
    /// was not confirmed. It no longer blocks a resume.
    Reviewed {
        answer: Answer,
        key: String,
    },
}

/// Whether the decision no longer prevents an ordinary task resume.
pub fn resolved(delivery: &Delivery) -> bool {
    matches!(delivery, Delivery::Delivered { .. } | Delivery::Reviewed { .. })
}

/// One stable interrupted question and its first answer, if any.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    /// Stable capture identity supplied by the holder or approval queue.
    pub id: String,
    pub question: Question,
    pub delivery: Delivery,
}

/// Exclusive writer for one existing task directory's recovery file.
pub struct Store {
    dir: PathBuf,
    lock: File,
    items: Vec<Item>,
    stage: Option<StagedStop>,
}

impl Store {
    /// Open an existing task directory. A second writer is rejected.
    pub fn open(dir: &Path) -> Result<Self> {
        let lock_path = dir.join("interrupted-decisions.lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
            .with_context(|| format!("opening {}", lock_path.display()))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(Busy.into()),
            Err(TryLockError::Error(error)) => return Err(error).context("locking interrupted decisions"),
        }
        let (items, stage) = read(dir)?;
        Ok(Self { dir: dir.into(), lock, items, stage })
    }

    /// The durable snapshot in capture order.
    pub fn items(&self) -> &[Item] {
        &self.items
    }

    /// Read the staged transaction, including its terminal receipt. Its
    /// questions are not user-facing recovery items until it is committed.
    pub fn staged_stop(&self) -> Option<&StagedStop> {
        self.stage.as_ref()
    }

    /// Durably prepare a snapshot while the caller holds the holder and
    /// boundary fences. The caller must keep those fences until it either
    /// records stop intent or aborts. IDs must be stable across retries.
    pub fn prepare_stop(&mut self, id: &str, run: RunIdentity, questions: Vec<Captured>) -> Result<StagedStop> {
        valid_id(id, "transaction")?;
        valid_id(&run.run, "run")?;
        valid_id(&run.task, "task")?;
        ensure!(self.dir.file_name().and_then(|name| name.to_str()) == Some(&run.task), "run names another task");
        self.ensure_current_run(&run)?;
        if let Some(existing) = &self.stage {
            if existing.id == id {
                ensure!(existing.run == run && existing.questions == questions, "transaction id has different content");
                return Ok(existing.clone());
            }
            ensure!(matches!(existing.phase, StopPhase::Committed | StopPhase::Aborted), "another stop is staged");
        }
        ensure!(self.items.len().saturating_add(questions.len()) <= MAX_ITEMS, "too many interrupted questions");
        for (index, question) in questions.iter().enumerate() {
            let _ = checked_item(&question.id, question.question.clone())?;
            ensure!(!questions[..index].iter().any(|other| other.id == question.id), "duplicate staged question id");
            ensure!(!self.items.iter().any(|item| item.id == question.id), "staged question id already exists");
            if let Question::Boundary { approval } = &question.question {
                ensure!(approval.task == run.task, "staged approval belongs to another task");
            }
        }
        check_expiry(&questions)?;
        let stage = StagedStop { id: id.into(), run, questions, phase: StopPhase::Prepared };
        self.save_stage(stage.clone())?;
        Ok(stage)
    }

    /// Persist stop intent before the caller permits the holder to stop.
    /// Expired host approvals must be abandoned and captured afresh.
    pub fn begin_stop(&mut self, id: &str, run: &RunIdentity) -> Result<StagedStop> {
        self.ensure_current_run(run)?;
        let mut stage = self.match_stage(id, run)?.clone();
        match stage.phase {
            StopPhase::Prepared => {
                check_expiry(&stage.questions)?;
                stage.phase = StopPhase::Stopping;
                self.save_stage(stage.clone())?;
                Ok(stage)
            }
            StopPhase::Stopping => Ok(stage),
            _ => bail!("stop transaction is already terminal"),
        }
    }

    /// Promote all staged questions only after the caller confirmed the
    /// matching holder and its backend writers have exited. Promotion is a
    /// single atomic replacement; a retry after a crash preserves answers.
    pub fn promote_stop(&mut self, id: &str, confirmation: &ConfirmedExit) -> Result<Vec<Item>> {
        let mut stage = self.match_stage(id, &confirmation.run)?.clone();
        ensure!(matches!(stage.phase, StopPhase::Stopping | StopPhase::Committed), "stop intent was not recorded");
        if stage.phase == StopPhase::Stopping {
            self.ensure_current_run(&confirmation.run)?;
        }
        let mut next = self.items.clone();
        let mut promoted = Vec::with_capacity(stage.questions.len());
        for question in &stage.questions {
            if let Some(existing) = next.iter().find(|item| item.id == question.id) {
                ensure!(existing.question == question.question, "staged question id has different content");
                promoted.push(existing.clone());
            } else {
                ensure!(stage.phase == StopPhase::Stopping, "committed question is missing");
                let item = checked_item(&question.id, question.question.clone())?;
                next.push(item.clone());
                promoted.push(item);
            }
        }
        ensure!(next.len() <= MAX_ITEMS, "too many interrupted questions");
        if stage.phase == StopPhase::Stopping {
            stage.phase = StopPhase::Committed;
            self.save_snapshot(next, Some(stage))?;
        }
        Ok(promoted)
    }

    fn ensure_current_run(&self, run: &RunIdentity) -> Result<()> {
        let recorded = crate::record::read_run_identity(&self.dir, &run.task)?;
        ensure!(recorded.as_ref() == Some(run), "run identity is missing or no longer current");
        Ok(())
    }

    /// Abort a prepared stop before stop intent was recorded.
    pub fn abort_prepared_stop(&mut self, id: &str, run: &RunIdentity) -> Result<StagedStop> {
        let stage = self.match_stage(id, run)?.clone();
        match stage.phase {
            StopPhase::Prepared | StopPhase::Aborted => self.abort_stage(stage),
            _ => bail!("stop intent was recorded; a confirmed cancellation is required"),
        }
    }

    /// Abort after stop intent only when the caller has confirmed the same
    /// holder is alive and that no stop request can still take effect.
    pub fn abort_stopping(&mut self, id: &str, confirmation: &ConfirmedAbort) -> Result<StagedStop> {
        let stage = self.match_stage(id, &confirmation.run)?.clone();
        ensure!(matches!(stage.phase, StopPhase::Stopping | StopPhase::Aborted), "stop is not awaiting cancellation");
        ensure!(
            !stage.questions.iter().any(|question| self.items.iter().any(|item| item.id == question.id)),
            "staged questions have already been promoted"
        );
        self.abort_stage(stage)
    }

    fn abort_stage(&mut self, mut stage: StagedStop) -> Result<StagedStop> {
        if stage.phase != StopPhase::Aborted {
            stage.phase = StopPhase::Aborted;
            self.save_stage(stage.clone())?;
        }
        Ok(stage)
    }

    fn match_stage(&self, id: &str, run: &RunIdentity) -> Result<&StagedStop> {
        let stage = self.stage.as_ref().context("no staged stop")?;
        ensure!(stage.id == id && &stage.run == run, "stop transaction or run does not match");
        Ok(stage)
    }

    fn ensure_unstaged(&self) -> Result<()> {
        if self.stage.as_ref().is_some_and(|stage| matches!(stage.phase, StopPhase::Prepared | StopPhase::Stopping)) {
            bail!("a stop transaction is still in progress");
        }
        Ok(())
    }

    fn save_stage(&mut self, stage: StagedStop) -> Result<()> {
        self.save_snapshot(self.items.clone(), Some(stage))
    }

    /// Record a question exactly once by its caller-supplied stable identity.
    /// Repeating the same capture is harmless; changing it is refused.
    pub fn capture(&mut self, id: &str, question: Question) -> Result<Item> {
        self.ensure_unstaged()?;
        if let Some(stage) = &self.stage
            && stage.phase == StopPhase::Aborted
            && stage.questions.iter().any(|question| question.id == id)
        {
            bail!("question id belongs to an aborted stop");
        }
        if let Some(existing) = self.items.iter().find(|item| item.id == id) {
            ensure!(existing.question == question, "interrupted question id already has different content");
            return Ok(existing.clone());
        }
        ensure!(self.items.len() < MAX_ITEMS, "too many interrupted questions");
        let item = checked_item(id, question)?;
        let mut next = self.items.clone();
        next.push(item.clone());
        self.save(next)?;
        Ok(item)
    }

    /// Persist the first valid answer and its fresh key before returning it.
    /// Later answers return the first result without changing it.
    pub fn answer(&mut self, id: &str, answer: Answer) -> Result<Item> {
        self.ensure_unstaged()?;
        let mut next = self.items.clone();
        let item = next.iter_mut().find(|item| item.id == id).context("unknown interrupted question")?;
        if !matches!(item.delivery, Delivery::Pending) {
            return Ok(item.clone());
        }
        match (&item.question, &answer) {
            (Question::Agent { options, .. }, Answer::Agent { option_id }) => {
                ensure!(options.iter().any(|option| &option.id == option_id), "unknown agent option");
            }
            (Question::Agent { .. }, Answer::AgentCancelled) => {}
            (Question::Boundary { .. }, Answer::Boundary { .. }) => {}
            _ => bail!("answer does not match interrupted question"),
        }
        item.delivery = Delivery::Answered { answer, key: deck_acp::key::new_key() };
        let result = item.clone();
        ensure!(serde_json::to_vec(&result)?.len() <= MAX_ITEM, "interrupted answer is too large");
        self.save(next)?;
        Ok(result)
    }

    /// Mark a delivery attempt before any prompt is sent. A `Sending` state
    /// left by a crash is ambiguous and requires capability-aware recovery.
    pub fn start_delivery(&mut self, id: &str) -> Result<Item> {
        self.transition(id, |state| match state {
            Delivery::Answered { answer, key }
            | Delivery::Sending { answer, key }
            | Delivery::Uncertain { answer, key } => Ok(Delivery::Sending { answer, key }),
            _ => bail!("the answer is not ready for delivery"),
        })
    }
    /// Mark an attempted delivery as accepted by the resumed agent.
    pub fn delivered(&mut self, id: &str) -> Result<Item> {
        self.transition(id, |state| match state {
            Delivery::Sending { answer, key } => Ok(Delivery::Delivered { answer, key }),
            _ => bail!("the answer is not being sent"),
        })
    }

    /// Record that a delivery attempt may or may not have reached the agent.
    pub fn uncertain(&mut self, id: &str) -> Result<Item> {
        self.transition(id, |state| match state {
            Delivery::Sending { answer, key } => Ok(Delivery::Uncertain { answer, key }),
            _ => bail!("the answer is not being sent"),
        })
    }

    /// Restore an answer to ready when the holder confirms it sent nothing.
    pub fn not_sent(&mut self, id: &str) -> Result<Item> {
        self.transition(id, |state| match state {
            Delivery::Sending { answer, key } => Ok(Delivery::Answered { answer, key }),
            _ => bail!("the answer is not being sent"),
        })
    }
    /// Accept an uncertain delivery as reviewed. The answer is not resent
    /// and no authority is granted by this transition.
    pub fn review(&mut self, id: &str) -> Result<Item> {
        self.transition(id, |state| match state {
            Delivery::Sending { answer, key } | Delivery::Uncertain { answer, key } => {
                Ok(Delivery::Reviewed { answer, key })
            }
            _ => bail!("there is no uncertain answer to review"),
        })
    }

    fn transition(&mut self, id: &str, change: impl FnOnce(Delivery) -> Result<Delivery>) -> Result<Item> {
        self.ensure_unstaged()?;
        let mut next = self.items.clone();
        let item = next.iter_mut().find(|item| item.id == id).context("unknown interrupted question")?;
        item.delivery = change(item.delivery.clone())?;
        let result = item.clone();
        self.save(next)?;
        Ok(result)
    }

    fn save(&mut self, next: Vec<Item>) -> Result<()> {
        self.save_snapshot(next, self.stage.clone())
    }

    fn save_snapshot(&mut self, next: Vec<Item>, stage: Option<StagedStop>) -> Result<()> {
        if let Err(error) = write(&self.dir, &next, stage.as_ref()) {
            // A failed directory sync can follow a successful rename. Reload
            // so a retry cannot overwrite a decision that already landed.
            (self.items, self.stage) = read(&self.dir).context("reloading after interrupted decision write failed")?;
            return Err(error);
        }
        self.items = next;
        self.stage = stage;
        Ok(())
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = self.lock.unlock();
    }
}

fn valid_id(id: &str, name: &str) -> Result<()> {
    ensure!(!id.is_empty() && id.len() <= 256, "invalid {name} id");
    Ok(())
}

fn checked_item(id: &str, question: Question) -> Result<Item> {
    valid_id(id, "interrupted question")?;
    match &question {
        Question::Agent { options, .. } => {
            ensure!(!options.is_empty(), "an agent question needs an option");
            ensure!(options.iter().all(|option| !option.id.is_empty()), "an agent option needs an id");
            for (index, option) in options.iter().enumerate() {
                ensure!(!options[..index].iter().any(|other| other.id == option.id), "duplicate agent option id");
            }
        }
        Question::Boundary { approval } => {
            ensure!(approval.state == ApprovalState::Pending, "only a pending approval can be interrupted");
        }
    }
    let item = Item { id: id.into(), question, delivery: Delivery::Pending };
    ensure!(serde_json::to_vec(&item)?.len() <= MAX_ITEM, "interrupted question is too large");
    Ok(item)
}

fn check_expiry(questions: &[Captured]) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("host clock precedes the Unix epoch")?
        .as_millis();
    for question in questions {
        if let Question::Boundary { approval } = &question.question {
            ensure!(u128::from(approval.expires_ms) > now, "staged approval has expired");
        }
    }
    Ok(())
}

fn validate_stage(dir: &Path, stage: &StagedStop, items: &[Item]) -> Result<()> {
    valid_id(&stage.id, "transaction")?;
    valid_id(&stage.run.task, "task")?;
    valid_id(&stage.run.run, "run")?;
    ensure!(dir.file_name().and_then(|name| name.to_str()) == Some(&stage.run.task), "staged stop names another task");
    ensure!(stage.questions.len() <= MAX_ITEMS, "too many staged questions");
    for (index, question) in stage.questions.iter().enumerate() {
        let _ = checked_item(&question.id, question.question.clone())?;
        ensure!(!stage.questions[..index].iter().any(|other| other.id == question.id), "duplicate staged question id");
        if let Question::Boundary { approval } = &question.question {
            ensure!(approval.task == stage.run.task, "staged approval belongs to another task");
        }
        let existing = items.iter().find(|item| item.id == question.id);
        if stage.phase == StopPhase::Committed {
            ensure!(
                existing.is_some_and(|item| item.question == question.question),
                "committed question is missing or changed"
            );
        } else {
            ensure!(existing.is_none(), "uncommitted question is already visible");
        }
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum Snapshot {
    Legacy(Vec<Item>),
    Staged { items: Vec<Item>, stop: StagedStop },
}

fn read(dir: &Path) -> Result<(Vec<Item>, Option<StagedStop>)> {
    let path = dir.join(FILE);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), None)),
        Err(error) => return Err(error).with_context(|| format!("opening {}", path.display())),
    };
    ensure!(file.metadata()?.len() <= MAX_FILE, "interrupted decisions file is too large");
    let snapshot: Snapshot = serde_json::from_reader(file).with_context(|| format!("reading {}", path.display()))?;
    let (items, stage) = match snapshot {
        Snapshot::Legacy(items) => (items, None),
        Snapshot::Staged { items, stop } => (items, Some(stop)),
    };
    ensure!(items.len() <= MAX_ITEMS, "too many interrupted questions");
    if let Some(stage) = &stage {
        validate_stage(dir, stage, &items)?;
    }
    Ok((items, stage))
}

fn write(dir: &Path, items: &[Item], stage: Option<&StagedStop>) -> Result<()> {
    let bytes = match stage {
        Some(stop) => serde_json::to_vec(&Snapshot::Staged { items: items.to_vec(), stop: stop.clone() })?,
        None => serde_json::to_vec(items)?,
    };
    write_atomic(dir, FILE, &bytes).context("saving interrupted decisions")
}

fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    ensure!(bytes.len() as u64 <= MAX_FILE, "interrupted decisions file is too large");
    let temp = dir.join(format!(".{name}.{}.tmp", deck_acp::key::new_key()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, dir.join(name))?;
        File::open(dir)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests;
