//! Reviewed-write identities track effective edits rather than JSON layout
//! or pack encoding, and reject targets outside the host's policy.

use super::*;
use crate::github::policy::Policy;

fn settings() -> Settings {
    Settings {
        api: "https://api.github.com".into(),
        policy: Policy::new("owner/repo", "main", vec![]).unwrap(),
        task: "t-1".into(),
        repository_url: "https://github.com/owner/repo".into(),
        token: None,
        scratch: "/unused".into(),
        git: None,
    }
}

fn ask(value: Value) -> Ask {
    serde_json::from_value(value).unwrap()
}

#[test]
fn a_publish_identity_keeps_the_branch_and_commit_across_pack_encodings() {
    let request = ask(json!({ "operation": "publish", "branch": "deck/t-1" }));
    let header = format!("# v2 git bundle\n{} refs/heads/deck/t-1\n\n", "1".repeat(40));
    let first = format!("{header}first pack");
    let second = format!("{header}another pack");
    let fingerprint = review(&request, Operation::Publish, Some(first.as_bytes()), &settings()).unwrap();
    assert_eq!(fingerprint, review(&request, Operation::Publish, Some(second.as_bytes()), &settings()).unwrap());
    let changed = first.replacen(&"1".repeat(40), &"2".repeat(40), 1);
    assert_ne!(fingerprint, review(&request, Operation::Publish, Some(changed.as_bytes()), &settings()).unwrap());
    assert!(fingerprint.unwrap().starts_with("sha256:"));
}

#[test]
fn a_pull_request_identity_tracks_effective_fields_and_defaults() {
    let original = json!({
        "operation": "create_pr", "head": "deck/t-1", "base": "main", "title": "Title", "body": "first",
    });
    let fingerprint = review(&ask(original.clone()), Operation::PullRequest, None, &settings()).unwrap();
    let mut explicit = original.clone();
    explicit["draft"] = json!(true);
    explicit["number"] = json!(9);
    assert_eq!(fingerprint, review(&ask(explicit), Operation::PullRequest, None, &settings()).unwrap());
    for (field, value) in [("title", json!("Other")), ("body", json!("other")), ("draft", json!(false))] {
        let mut changed = original.clone();
        changed[field] = value;
        assert_ne!(fingerprint, review(&ask(changed), Operation::PullRequest, None, &settings()).unwrap(), "{field}");
    }
}

#[test]
fn a_pull_request_update_identity_distinguishes_no_body_edit_from_clearing_it() {
    let original = json!({ "operation": "update_pr", "number": 7, "title": "Title" });
    let fingerprint = review(&ask(original.clone()), Operation::UpdatePullRequest, None, &settings()).unwrap();
    let mut ignored = original.clone();
    ignored["body"] = json!("");
    assert_eq!(fingerprint, review(&ask(ignored), Operation::UpdatePullRequest, None, &settings()).unwrap());
    for (field, value) in [
        ("number", json!(8)),
        ("title", json!("Other")),
        ("body", json!("new")),
        ("empty_body", json!(true)),
        ("draft", json!(false)),
        ("base", json!("main")),
    ] {
        let mut changed = original.clone();
        changed[field] = value;
        assert_ne!(
            fingerprint,
            review(&ask(changed), Operation::UpdatePullRequest, None, &settings()).unwrap(),
            "{field}"
        );
    }
}

#[test]
fn refused_targets_and_malformed_writes_never_get_a_review_identity() {
    for value in [
        json!({ "operation": "publish", "branch": "main" }),
        json!({ "operation": "create_pr", "head": "deck/other", "base": "main", "title": "Title" }),
        json!({ "operation": "create_pr", "head": "deck/t-1", "base": "other", "title": "Title" }),
        json!({ "operation": "create_pr", "head": "deck/t-1", "base": "main", "title": " " }),
        json!({ "operation": "update_pr", "number": 7 }),
        json!({ "operation": "update_pr", "number": 7, "base": "other" }),
        json!({ "operation": "update_pr", "number": 0, "title": "Title" }),
    ] {
        let request = ask(value);
        let operation = crate::github::policy::requested(&request.operation).unwrap();
        assert!(review(&request, operation, None, &settings()).is_err());
    }
    let valid = ask(json!({ "operation": "publish", "branch": "deck/t-1" }));
    assert!(review(&valid, Operation::Publish, Some(b"bad bundle"), &settings()).is_err());
    let read = ask(json!({ "operation": "refs" }));
    assert_eq!(review(&read, Operation::Refs, None, &settings()).unwrap(), None);
}
