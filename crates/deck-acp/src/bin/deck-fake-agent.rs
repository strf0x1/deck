//! A scripted ACP agent for tests and for driving deck without a model.
//! Each prompt: a thought, a streamed reply, one tool call that asks
//! permission, then a final line saying what it was told.
//!
//! It speaks over stdio by default. `deck-fake-agent --listen <socket>` binds
//! a Unix socket and serves the first connection instead, so a test or a
//! runtime can reach an agent deck did not launch. Either way the loop ends
//! at end of input, which is what closing the stream looks like from here.
//!
//! `DECK_FAKE_DELAY_MS` paces the stream (default 35).
//! `DECK_FAKE_NAMES=legacy|reverse` makes it speak only the bare or only the
//! reverse-DNS extension names, like a particular Construct release; unset
//! advertises and accepts both (deck#36).
//!
//! Sessions outlive the process the way construct's do, so the runtime's
//! restart paths have something to restart: with `DECK_FAKE_STATE` naming a
//! directory, each update is appended before delivery to `<dir>/<session>.jsonl`, and
//! `session/load` replays it before answering while `session/resume` does
//! not. Repeated initialize starts a fresh attachment; loading a live session
//! replays it and reissues unanswered questions with fresh IDs. Close forgets it. Without the variable a
//! session is always `fake-1` and nothing persists. Loading a cut-off turn
//! records it as interrupted, without repeating it.

use std::time::Duration;

#[path = "fake/agent.rs"]
mod agent;
#[path = "fake/journal.rs"]
mod journal;
#[path = "fake/turn.rs"]
mod turn;
use agent::Agent;

fn main() {
    let delay =
        Duration::from_millis(std::env::var("DECK_FAKE_DELAY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(35));
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--listen") => listen(args.next(), delay),
        Some(other) => {
            eprintln!("unknown argument {other}; usage: deck-fake-agent [--listen <socket>]");
            std::process::exit(2);
        }
        None => {
            Agent::new(std::io::stdout().lock()).serve(std::io::BufReader::new(std::io::stdin()), delay);
        }
    }
}

#[cfg(unix)]
fn listen(path: Option<String>, delay: Duration) {
    use std::os::unix::net::UnixListener;
    let Some(path) = path else {
        eprintln!("--listen needs a socket path");
        std::process::exit(2);
    };
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("bind {path}: {e}");
            std::process::exit(1);
        }
    };
    let (stream, _) = match listener.accept() {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("accept {path}: {e}");
            std::process::exit(1);
        }
    };
    let input = match stream.try_clone() {
        Ok(s) => std::io::BufReader::new(s),
        Err(e) => {
            eprintln!("clone {path}: {e}");
            std::process::exit(1);
        }
    };
    Agent::new(stream).serve(input, delay);
}

#[cfg(not(unix))]
fn listen(_: Option<String>, _: Duration) {
    eprintln!("--listen needs Unix sockets");
    std::process::exit(2);
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())
}
