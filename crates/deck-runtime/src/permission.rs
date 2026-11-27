//! Read-only permission observations. Wire identities and offered choices stay
//! in memory; only bounded previews and subject-free resolutions reach disk.
use crate::Turn;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

const LIMIT: usize = 16;
const SUBJECT_BYTES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct View {
    pub cursor: String,
    pub pending_count: usize,
    pub pending: Vec<Pending>,
    pub resolved: Vec<Resolved>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub observation_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn: Option<u64>,
    pub subject: String,
    pub kind: Kind,
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Allowed,
    Denied,
    Cancelled,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Response,
    Retired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolved {
    pub observation_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn: Option<u64>,
    pub verdict: Verdict,
    pub source: Source,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Observation {
    pub run: u64,
    pub turn: Option<Turn>,
    pub permissions: View,
}

struct Question {
    fingerprint: u64,
    preview: Pending,
    options: HashMap<String, Verdict>,
    /// Reused IDs cannot distinguish a delayed response from a fresh answer.
    ambiguous: bool,
}

pub(crate) struct Questions {
    namespace: String,
    revision: u64,
    sequence: u64,
    open: HashMap<Value, Question>,
    resolved: VecDeque<(u64, Resolved)>,
    clock: Arc<AtomicU64>,
}

impl Questions {
    pub fn new(namespace: String) -> Self {
        Self {
            namespace,
            revision: 0,
            sequence: 0,
            open: HashMap::new(),
            resolved: VecDeque::new(),
            clock: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The observer serializes selected and staged sessions. Share their
    /// resolution order so committing a replay cannot reorder recent answers.
    pub fn stage(&self, namespace: String) -> Self {
        Self {
            namespace,
            revision: 0,
            sequence: 0,
            open: HashMap::new(),
            resolved: VecDeque::new(),
            clock: self.clock.clone(),
        }
    }

    /// A successful same-session load replaces wire questions but retains
    /// subject-free history. The new namespace also fences its cursor.
    pub fn retain_history(&mut self, mut previous: Questions) {
        previous.retire();
        previous.resolved.append(&mut self.resolved);
        previous.resolved.make_contiguous().sort_by_key(|(order, _)| *order);
        while previous.resolved.len() > LIMIT {
            previous.resolved.pop_front();
        }
        self.resolved = previous.resolved;
    }

    pub fn is_empty(&self) -> bool {
        self.open.is_empty()
    }
    pub fn contains(&self, id: &Value) -> bool {
        self.open.contains_key(id)
    }
    pub fn block(&mut self, id: &Value) {
        if let Some(question) = self.open.get_mut(id) {
            question.ambiguous = true;
        }
    }

    pub fn view(&self) -> View {
        let mut pending: Vec<_> = self.open.values().map(|q| (q.preview.observation_id.clone(), q)).collect();
        // The sequence is padded to make lexical order observation order.
        pending.sort_by(|a, b| a.0.cmp(&b.0));
        View {
            cursor: format!("{}:{}", self.namespace, self.revision),
            pending_count: self.open.len(),
            pending: pending.into_iter().take(LIMIT).map(|(_, q)| q.preview.clone()).collect(),
            resolved: self.resolved.iter().map(|(_, resolution)| resolution.clone()).collect(),
        }
    }

    pub fn ask(&mut self, id: Value, params: &Value, turn: Option<u64>, ambiguous: bool) {
        // Conflicting duplicates must not replace the choices already offered.
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        params.hash(&mut hash);
        let fingerprint = hash.finish();
        if let Some(question) = self.open.get_mut(&id) {
            if question.fingerprint != fingerprint && !question.ambiguous {
                question.ambiguous = true;
                self.revision += 1;
            }
            return;
        }
        self.sequence += 1;
        self.revision += 1;
        let tool = &params["toolCall"];
        let text = tool["title"].as_str().unwrap_or("a tool");
        let normalized: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
        let text = normalized.trim();
        let text = if text.is_empty() { "a tool" } else { text };
        let mut end = text.len().min(SUBJECT_BYTES);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let kind = match tool["kind"].as_str() {
            Some("read") => Kind::Read,
            Some("edit") => Kind::Edit,
            Some("delete") => Kind::Delete,
            Some("move") => Kind::Move,
            Some("search") => Kind::Search,
            Some("execute") => Kind::Execute,
            Some("think") => Kind::Think,
            Some("fetch") => Kind::Fetch,
            _ => Kind::Other,
        };
        let mut options = HashMap::new();
        if let Some(offered) = params["options"].as_array().filter(|options| options.len() <= 32) {
            for option in offered {
                if let Some(id) = option["optionId"].as_str() {
                    let verdict = match option["kind"].as_str() {
                        Some("allow_once" | "allow_always") => Verdict::Allowed,
                        Some("reject_once" | "reject_always") => Verdict::Denied,
                        _ => Verdict::Unknown,
                    };
                    options.entry(id.to_owned()).and_modify(|v| *v = Verdict::Unknown).or_insert(verdict);
                }
            }
        }
        self.open.insert(
            id,
            Question {
                fingerprint,
                preview: Pending {
                    observation_id: format!("{}:{:020}", self.namespace, self.sequence),
                    turn,
                    subject: text.get(..end).unwrap_or_default().to_owned(),
                    kind,
                    truncated: end < text.len(),
                },
                options,
                ambiguous,
            },
        );
    }

    pub fn answer(&mut self, id: &Value, result: &Result<Value, String>) -> bool {
        if self.open.get(id).is_none_or(|q| q.ambiguous) {
            return false;
        }
        let Some(question) = self.open.remove(id) else { return false };
        let verdict = match result {
            Ok(value) if value["outcome"]["outcome"] == "cancelled" => Verdict::Cancelled,
            Ok(value) if value["outcome"]["outcome"] == "selected" => value["outcome"]["optionId"]
                .as_str()
                .and_then(|id| question.options.get(id))
                .copied()
                .unwrap_or(Verdict::Unknown),
            _ => Verdict::Unknown,
        };
        self.resolve(question.preview, verdict, Source::Response);
        true
    }

    pub fn retire(&mut self) {
        let questions = std::mem::take(&mut self.open);
        let mut questions: Vec<_> = questions.into_values().collect();
        questions.sort_by(|a, b| a.preview.observation_id.cmp(&b.preview.observation_id));
        for question in questions {
            self.resolve(question.preview, Verdict::Unknown, Source::Retired);
        }
    }

    fn resolve(&mut self, question: Pending, verdict: Verdict, source: Source) {
        self.revision += 1;
        let order = self.clock.fetch_add(1, Ordering::Relaxed);
        self.resolved.push_back((
            order,
            Resolved { observation_id: question.observation_id, turn: question.turn, verdict, source },
        ));
        if self.resolved.len() > LIMIT {
            self.resolved.pop_front();
        }
    }
}
