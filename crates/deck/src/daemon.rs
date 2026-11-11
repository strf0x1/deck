//! `deck daemon …`: the commands for the deckd that serves this data root.
//! Argument parsing stays in `main`; the runtime owns the commands.

use anyhow::Result;

/// Run `deck daemon <args>` against the selected data root (`DECK_DATA`).
pub fn run(args: &[String]) -> Result<()> {
    deck_runtime::daemon_cli::run(&deck_runtime::data_root()?, args)
}
