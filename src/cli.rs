use clap::{Parser, Subcommand};

#[derive(Debug, Clone, Eq, PartialEq, Parser)]
#[command(name = "lop", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Clone, Eq, PartialEq, Subcommand)]
pub enum Command {
    /// Fetch, inspect, and report without removing worktrees.
    Scan,
    /// Inspect candidates; removal remains disabled unless --yes is present.
    Prune {
        /// Apply approved removals.
        #[arg(long)]
        yes: bool,
    },
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, Command};

    #[test]
    fn parses_scan() {
        assert_eq!(
            Cli::try_parse_from(["lop", "scan"]).unwrap().command,
            Command::Scan
        );
    }

    #[test]
    fn prune_is_dry_run_by_default() {
        assert_eq!(
            Cli::try_parse_from(["lop", "prune"]).unwrap().command,
            Command::Prune { yes: false }
        );
    }

    #[test]
    fn prune_yes_enables_application() {
        assert_eq!(
            Cli::try_parse_from(["lop", "prune", "--yes"])
                .unwrap()
                .command,
            Command::Prune { yes: true }
        );
    }

    #[test]
    fn rejects_yes_for_scan() {
        assert!(Cli::try_parse_from(["lop", "scan", "--yes"]).is_err());
    }
}
