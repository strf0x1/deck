//! Commands for tasks in the selected data root.

use crate::DaemonClient;
use anyhow::{Result, bail};

const USAGE: &str = "deck task resume <id>";

/// Resume a stopped task through the daemon, starting the daemon if needed.
pub fn run(args: &[String]) -> Result<()> {
    match args {
        [command, id] if command == "resume" && !id.is_empty() => {
            let client = DaemonClient::connect_or_start_for_cli(&crate::data_root()?)?;
            let task = client.resume(id)?;
            println!("resumed task {}", task.id);
            Ok(())
        }
        _ => bail!("usage: {USAGE}"),
    }
}
