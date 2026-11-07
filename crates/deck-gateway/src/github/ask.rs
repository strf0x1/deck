//! One GitHub operation request, parsed against the shared contract
//! (contract 1.2.0 §11.2).
//!
//! The design choice: requests are closed per operation. The body is read as
//! JSON first so the field names it carries can be compared with the ones its
//! operation takes; a field the operation does not name is refused, so an
//! agent that asks for something the gateway does not do learns that instead
//! of being given less.

use super::policy::{Operation, requested};
use serde::{Deserialize, Serialize};

/// One bounded request body. An action asks for exactly one operation; a
/// publish request adds the branch and a bundle on the connection. The fields
/// each operation takes are closed (contract 1.2.0 §11.2): a field the
/// operation does not name is refused, never ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ask {
    pub operation: String,
    #[serde(default)]
    pub branch: String,
    #[serde(default)]
    pub base: String,
    #[serde(default)]
    pub head: String,
    #[serde(default)]
    pub title: String,
    /// The body to set. `update_pr` sets it whenever it is present, even
    /// empty; [`Ask::body_set`] says whether it was.
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub draft: Option<bool>,
    #[serde(default)]
    pub number: Option<u64>,
    /// What the guest says it is publishing, for the card only.
    #[serde(default)]
    pub summary: Option<Summary>,
}

/// A publish's guest-supplied description: the subject of the commit and how
/// many commits are new. It decides nothing; the bundle says which commit
/// lands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Summary {
    pub subject: String,
    pub count: u64,
}

/// The longest summary subject, in characters.
pub const MAX_SUBJECT: usize = 256;

/// Why a request was not parsed: malformed, or an operation outside the set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unparsed {
    Invalid(String),
    NotPermitted(String),
}

impl Ask {
    pub fn parse(bytes: &[u8]) -> Result<(Self, Operation), Unparsed> {
        let value: serde_json::Value = serde_json::from_slice(bytes)
            .map_err(|error| Unparsed::Invalid(format!("the request is not JSON: {error}")))?;
        let name = value.get("operation").and_then(serde_json::Value::as_str).unwrap_or_default();
        let operation = requested(name).map_err(|message| match Operation::refused().contains(&name) {
            true => Unparsed::NotPermitted(message),
            false => Unparsed::Invalid(message),
        })?;
        let ask: Self = serde_json::from_value(value.clone())
            .map_err(|error| Unparsed::Invalid(format!("the request is not a {name} request: {error}")))?;
        let fields: Vec<&str> =
            value.as_object().map(|object| object.keys().map(String::as_str).collect()).unwrap_or_default();
        ask.check(operation, &fields).map_err(Unparsed::Invalid)?;
        Ok((ask, operation))
    }

    /// Whether `update_pr` sets the body.
    pub fn body_set(&self) -> bool {
        self.body.is_some()
    }

    /// The body text, empty when absent.
    pub fn body(&self) -> &str {
        self.body.as_deref().unwrap_or_default()
    }

    /// The fields `operation` takes and the ones it requires, as the
    /// contract's request schemas state them.
    fn check(&self, operation: Operation, fields: &[&str]) -> Result<(), String> {
        let (allowed, required): (&[&str], &[&str]) = match operation {
            Operation::Publish => (&["operation", "branch", "summary"], &["branch"]),
            Operation::Refs => (&["operation"], &[]),
            Operation::PullRequest => {
                (&["operation", "head", "base", "title", "body", "draft"], &["head", "base", "title"])
            }
            Operation::UpdatePullRequest => (&["operation", "number", "title", "body", "base", "draft"], &["number"]),
            Operation::Review => (&["operation", "number"], &["number"]),
        };
        let name = operation.name();
        if let Some(field) = fields.iter().find(|field| !allowed.contains(field)) {
            return Err(format!("{name} takes no {field:?}"));
        }
        if let Some(field) = required.iter().find(|field| !fields.contains(field)) {
            return Err(format!("{name} needs {field:?}"));
        }
        if fields.contains(&"title") && self.title.trim().is_empty() {
            return Err(format!("{name} needs a title that is not blank"));
        }
        if self.number == Some(0) {
            return Err("a pull request number starts at 1".into());
        }
        if operation == Operation::UpdatePullRequest
            && !["title", "body", "base", "draft"].iter().any(|f| fields.contains(f))
        {
            return Err("an update names a title, a body, a base or the draft state".into());
        }
        if let Some(summary) = &self.summary
            && (summary.count == 0 || summary.subject.chars().count() > MAX_SUBJECT)
        {
            return Err(format!("a summary is a subject of at most {MAX_SUBJECT} characters and a count from 1"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ask_carries_its_operation() {
        let (ask, operation) =
            Ask::parse(br#"{"operation":"create_pr","head":"deck/t-1","base":"main","title":"t"}"#).unwrap();
        assert_eq!(operation, Operation::PullRequest);
        assert_eq!(ask.base, "main");
        assert!(
            matches!(Ask::parse(br#"{"operation":"merge"}"#), Err(Unparsed::NotPermitted(m)) if m.contains("not a Deck"))
        );
        assert!(matches!(Ask::parse(br#"{"operation":"rebase"}"#), Err(Unparsed::Invalid(_))));
        assert!(Ask::parse(b"[]").is_err());
    }

    #[test]
    fn each_operation_takes_exactly_its_fields() {
        let invalid = |body: &str| matches!(Ask::parse(body.as_bytes()), Err(Unparsed::Invalid(_)));
        assert!(invalid(r#"{"operation":"publish","branch":"deck/t-1","force":true}"#), "unknown field");
        assert!(invalid(r#"{"operation":"refs","branch":"main"}"#), "a field of another operation");
        assert!(invalid(r#"{"operation":"publish"}"#), "publish needs a branch");
        assert!(invalid(r#"{"operation":"create_pr","base":"main","title":"t"}"#), "create_pr needs a head");
        assert!(invalid(r#"{"operation":"create_pr","head":"h","base":"main","title":"  "}"#));
        assert!(invalid(r#"{"operation":"update_pr","number":7}"#), "an update names an edit");
        assert!(invalid(r#"{"operation":"update_pr","number":7,"body":"","empty_body":true}"#), "no empty_body");
        assert!(invalid(r#"{"operation":"review","number":0}"#));
        assert!(invalid(r#"{"operation":"publish","branch":"b","summary":{"subject":"s","count":0}}"#));
        let long = "x".repeat(MAX_SUBJECT + 1);
        assert!(invalid(&format!(
            r#"{{"operation":"publish","branch":"b","summary":{{"subject":"{long}","count":1}}}}"#
        )));

        let (clear, _) = Ask::parse(br#"{"operation":"update_pr","number":7,"body":""}"#).unwrap();
        assert!(clear.body_set(), "a present empty body clears it");
        let (ready, _) = Ask::parse(br#"{"operation":"update_pr","number":7,"draft":false}"#).unwrap();
        assert!(!ready.body_set());
        let (publish, _) =
            Ask::parse(br#"{"operation":"publish","branch":"deck/t-1","summary":{"subject":"Add b","count":2}}"#)
                .unwrap();
        assert_eq!(publish.summary, Some(Summary { subject: "Add b".into(), count: 2 }));
    }
}
