//! `deck daemon status|stop|restart`: the one deckd serving a data root.
//!
//! The design choice: the commands never guess at a process. They ask the
//! daemon itself through the compatibility handshake (pid, version, protocol,
//! and whether its binary was replaced) and stop it by asking it to exit, so
//! its task holders keep their agents; the next daemon adopts them.

use crate::{DaemonClient, Hello};
use anyhow::{Context as _, Result, bail};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const USAGE: &str = "deck daemon status | stop | restart";

/// How long a stopped daemon may take to exit.
const EXIT: Duration = Duration::from_secs(10);

pub fn run(data: &Path, args: &[String]) -> Result<()> {
    match args {
        [command] => match command.as_str() {
            "status" => status(data),
            "stop" => stop(data),
            "restart" => restart(data),
            _ => bail!("unknown daemon command {command:?}\nusage: {USAGE}"),
        },
        _ => bail!("usage: {USAGE}"),
    }
}

fn status(data: &Path) -> Result<()> {
    match DaemonClient::running(data)? {
        Some((client, hello)) => describe(data, &client, &hello),
        None => {
            println!("deckd is not running for {}", data.display());
            Ok(())
        }
    }
}

fn describe(data: &Path, client: &DaemonClient, hello: &Hello) -> Result<()> {
    println!("deckd running for {}", data.display());
    println!(
        "  pid {}, version {}, protocol {}, started {}",
        hello.pid,
        hello.version,
        hello.protocol,
        ago(hello.started)
    );
    println!("  executable {}", hello.executable.display());
    if hello.stale {
        println!("  the executable was rebuilt after this process started; run `deck daemon restart`");
    }
    let tasks = client.tasks()?;
    let live = tasks.iter().filter(|task| task.state == crate::State::Running).count();
    println!("  {} tasks, {live} running", tasks.len());
    Ok(())
}

fn stop(data: &Path) -> Result<()> {
    let Some((client, hello)) = DaemonClient::running(data)? else {
        println!("deckd is not running for {}", data.display());
        return Ok(());
    };
    client.shutdown()?;
    wait_exit(&hello)?;
    println!("stopped deckd pid {} for {}", hello.pid, data.display());
    Ok(())
}

fn restart(data: &Path) -> Result<()> {
    // A daemon from another build cannot be asked to exit; say what replaces
    // it instead of leaving the user with a raw protocol error.
    match DaemonClient::running(data) {
        Ok(Some((client, hello))) => {
            client.shutdown()?;
            wait_exit(&hello)?;
            println!("stopped deckd pid {} for {}", hello.pid, data.display());
        }
        Ok(None) => {}
        Err(e) => return Err(e).context("the running deckd cannot be restarted cleanly"),
    }
    let client = DaemonClient::connect_or_start(data)?;
    let hello = client.hello()?;
    describe(data, &client, &hello)
}

/// Wait until the stopped process is gone. A zombie under init is gone too:
/// [`crate::reconcile::group_running`] ignores it, as it does for task reuse.
fn wait_exit(hello: &Hello) -> Result<()> {
    let deadline = Instant::now() + EXIT;
    while crate::reconcile::group_running(hello.pid)? {
        if Instant::now() >= deadline {
            bail!("deckd pid {} did not exit within {}s", hello.pid, EXIT.as_secs());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

fn ago(started: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(started, |age| u64::try_from(age.as_millis()).unwrap_or(u64::MAX));
    let seconds = now.saturating_sub(started) / 1000;
    match seconds {
        0..=59 => format!("{seconds}s ago"),
        60..=3599 => format!("{}m ago", seconds / 60),
        _ => format!("{}h ago", seconds / 3600),
    }
}
