//! `deck [--] [agent argv…]`: open the window and launch the agent. With no
//! agent command, runs `construct acp` in the current directory. The `--` is
//! optional because `cargo run -- …` consumes one.

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "--") {
        args.remove(0);
    }
    let argv = if args.is_empty() { vec!["construct".into(), "acp".into()] } else { args };
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
    deck::app::run(argv, cwd);
}
