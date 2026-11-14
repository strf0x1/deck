//! Test cleanup for detached task holders. A holder outlives `deckd` by
//! design (docs/deckd.md), so a test that fails before stopping its task, or
//! whose stop does not complete, would leave the holder and its agent running
//! and holding the test's stderr open. A [`Holders`] guard finds, on drop
//! (panicking or not), every `deckd --holder` whose command line names its
//! data root, so it can never touch a holder of another data root.
//!
//! Shared by deck-runtime's and deck's integration tests through `#[path]`;
//! it uses only `ps` and `kill`, which both supported hosts have.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Kills the holders of `data` and their agents when dropped.
pub struct Holders {
    data: PathBuf,
}

impl Holders {
    pub fn new(data: &Path) -> Self {
        Self { data: data.to_path_buf() }
    }
}

impl Drop for Holders {
    fn drop(&mut self) {
        kill(&self.data);
    }
}

struct Row {
    pid: u32,
    ppid: u32,
    pgid: u32,
    zombie: bool,
    command: String,
}

fn processes() -> Vec<Row> {
    let Ok(output) = Command::new("ps").args(["-axww", "-o", "pid=,ppid=,pgid=,stat=,command="]).output() else {
        return vec![];
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let ppid = fields.next()?.parse().ok()?;
            let pgid = fields.next()?.parse().ok()?;
            let zombie = fields.next()?.starts_with('Z');
            Some(Row { pid, ppid, pgid, zombie, command: fields.collect::<Vec<_>>().join(" ") })
        })
        .collect()
}

/// The holders whose `--holder` argument is `data`, as given or canonical.
pub fn holders(data: &Path) -> Vec<u32> {
    let mut names = vec![data.display().to_string()];
    if let Ok(canonical) = data.canonicalize() {
        names.push(canonical.display().to_string());
    }
    processes()
        .into_iter()
        .filter(|row| !row.zombie && names.iter().any(|name| row.command.contains(&format!("--holder {name} "))))
        .map(|row| row.pid)
        .collect()
}

/// SIGKILL each holder of `data` and its agent's process group, then wait
/// until none of them can still execute. A holder leads its own session, and
/// its child lifeline leads the agent's group, so the groups are exactly
/// these processes; killing the holder alone would also end the agent, via
/// the lifeline, but not before this returns.
pub fn kill(data: &Path) {
    let holders = holders(data);
    if holders.is_empty() {
        return;
    }
    let rows = processes();
    let mut groups = holders.clone();
    groups.extend(rows.iter().filter(|row| holders.contains(&row.ppid)).map(|row| row.pgid));
    groups.sort_unstable();
    groups.dedup();
    for group in &groups {
        let _ = Command::new("kill").args(["-KILL", "--", &format!("-{group}")]).status();
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !processes().iter().any(|row| !row.zombie && groups.contains(&row.pgid)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    eprintln!("holders of {} did not exit after SIGKILL", data.display());
}
