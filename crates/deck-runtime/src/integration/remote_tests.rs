//! Exercise pinned checkouts with real Git refs. The optional VM test also
//! resolves overlapping changes in the imported checkout before opening ACP.

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
        target: Target::Github { repository: "owner/repo".into(), reference: Reference::Pull { number: 42 } },
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

#[test]
fn a_vm_resolves_conflicting_pins_in_its_own_checkout() {
    if std::env::var_os(crate::vm::IMAGE_ENV).is_none() {
        eprintln!("skipping: {} is not set", crate::vm::IMAGE_ENV);
        return;
    }
    let root = std::env::temp_dir().join(format!("deck-integration-vm-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let repo = root.join("source");
    std::fs::create_dir_all(&repo).unwrap();
    ok(&repo, &["init", "-qb", "main"]);
    std::fs::write(
        repo.join("probe.sh"),
        r#"set -eu
cd /workspace
test "$(git rev-parse HEAD)" = "$1"
test "$(git rev-parse refs/deck-inputs/1)" = "$2"
test "$(git rev-parse refs/deck-inputs/2)" = "$3"
test "$(cat notes)" = 'pinned base'
test ! -e host-only
git merge --no-ff --no-edit "$2"
if git merge --no-ff --no-edit "$3"; then exit 1; fi
test -n "$(git ls-files --unmerged -- notes)"
printf '%s\n' 'change A' 'change B' > notes
git add -- notes
git commit -m 'Keep both changes'
git merge-base --is-ancestor "$1" HEAD
git merge-base --is-ancestor "$2" HEAD
git merge-base --is-ancestor "$3" HEAD
printf '%s\n' 'change A' 'change B' | cmp - notes
exec /usr/local/bin/construct acp
"#,
    )
    .unwrap();
    ok(&repo, &["add", "probe.sh"]);
    let base = write(&repo, "notes", "pinned base\n", "base");
    ok(&repo, &["switch", "-qc", "deck/a"]);
    let a = write(&repo, "notes", "change A\n", "a");
    ok(&repo, &["switch", "-qc", "deck/b", &base]);
    let b = write(&repo, "notes", "change B\n", "b");
    let refs_before = ok(&repo, &["show-ref"]);
    std::fs::write(repo.join("host-only"), "dirty work").unwrap();
    let data = root.join("data");
    let paths = crate::paths::Paths::for_data(&data).unwrap();
    let runtime = crate::Runtime::open(&data, crate::Vm::from_env(&data, paths).unwrap()).unwrap();
    let coordinator = runtime
        .create(crate::Spec {
            argv: vec!["construct".into(), "acp".into()],
            cwd: repo.clone(),
            project: Some("git:fixture".into()),
            kind: crate::TaskKind::Coordinator,
            ..crate::Spec::default()
        })
        .unwrap();
    let plan = Plan {
        target: Target::Github {
            repository: "owner/repo".into(),
            reference: Reference::Base { branch: "main".into() },
        },
        brief: "Integrate".into(),
        base_commit: Some(base.clone()),
        phase: Phase::Ready,
        inputs: vec![
            Input { id: "a".into(), commit: Some(a.clone()) },
            Input { id: "b".into(), commit: Some(b.clone()) },
        ],
    };
    let id = runtime
        .create_with_plan(
            crate::Spec {
                argv: vec!["/bin/sh".into(), "/workspace/probe.sh".into(), base, a, b],
                cwd: repo.clone(),
                project: Some("git:fixture".into()),
                created_by: Some(coordinator),
                github: Some(crate::github::Policy {
                    repository: "owner/repo".into(),
                    base: "main".into(),
                    branches: vec![],
                    credential: crate::github::TOKEN.into(),
                    approvals: Default::default(),
                    identity: crate::github::Identity {
                        name: "Deck Test".into(),
                        email: "test@example.invalid".into(),
                    },
                }),
                ..crate::Spec::default()
            },
            Some(Box::new(plan.clone())),
        )
        .unwrap();
    let import = data.join("tasks").join(&id).join(IMPORT);
    checkout(&import, &plan, &crate::github::task_branch(&id), &format!("file://{}", repo.display()), "file", None)
        .unwrap();
    let observed = (|| -> Result<bool> {
        runtime.start(&id)?;
        let stream = runtime.attach(&id)?;
        let (_client, mut events) = deck_acp::Client::over("/workspace".into(), stream.reader, stream.writer)?;
        let (ready, received) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(event) = futures::executor::block_on(events.recv()) {
                if matches!(event, deck_acp::Inbound::Ready { .. }) {
                    let _ = ready.send(true);
                    break;
                }
            }
        });
        Ok(received.recv_timeout(Duration::from_secs(30)).unwrap_or(false))
    })();
    let diagnostics = if matches!(observed, Ok(true)) {
        String::new()
    } else {
        let dir = data.join("tasks").join(&id);
        ["vmm.log", "console.log"]
            .into_iter()
            .filter_map(|name| std::fs::read_to_string(dir.join(name)).ok().map(|log| format!("{name}:\n{log}")))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let _ = runtime.stop(&id);
    let _ = runtime.destroy(&id);
    let refs_after = ok(&repo, &["show-ref"]);
    std::fs::remove_dir_all(root).unwrap();
    assert!(matches!(observed, Ok(true)), "the guest failed its integration checks: {observed:?}\n{diagnostics}");
    assert_eq!(refs_after, refs_before);
}
