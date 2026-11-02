//! Argument parsing for the host daemon and its internal child lifeline.

use anyhow::{Context as _, Result, bail};
use deck_runtime::{daemon, paths::Paths};
use std::path::PathBuf;
use std::time::Duration;

fn main() {
    if let Err(e) = run() {
        eprintln!("deckd: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut data = None;
    let mut idle = daemon::IDLE;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data" => data = Some(PathBuf::from(args.next().context("--data needs a path")?)),
            "--idle-ms" => idle = Duration::from_millis(args.next().context("--idle-ms needs a number")?.parse()?),
            "--child" => {
                let mut argv: Vec<_> = args.collect();
                if argv.first().is_some_and(|a| a == "--") {
                    argv.remove(0);
                }
                return deck_runtime::child::run(&argv);
            }
            _ => bail!("unknown argument {arg}; usage: deckd [--data path] [--idle-ms n]"),
        }
    }
    let data = data.map_or_else(deck_runtime::data_root, Ok)?;
    let paths = Paths::for_data(&data)?;
    daemon::serve(daemon::Options { data, paths, idle })
}
