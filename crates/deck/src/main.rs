//! `deck [--] [agent argv…]` or `deck daemon status|stop|restart`: open the
//! window and launch the agent, or manage the deckd serving this data root.
//! With no agent command the window runs `construct acp` in the current
//! directory. The `--` is optional because `cargo run -- …` consumes one, and
//! `deck -- daemon …` still runs an agent named `daemon`.

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
    if args.first().is_some_and(|a| a == "--") {
        args.remove(0);
    }
    let argv = if args.is_empty() { vec!["construct".into(), "acp".into()] } else { args };
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
    deck::app::run(argv, cwd);
}
