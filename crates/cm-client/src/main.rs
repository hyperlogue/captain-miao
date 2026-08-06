//! `miao-client` — a small CLI over the local pty pool: list the
//! sessions the per-host daemon is holding, and reattach a terminal to one. It
//! links libshpool for the attach (an in-process pty proxy) but hosts no
//! daemon/pool of its own — it's purely a client over the pool socket the
//! `miao-server` daemon binds.

mod pool;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "miao-client",
    version,
    about = "List and attach to captain-miao's local pooled sessions"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// List every pooled session on this host (the default when no subcommand
    /// is given).
    List {
        /// Emit the list as JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Attach this terminal to a pooled session by name — only if it exists and
    /// no other terminal is attached to it (won't create a stray shell, won't
    /// steal a live one).
    Attach {
        /// Pool session name, as shown by `list`.
        name: String,
    },
}

fn main() -> Result<()> {
    // Everything here is synchronous, and `attach` calls `libshpool::run` (which
    // must precede any thread — its daemon path can double-fork), so this binary
    // never builds an async runtime.
    match Cli::parse()
        .command
        .unwrap_or(Command::List { json: false })
    {
        Command::List { json } => pool::list(json),
        Command::Attach { name } => pool::attach(name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses() {
        let parse = |args: &[&str]| Cli::try_parse_from(args).map(|c| c.command);
        // Bare invocation → list by default.
        assert!(parse(&["miao-client"]).unwrap().is_none());
        assert!(matches!(
            parse(&["miao-client", "list"]).unwrap(),
            Some(Command::List { json: false })
        ));
        assert!(matches!(
            parse(&["miao-client", "list", "--json"]).unwrap(),
            Some(Command::List { json: true })
        ));
        assert!(matches!(
            parse(&["miao-client", "attach", "cm-claude-1-1"]).unwrap(),
            Some(Command::Attach { name }) if name == "cm-claude-1-1"
        ));
        // Attach requires a name.
        assert!(parse(&["miao-client", "attach"]).is_err());
    }
}
