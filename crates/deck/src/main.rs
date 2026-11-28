//! `deck [--] [agent argv…]`, `deck daemon …`, or `deck task …`: open the
//! window and launch the agent, or manage the deckd serving this data root.
//! With no agent command the window runs `construct acp` in the current
//! directory. The `--` is optional because `cargo run -- …` consumes one, and
//! `deck -- daemon …` or `deck -- task …` still runs an agent by that name.

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "daemon") {
        args.remove(0);
        if let Err(e) = deck::daemon::run(&args) {
            eprintln!("deck: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    if args.first().is_some_and(|a| a == "task") {
        args.remove(0);
        if let Err(e) = deck_runtime::task_cli::run(&args) {
            eprintln!("deck: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    if args.first().is_some_and(|a| a == "--") {
        args.remove(0);
    }
    let argv = if args.is_empty() { vec!["construct".into(), "acp".into()] } else { args };
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
    deck::app::run(argv, cwd);
}
