//! Task records on disk: `<data>/tasks/<id>/events.jsonl`, one lifecycle
//! event per line, and `task.json`, the state they fold into.
//!
//! The design choice: the log is the record and `task.json` is only a
//! convenience for people and listings. Each line is checked against the
//! task's state (see [`crate::lifecycle`]) and fsynced before the call
//! returns; lifecycle events are rare, so each one can afford to be durable.
//! A line cut short by a crash is dropped and truncated away on load, and
//! anything else that fails to parse is an error, never skipped.
//!
//! `lock` is the task's liveness: whatever holds a running task holds an
//! `flock` on `tasks/<id>/lock`. The kernel drops it however the holder
//! dies, so a running record with a free lock has a dead holder. Normal
//! release explicitly unlocks before close: an unrelated concurrent fork
//! can retain the descriptor until exec and must not extend its lifetime.

use crate::lifecycle::{Event, Spec, Task};
use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const LOG: &str = "events.jsonl";
const SNAPSHOT: &str = "task.json";

#[derive(Serialize, Deserialize)]
struct Line {
    at_ms: u64,
    #[serde(flatten)]
    event: Event,
}

/// The tasks under one data root.
pub struct Store {
    tasks: PathBuf,
}

/// A held record lock. The holder releases it explicitly, even if a
/// concurrently forked child has not yet closed its inherited descriptor.
pub struct RecordLock(Option<File>);

impl RecordLock {
    /// Transfer ownership through fork/exec without explicitly unlocking it.
    pub(crate) fn transfer(mut self) -> Option<File> {
        self.0.take()
    }

    pub(crate) fn held(file: File) -> Self {
        Self(Some(file))
    }
}

impl Drop for RecordLock {
    fn drop(&mut self) {
        if let Some(file) = &self.0 {
            let _ = file.unlock();
        }
    }
}

impl Store {
    pub fn open(data: &Path) -> Result<Store> {
        let tasks = data.join("tasks");
        fs::create_dir_all(&tasks).with_context(|| format!("creating {}", tasks.display()))?;
        Ok(Store { tasks })
    }

    /// A new task, recorded as created.
    pub fn create(&self, backend: &str, spec: Spec) -> Result<Task> {
        let first = Event::Created { backend: backend.into(), spec };
        loop {
            let id = new_id();
            match fs::create_dir(self.tasks.join(&id)) {
                Ok(()) => {
                    let task = Task::new(&id, &first)?;
                    self.write(&task, &first, now_ms())?;
                    return Ok(task);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e).context("creating a task directory"),
            }
        }
    }

    /// Record `ev` for `task`, refusing it if it cannot follow the task's
    /// state. `task` is updated only once the line is on disk.
    pub fn append(&self, task: &mut Task, ev: Event) -> Result<()> {
        // The fold and the line share one time, so a recorded time is exactly
        // what a later load reads back.
        let at_ms = now_ms();
        let mut next = task.clone();
        next.apply(at_ms, &ev)?;
        self.write(&next, &ev, at_ms)?;
        *task = next;
        Ok(())
    }

    /// The task `id`, folded from its log.
    pub fn load(&self, id: &str) -> Result<Task> {
        self.read(id, true)
    }

    /// Observe a record while its authority may be appending. Ignore an
    /// incomplete tail, but never truncate another process's in-flight write.
    pub(crate) fn observe(&self, id: &str) -> Result<Task> {
        self.read(id, false)
    }

    fn read(&self, id: &str, repair: bool) -> Result<Task> {
        let path = self.tasks.join(id).join(LOG);
        let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let complete = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        if repair && complete < bytes.len() {
            // A write cut short. Its event never happened as far as anyone
            // was told; drop it so the next append starts on a clean line.
            OpenOptions::new().write(true).open(&path)?.set_len(complete as u64)?;
        }
        let text =
            std::str::from_utf8(&bytes[..complete]).with_context(|| format!("{} is not UTF-8", path.display()))?;
        let mut lines = text.lines().enumerate().map(|(n, l)| {
            serde_json::from_str::<Line>(l).with_context(|| format!("{} line {}", path.display(), n + 1))
        });
        let Some(first) = lines.next() else { bail!("{} is empty", path.display()) };
        let mut task = Task::new(id, &first?.event)?;
        for line in lines {
            let line = line?;
            task.apply(line.at_ms, &line.event)?;
        }
        Ok(task)
    }

