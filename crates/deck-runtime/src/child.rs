//! The internal B1 child helper. It shares a process group with the agent
//! and kills that entire group when deckd's input pipe closes or the agent
//! exits. A separate HUP watcher still detects deckd death when forwarding
//! stdin is blocked on an agent that stopped reading.

use anyhow::{Context as _, Result, bail};
use std::io::{self, Write as _};
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

extern "C" fn keep_supervising(_: libc::c_int) {}

/// Run only as the process-group leader started by `Local`.
pub fn run(argv: &[String]) -> Result<()> {
    let (program, args) = argv.split_first().context("no agent command")?;
    // SAFETY: these functions take no pointers and only query this process.
    if unsafe { libc::getpgrp() != libc::getpid() } {
        bail!("deckd child helper must lead its own process group");
    }
    // SAFETY: the handler has C ABI and performs no work. Catching TERM
    // keeps the supervisor alive while the group receives graceful stop.
    if unsafe { libc::signal(libc::SIGTERM, keep_supervising as *const () as libc::sighandler_t) } == libc::SIG_ERR {
        return Err(io::Error::last_os_error()).context("installing child helper signal handler");
    }
    let mut command = Command::new(program);
    command.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());
    // SAFETY: the hook calls only async-signal-safe signal(2), then returns.
    // The real agent inherits this helper's process group, but gets TERM's
    // normal disposition rather than the supervisor's handler.
    unsafe {
        command.pre_exec(|| {
            if libc::signal(libc::SIGTERM, libc::SIG_DFL) == libc::SIG_ERR {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut agent = match command.spawn().with_context(|| format!("launching {program}")) {
        Ok(agent) => agent,
        Err(error) => {
            report(Err(format!("{error:#}")))?;
            return Err(error);
        }
    };
    let mut input = agent.stdin.take().context("agent stdin")?;
    let mut output = agent.stdout.take().context("agent stdout")?;
    if std::thread::Builder::new().name("deckd-parent-watch".into()).spawn(watch_parent).is_err() {
        kill_group();
    }
    if std::thread::Builder::new()
        .name("deckd-child-input".into())
        .spawn(move || {
            let _ = io::copy(&mut io::stdin().lock(), &mut input);
            kill_group();
        })
        .is_err()
    {
        kill_group();
    }
    let (ready, output_ready) = mpsc::sync_channel(1);
    let (drained, output_drained) = mpsc::sync_channel(1);
    if std::thread::Builder::new()
        .name("deckd-child-output".into())
        .spawn(move || {
            if output_ready.recv().is_ok() {
                let _ = io::copy(&mut output, &mut io::stdout().lock());
            }
            let _ = drained.send(());
        })
        .is_err()
    {
        kill_group();
    }
    // ACP output cannot precede this private startup result. The backend
    // consumes the header before exposing the remaining stream to a relay.
    if report(Ok(())).is_err() || ready.send(()).is_err() {
        kill_group();
    }
    let _ = agent.wait();
    // Let final stdout reach the relay before terminating lingering tools.
    // A tool that inherited stdout cannot hold supervision open forever.
    let _ = output_drained.recv_timeout(Duration::from_millis(100));
    // Even an agent that exits normally may leave tools in its group.
    kill_group();
}

fn report(started: std::result::Result<(), String>) -> Result<()> {
    let mut output = io::stdout().lock();
    serde_json::to_writer(&mut output, &started)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

fn watch_parent() {
    // HUP is reported regardless of requested events, including while
    // unread input remains in the pipe. Do not request POLLIN: the input
    // copier owns reads, and data must not wake this monitor repeatedly.
    let mut parent = libc::pollfd { fd: libc::STDIN_FILENO, events: 0, revents: 0 };
    loop {
        // SAFETY: parent is one initialized pollfd alive for the call.
        let result = unsafe { libc::poll(&mut parent, 1, -1) };
        if result > 0 && parent.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            kill_group();
        }
        if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            kill_group();
        }
    }
}

fn kill_group() -> ! {
    // SAFETY: run verified that this still-live helper leads this group.
    // Including ourselves prevents a tool from outliving the supervisor.
    unsafe { libc::kill(-libc::getpgrp(), libc::SIGKILL) };
    std::process::exit(1);
}
