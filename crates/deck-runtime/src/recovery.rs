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
const MAX_FILE: u64 = (MAX_ITEM * MAX_ITEMS) as u64;

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
            Err(TryLockError::WouldBlock) => bail!("interrupted decisions already have a writer"),
            Err(TryLockError::Error(error)) => return Err(error).context("locking interrupted decisions"),
        }
        let items = read(dir)?;
        Ok(Self { dir: dir.into(), lock, items })
    }

    /// The durable snapshot in capture order.
    pub fn items(&self) -> &[Item] {
        &self.items
    }

    /// Record a question exactly once by its caller-supplied stable identity.
    /// Repeating the same capture is harmless; changing it is refused.
    pub fn capture(&mut self, id: &str, question: Question) -> Result<Item> {
        ensure!(!id.is_empty() && id.len() <= 256, "invalid interrupted question id");
        if let Some(existing) = self.items.iter().find(|item| item.id == id) {
            ensure!(existing.question == question, "interrupted question id already has different content");
            return Ok(existing.clone());
        }
        ensure!(self.items.len() < MAX_ITEMS, "too many interrupted questions");
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
        let mut next = self.items.clone();
        next.push(item.clone());
        self.save(next)?;
        Ok(item)
    }

    /// Persist the first valid answer and its fresh key before returning it.
    /// Later answers return the first result without changing it.
    pub fn answer(&mut self, id: &str, answer: Answer) -> Result<Item> {
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
        let mut next = self.items.clone();
        let item = next.iter_mut().find(|item| item.id == id).context("unknown interrupted question")?;
        item.delivery = change(item.delivery.clone())?;
        let result = item.clone();
        self.save(next)?;
        Ok(result)
    }

    fn save(&mut self, next: Vec<Item>) -> Result<()> {
        if let Err(error) = write(&self.dir, &next) {
            // A failed directory sync can follow a successful rename. Reload
            // so a retry cannot overwrite a decision that already landed.
            self.items = read(&self.dir).context("reloading after interrupted decision write failed")?;
            return Err(error);
        }
        self.items = next;
        Ok(())
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = self.lock.unlock();
    }
}

fn read(dir: &Path) -> Result<Vec<Item>> {
    let path = dir.join(FILE);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("opening {}", path.display())),
    };
    ensure!(file.metadata()?.len() <= MAX_FILE, "interrupted decisions file is too large");
    let items: Vec<Item> = serde_json::from_reader(file).with_context(|| format!("reading {}", path.display()))?;
    ensure!(items.len() <= MAX_ITEMS, "too many interrupted questions");
    Ok(items)
}

fn write(dir: &Path, items: &[Item]) -> Result<()> {
    let bytes = serde_json::to_vec(items)?;
    ensure!(bytes.len() as u64 <= MAX_FILE, "interrupted decisions file is too large");
    let temp = dir.join(format!(".{FILE}.{}.tmp", deck_acp::key::new_key()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temp, dir.join(FILE))?;
        File::open(dir)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.context("saving interrupted decisions")
}

#[cfg(test)]
mod tests;
