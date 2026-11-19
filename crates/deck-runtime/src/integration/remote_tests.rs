//! Use real Git refs to prove that the prepared checkout has the PR head
//! and the requested commits even after a contributor branch advances.

use super::*;
use crate::integration::{Input, Phase};

fn run(path: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .current_dir(path)
        .args([
            "-c",
            "user.name=Deck Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap()
}

fn ok(path: &Path, args: &[&str]) -> String {
    let output = run(path, args);
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn write(repo: &Path, filename: &str, content: &str, title: &str) -> String {
    std::fs::write(repo.join(filename), content).unwrap();
    ok(repo, &["add", "--", filename]);
    ok(repo, &["commit", "-qm", title]);
    ok(repo, &["rev-parse", "HEAD"])
}

#[test]
fn a_clean_integration_checkout_preserves_an_existing_head_and_uses_the_old_pins() {
    let root = std::env::temp_dir().join(format!("deck-integration-refs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let repo = root.join("source");
    std::fs::create_dir_all(&repo).unwrap();
    ok(&repo, &["init", "-qb", "main"]);
    let main = write(&repo, "notes", "base\n", "base");
    ok(&repo, &["switch", "-qc", "existing-pr"]);
    let head = write(&repo, "reviewed", "prior review\n", "existing PR");
    ok(&repo, &["update-ref", "refs/pull/42/head", &head]);
    ok(&repo, &["switch", "-qc", "deck/a", &main]);
    let a = write(&repo, "notes", "change A\n", "a");
    let newer = write(&repo, "future", "not selected\n", "a advanced");
    ok(&repo, &["switch", "-qc", "deck/b", &main]);
    let b = write(&repo, "notes", "change B\n", "b");
    let refs_before = ok(&repo, &["show-ref"]);
    std::fs::write(repo.join("dirty"), "host only").unwrap();

    let plan = Plan {
        target: Target { repository: "owner/repo".into(), reference: Reference::Pull { number: 42 } },
        brief: "Integrate".into(),
        base_commit: Some(head.clone()),
        phase: Phase::Preparing,
        inputs: vec![
            Input { id: "a".into(), commit: Some(a.clone()) },
            Input { id: "b".into(), commit: Some(b.clone()) },
        ],
    };
    let import = root.join("import");
    checkout(&import, &plan, "deck/integration", &format!("file://{}", repo.display()), "file", None).unwrap();
    assert_eq!(ok(&import, &["rev-parse", "HEAD"]), head);
    assert_eq!(ok(&import, &["branch", "--show-current"]), "deck/integration");
    assert_eq!(ok(&import, &["rev-parse", "refs/deck-inputs/1"]), a);
    assert_eq!(ok(&import, &["rev-parse", "refs/deck-inputs/2"]), b);
    assert!(!import.join("dirty").exists());
    assert_eq!(ok(&import, &["remote"]), "");
    ok(&import, &["merge", "--no-ff", "--no-edit", &a]);
    let conflict = run(&import, &["merge", "--no-ff", "--no-edit", &b]);
    assert_eq!(conflict.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&conflict.stdout).contains("CONFLICT"));
    write(&import, "notes", "change A\nchange B\n", "resolve overlapping changes");
    assert_eq!(std::fs::read_to_string(import.join("notes")).unwrap(), "change A\nchange B\n");
    assert_eq!(std::fs::read_to_string(import.join("reviewed")).unwrap(), "prior review\n");
    for commit in [&head, &a, &b] {
        ok(&import, &["merge-base", "--is-ancestor", commit, "HEAD"]);
    }
    assert_eq!(run(&import, &["merge-base", "--is-ancestor", &newer, "HEAD"]).status.code(), Some(1));
    assert_eq!(ok(&repo, &["show-ref"]), refs_before);
    std::fs::remove_dir_all(root).unwrap();
}
