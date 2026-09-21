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
    /// Manage optional platform scheduling for Lop runs.
    Schedule {
        #[command(subcommand)]
        command: ScheduleCommand,
    },
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Subcommand)]
pub enum ScheduleCommand {
    /// Install or refresh the platform schedule.
    Install,
    /// Report whether the platform schedule is installed and loaded.
    Status,
    /// Edit validated settings, then regenerate and reload the schedule.
    Edit,
    /// Stop and remove the platform schedule.
    Uninstall,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, Command, ScheduleCommand};

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

    #[test]
    fn parses_schedule_commands() {
        for (name, expected) in [
            ("install", ScheduleCommand::Install),
            ("status", ScheduleCommand::Status),
            ("edit", ScheduleCommand::Edit),
            ("uninstall", ScheduleCommand::Uninstall),
        ] {
            assert_eq!(
                Cli::try_parse_from(["lop", "schedule", name])
                    .unwrap()
                    .command,
                Command::Schedule { command: expected }
            );
        }
    }
}
