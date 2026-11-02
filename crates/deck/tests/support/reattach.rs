//! Real daemon and agent fixtures for live attachment tests. Every test owns
//! its data, model server and process cleanup; no developer state is reused.

use deck::task::WindowTask;
use deck::ui::session::{Entry, Session, Status};
use deck_acp::{Client, Inbound};
use deck_runtime::{DaemonClient, paths::Paths};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub const WAIT: Duration = Duration::from_secs(15);

pub struct Fixture {
    pub root: PathBuf,
    pub cwd: PathBuf,
    pub argv: Vec<String>,
    pub daemon: DaemonClient,
    pub id: Option<String>,
    server: Option<Child>,
    daemon_child: Child,
}

#[expect(clippy::unwrap_used, reason = "test fixture setup must succeed")]
impl Fixture {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "deck-attach-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let cwd = root.join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd = cwd.canonicalize().unwrap();
        let bin = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("deckd");
        let daemon_child = Command::new(bin)
            .arg("--data")
            .arg(root.join("data"))
            .args(["--idle-ms", "1000"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        eventually(|| DaemonClient::connect(&root.join("data")).is_ok());
        let daemon = DaemonClient::connect(&root.join("data")).unwrap();
        Self { root, cwd, argv: vec![], daemon, id: None, server: None, daemon_child }
    }

    pub fn fake(name: &str) -> Self {
        let mut fixture = Self::new(name);
        let bin = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("deck-fake-agent");
        assert!(bin.is_file(), "build the workspace binaries first");
        fixture.argv = vec![
            "env".into(),
            "DECK_FAKE_DELAY_MS=30".into(),
            format!("DECK_FAKE_STATE={}", fixture.root.join("journal").display()),
            format!("DECK_FAKE_CLOSE_LOG={}", fixture.root.join("closed").display()),
            bin.display().to_string(),
        ];
        fixture
    }

    /// Same scripted endpoint that Construct's scripts/mock.sh launches.
    pub fn construct(name: &str, script: Value) -> Option<Self> {
        let Some(bin) = std::env::var_os("CONSTRUCT_BIN") else {
            eprintln!("skipping real Construct; set CONSTRUCT_BIN and build construct-dev alongside it");
            return None;
        };
        let bin = PathBuf::from(bin).canonicalize().unwrap();
        let dev = std::env::var_os("CONSTRUCT_DEV_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| bin.with_file_name("construct-dev"));
        let mut fixture = Self::new(name);
        let script_path = fixture.root.join("script.json");
        std::fs::write(&script_path, script.to_string()).unwrap();
        let port_file = fixture.root.join("port");
        fixture.server = Some(
            Command::new(dev)
                .args(["serve", "--script"])
                .arg(script_path)
                .args(["--chunk-ms", "30", "--port-file"])
                .arg(&port_file)
                .arg("--dump")
                .arg(fixture.root.join("requests"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        eventually(|| port_file.exists());
        let port = std::fs::read_to_string(port_file).unwrap();
        let home = fixture.root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("config.toml"), "[learn]\nenabled = false\n").unwrap();
        fixture.argv = vec![
            "env".into(),
            "-u".into(),
            "TYPESAFE_API_KEY".into(),
            "-u".into(),
            "CONSTRUCT_MODEL".into(),
            "-u".into(),
            "CONSTRUCT_ROUTER".into(),
            "-u".into(),
            "CONSTRUCT_SANDBOX".into(),
            format!("CONSTRUCT_HOME={}", home.display()),
            format!("CONSTRUCT_BASE_URL=http://127.0.0.1:{}/v1", port.trim()),
            "OPENAI_API_KEY=x".into(),
            bin.display().to_string(),
            "acp".into(),
        ];
        Some(fixture)
    }

    pub fn open(&mut self) -> Attached {
        let (task, connection) = WindowTask::run(&self.root.join("data"), self.argv.clone(), self.cwd.clone()).unwrap();
        if let Some(id) = &self.id {
            assert_eq!(task.id(), id);
        }
        self.id = Some(task.id().into());
        let (client, rx) = connection.unwrap();
        let mut attached = Attached { task, client: Some(client), rx, session: Session::new(), sid: String::new() };
        attached.until(|a| !a.sid.is_empty());
        attached
    }

    pub fn crash_and_restart_daemon(&mut self) {
        self.daemon_child.kill().unwrap();
        self.daemon_child.wait().unwrap();
        let bin = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("deckd");
        self.daemon_child = Command::new(bin)
            .arg("--data")
            .arg(self.root.join("data"))
            .args(["--idle-ms", "1000"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        eventually(|| DaemonClient::connect(&self.root.join("data")).is_ok());
        self.daemon = DaemonClient::connect(&self.root.join("data")).unwrap();
    }

    pub fn pid(&self) -> u32 {
        self.daemon.task(self.id.as_ref().unwrap()).unwrap().pid.unwrap()
    }

    pub fn model_calls(&self) -> usize {
        std::fs::read_dir(self.root.join("requests")).unwrap().count()
    }

    pub fn assert_no_recovery(&self) {
        // Read Construct's own SQLite journal, independently of ACP replay.
        let output = Command::new("python3").arg("-c").arg(
            "import glob,sqlite3,sys; p=glob.glob(sys.argv[1]+'/home/projects/*/journal.sqlite'); assert len(p)==1,p; c=sqlite3.connect(p[0]); assert c.execute(\"select count(*) from events where type='recovered'\").fetchone()[0]==0"
        ).arg(&self.root).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(id) = &self.id {
            let _ = self.daemon.destroy(id);
        }
        if let Some(server) = &mut self.server {
            let _ = server.kill();
            let _ = server.wait();
        }
        let _ = self.daemon_child.kill();
        let _ = self.daemon_child.wait();
        if let Ok(paths) = Paths::for_data(&self.root.join("data")) {
            let _ = std::fs::remove_dir_all(paths.directory);
        }
        if std::thread::panicking() {
            eprintln!("test evidence retained at {}", self.root.display());
        } else {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

pub struct Attached {
    pub task: WindowTask,
    pub client: Option<Client>,
    pub rx: futures::channel::mpsc::UnboundedReceiver<Inbound>,
    pub session: Session,
    pub sid: String,
}

#[expect(clippy::unwrap_used, reason = "test actions must succeed")]
impl Attached {
    pub fn until(&mut self, mut check: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + WAIT;
        while !check(self) {
            match self.rx.try_recv() {
                Ok(event) => {
                    if let Inbound::Ready { session_id, .. } = &event {
                        self.sid = session_id.clone();
                        self.task.record_session(session_id).unwrap();
                    }
                    assert!(!matches!(&event, Inbound::Failed(_)), "{event:?}");
                    self.session.apply(event);
                }
                Err(_) => {
                    assert!(
                        Instant::now() < deadline,
                        "condition timed out: {:?}, {:?}",
                        self.session.status,
                        self.session.entries
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }

    pub fn prompt(&mut self, text: &str) {
        self.client.as_ref().unwrap().prompt(text).unwrap();
        self.session.sent(text);
    }

    pub fn answer(&mut self) -> Value {
        let ask = self.session.answered().unwrap();
        let option = ask.options.iter().find(|o| o.allows()).unwrap();
        self.client.as_ref().unwrap().answer(&ask.request, Some(&option.id)).unwrap();
        ask.request
    }

    pub fn detach(mut self) {
        self.client = None;
        let deadline = Instant::now() + WAIT;
        loop {
            if matches!(self.rx.try_recv(), Ok(Inbound::Failed(_))) {
                break;
            }
            assert!(Instant::now() < deadline, "the old attachment did not close");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn text(&self) -> String {
        self.session
            .entries
            .iter()
            .filter_map(|e| match e {
                Entry::Agent(s) => Some(s.as_str()),
                _ => None,
            })
            .collect()
    }

    pub fn assert_user_once(&self) {
        assert_eq!(self.session.entries.iter().filter(|e| matches!(e, Entry::You(_))).count(), 1);
    }

    pub fn idle(&mut self) {
        self.until(|a| a.session.status == Status::Ready);
    }
}

pub fn eventually(mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !check() {
        assert!(Instant::now() < deadline, "condition timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A line observed while the agent appends it is not JSON yet; it is not the
/// end event, so the barrier keeps polling instead of failing the test.
pub fn journal_finished(root: &std::path::Path) -> bool {
    let Ok(entries) = std::fs::read_dir(root.join("journal")) else { return false };
    entries.flatten().any(|entry| {
        std::fs::read_to_string(entry.path()).is_ok_and(|text| {
            text.lines().any(|line| serde_json::from_str::<Value>(line).is_ok_and(|v| v["event"] == json!("end")))
        })
    })
}