    /// Every task with a record. A directory without a log is a create that
    /// never finished and is not a task.
    pub fn list(&self) -> Result<Vec<Task>> {
        let mut out = vec![];
        for entry in fs::read_dir(&self.tasks)? {
            let entry = entry?;
            let Some(id) = entry.file_name().to_str().map(str::to_owned) else { continue };
            if entry.path().join(LOG).exists() {
                out.push(self.load(&id)?);
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    /// When the task's first event was recorded, in Unix milliseconds. The
    /// project backfill uses it as the project's first open time.
    pub fn created_ms(&self, id: &str) -> Result<u64> {
        let path = self.tasks.join(id).join(LOG);
        let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let first = bytes.split(|&byte| byte == b'\n').next().unwrap_or_default();
        let line: Line = serde_json::from_slice(first).with_context(|| format!("parsing {}", path.display()))?;
        Ok(line.at_ms)
    }

    /// Write the task's live model budget where its gateway re-reads it.
    /// `None` is unlimited, and the file is a complete snapshot: writing it
    /// replaces both limits, so a missing field never silently widens one.
    pub fn set_budget(&self, id: &str, requests: Option<u64>, tokens: Option<u64>) -> Result<()> {
        let dir = self.tasks.join(id);
        if !dir.join(LOG).is_file() {
            bail!("task {id} has no record");
        }
        let budget = serde_json::json!({ "requests": requests, "tokens": tokens });
        let tmp = dir.join("budget.json.tmp");
        let mut file = File::create(&tmp)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(&serde_json::to_vec(&budget)?)?;
        file.sync_data()?;
        fs::rename(&tmp, dir.join("budget.json"))?;
        // Make the new name durable; not every platform can sync a directory.
        if let Ok(directory) = File::open(&dir) {
            let _ = directory.sync_all();
        }
        Ok(())
    }

    /// Take `id`'s liveness lock, or `None` if a live holder has it.
    pub fn lock(&self, id: &str) -> Result<Option<RecordLock>> {
        let path = self.tasks.join(id).join("lock");
        let file = OpenOptions::new().create(true).truncate(false).write(true).open(&path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(RecordLock::held(file))),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(e).with_context(|| format!("locking {}", path.display())),
        }
    }

    fn write(&self, task: &Task, ev: &Event, at_ms: u64) -> Result<()> {
        let dir = self.tasks.join(&task.id);
        let mut line = serde_json::to_string(&Line { at_ms, event: ev.clone() })?;
        line.push('\n');
        let mut log = OpenOptions::new().create(true).append(true).open(dir.join(LOG))?;
        log.write_all(line.as_bytes())?;
        log.sync_data()?;
        // The snapshot can lag the log after a crash; the log is what loads.
        let tmp = dir.join(format!("{SNAPSHOT}.tmp"));
        let mut snap = File::create(&tmp)?;
        snap.write_all(serde_json::to_string_pretty(task)?.as_bytes())?;
        snap.sync_data()?;
        fs::rename(&tmp, dir.join(SNAPSHOT))?;
        // Make the new names durable; not every platform can sync a directory.
        if let Ok(d) = File::open(&dir) {
            let _ = d.sync_all();
        }
        Ok(())
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Unique across processes and restarts: time, pid, and a counter for ids
/// made in the same millisecond.
fn new_id() -> String {
    static N: AtomicU32 = AtomicU32::new(0);
    format!("{:011x}-{:x}-{}", now_ms(), std::process::id(), N.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::State;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("deck-record-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn spec() -> Spec {
        Spec { argv: vec!["agent".into()], cwd: "/w".into(), project: None, github: None, model: None }
    }

    #[test]
    fn a_task_reloads_as_it_was_left() {
        let dir = scratch("reload");
        let store = Store::open(&dir).unwrap();
        let mut t = store.create("local", spec()).unwrap();
        store.append(&mut t, Event::Started { pid: Some(9), holder: None }).unwrap();
        store.append(&mut t, Event::Stopped { clean: true, reason: "asked".into() }).unwrap();
        let again = Store::open(&dir).unwrap().load(&t.id).unwrap();
        assert_eq!(again, t);
        let snap: Task =
            serde_json::from_str(&fs::read_to_string(dir.join("tasks").join(&t.id).join(SNAPSHOT)).unwrap()).unwrap();
        assert_eq!(snap, t);
        assert_eq!(store.list().unwrap(), vec![t]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_refused_event_leaves_the_record_alone() {
        let dir = scratch("refused");
        let store = Store::open(&dir).unwrap();
        let mut t = store.create("local", spec()).unwrap();
        assert!(store.append(&mut t, Event::Stopped { clean: true, reason: "never ran".into() }).is_err());
        assert_eq!(t.state, State::Created);
        assert_eq!(store.load(&t.id).unwrap(), t);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_torn_last_line_is_dropped_and_truncated() {
        let dir = scratch("torn");
        let store = Store::open(&dir).unwrap();
        let mut t = store.create("local", spec()).unwrap();
        store.append(&mut t, Event::Started { pid: Some(9), holder: None }).unwrap();
        let log = dir.join("tasks").join(&t.id).join(LOG);
        let whole = fs::read(&log).unwrap();
        OpenOptions::new().append(true).open(&log).unwrap().write_all(br#"{"at_ms":1,"event":"stop"#).unwrap();

        let incomplete = fs::read(&log).unwrap();
        assert_eq!(store.observe(&t.id).unwrap().state, State::Running);
        assert_eq!(fs::read(&log).unwrap(), incomplete, "an observer cannot truncate an active writer");
        let loaded = store.load(&t.id).unwrap();
        assert_eq!(loaded.state, State::Running);
        assert_eq!(fs::read(&log).unwrap(), whole, "the fragment is gone");
        let mut loaded = loaded;
        store.append(&mut loaded, Event::Stopped { clean: false, reason: "x".into() }).unwrap();
        assert_eq!(store.load(&t.id).unwrap(), loaded);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_corrupt_complete_line_is_an_error() {
        let dir = scratch("corrupt");
        let store = Store::open(&dir).unwrap();
        let t = store.create("local", spec()).unwrap();
        let log = dir.join("tasks").join(&t.id).join(LOG);
        OpenOptions::new().append(true).open(&log).unwrap().write_all(b"not json\n").unwrap();
        assert!(store.load(&t.id).is_err());
        assert!(store.list().is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_unfinished_create_is_not_a_task() {
        let dir = scratch("unfinished");
        let store = Store::open(&dir).unwrap();
        fs::create_dir(dir.join("tasks").join("half")).unwrap();
        assert!(store.list().unwrap().is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_lock_is_held_until_its_file_closes() {
        let dir = scratch("lock");
        let store = Store::open(&dir).unwrap();
        let t = store.create("local", spec()).unwrap();
        let held = store.lock(&t.id).unwrap().unwrap();
        assert!(store.lock(&t.id).unwrap().is_none(), "a second holder is refused");
        drop(held);
        assert!(store.lock(&t.id).unwrap().is_some(), "closing releases it");
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn releasing_a_lock_does_not_wait_for_a_forked_child_to_exec() {
        use std::io::Read as _;
        use std::os::fd::AsRawFd as _;
        use std::os::unix::net::UnixStream;

        let dir = scratch("fork-lock");
        let store = Store::open(&dir).unwrap();
        let task = store.create("local", spec()).unwrap();
        let held = store.lock(&task.id).unwrap().unwrap();
        let (mut parent, child) = UnixStream::pair().unwrap();
        let child_fd = child.as_raw_fd();
        // SAFETY: the child only calls async-signal-safe libc functions and
        // _exit. It holds the inherited descriptor until the parent signals
        // it, reproducing Command's fork-to-exec window deterministically.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed: {}", std::io::Error::last_os_error());
        if pid == 0 {
            let mut byte = 1u8;
            // SAFETY: byte lives across these one-byte I/O calls, the socket
            // is inherited, and _exit skips Rust's multithreaded cleanup.
            unsafe {
                libc::write(child_fd, (&byte as *const u8).cast(), 1);
                libc::read(child_fd, (&mut byte as *mut u8).cast(), 1);
                libc::_exit(0);
            }
        }
        let mut ready = [0];
        parent.read_exact(&mut ready).unwrap();
        drop(held);
        let available_while_child_lives = store.lock(&task.id).unwrap().is_some();
        // Reap before asserting so a failing baseline cannot strand a child.
        parent.write_all(&[1]).unwrap();
        let mut status = 0;
        // SAFETY: pid names our child and status is a valid output pointer.
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        assert!(available_while_child_lives, "an unrelated fork must not extend the task lock's lifetime");
        fs::remove_dir_all(dir).unwrap();
    }
}
