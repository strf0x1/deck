//! B1 startup safety after hard daemon death. A released task lock proves
//! deckd died, but its child lifeline may not have run yet. Confirm the old
//! group has no executing members before Runtime records a reusable stop.

use crate::{State, record::Store};
use anyhow::{Context as _, Result, bail};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

pub(crate) fn wait_for_children(data: &Path) -> Result<()> {
    let store = Store::open(data)?;
    for task in store.list()? {
        if task.state != State::Running {
            continue;
        }
        let Some(_lock) = store.lock(&task.id)? else { continue };
        let Some(pid) = task.pid else { continue };
        let deadline = Instant::now() + Duration::from_secs(3);
        while group_running(pid)? {
            if Instant::now() >= deadline {
                bail!("task {}: previous process group {pid} has not stopped; refusing to reuse it", task.id);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    Ok(())
}

fn group_running(pid: u32) -> Result<bool> {
    let group = i32::try_from(pid).context("recorded pid is out of range")?;
    // SAFETY: signal zero queries process-group existence, without sending a signal.
    if unsafe { libc::kill(-group, 0) } == -1 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(false);
        }
        return Err(error).context("checking previous task process group");
    }
    // Orphan zombies may remain under container PID 1. They cannot execute
    // or hold files. ps is available on both supported host platforms.
    let output = Command::new("ps").args(["-axo", "pgid=,stat="]).output().context("checking task group members")?;
    if !output.status.success() {
        bail!("could not confirm that process group {pid} stopped");
    }
    let text = std::str::from_utf8(&output.stdout)?;
    Ok(text.lines().any(|line| {
        let mut fields = line.split_whitespace();
        fields.next().and_then(|s| s.parse::<u32>().ok()) == Some(pid)
            && fields.next().is_some_and(|state| !state.starts_with('Z'))
    }))
}
