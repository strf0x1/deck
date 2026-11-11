//! Model admission retains the exact persisted grant. Launch models and
//! admitted once requests have no revocable lease; a task/project queue reply
//! must resolve to its durable rule before any credential is fetched.

use super::Model;
use crate::control::{Answer, Ask};
use crate::grants::Permit;
use crate::http::{self, Request};
use std::io;

pub(super) struct Denial {
    id: Option<String>,
    retryable: bool,
    message: String,
}

impl Denial {
    fn new(id: Option<String>, retryable: bool, message: String) -> Self {
        Self { id, retryable, message }
    }

    pub fn write(&self, guest: &mut impl io::Write) -> io::Result<()> {
        http::write_denied(guest, self.id.as_deref(), self.retryable, &self.message)
    }
}

impl Model {
    pub(super) fn admit_model(&self, request: &Request, model: &str) -> Result<Option<Permit>, Denial> {
        if self.models.allows(model) {
            return Ok(None);
        }
        if let Some(permit) = self.grants.model(model, self.secret.as_deref(), None) {
            return Ok(Some(permit));
        }
        let Some(control) = &self.control else {
            return Err(Denial::new(None, false, format!("model {model:?} is not allowed for this task")));
        };
        let ask = Ask {
            operation: "chat_completions".into(),
            destination: model.into(),
            credential: self.secret.clone(),
            method: Some(request.method.clone()),
            path: Some(request.target.clone()),
            size: Some(request.body.len() as u64),
        };
        match control.ask(&ask) {
            Answer::Allowed(scope) if scope == "once" => Ok(None),
            Answer::Allowed(scope) if matches!(scope.as_str(), "timed" | "task" | "project") => self
                .grants
                .model(model, self.secret.as_deref(), Some(&scope))
                .map(Some)
                .ok_or_else(|| Denial::new(None, false, "the authorizing model grant is no longer present".into())),
            Answer::Allowed(_) => Err(Denial::new(None, false, "the queue returned an unknown scope".into())),
            Answer::Pending(id) => {
                self.status.observe(&id, model, "pending", None, None);
                Err(Denial::new(
                    Some(id),
                    true,
                    format!("model {model:?} needs the user's approval; retry after they decide"),
                ))
            }
            Answer::Denied(id, state) => {
                self.status.observe(&id, model, &state, None, None);
                Err(Denial::new(
                    (!id.is_empty()).then_some(id),
                    false,
                    format!("model {model:?} was denied by the user's boundary policy"),
                ))
            }
            Answer::Unavailable(why) => {
                Err(Denial::new(None, true, format!("model {model:?} needs approval, but {why}")))
            }
        }
    }
}
