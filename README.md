# lop

Conservative cleanup of stale Git worktrees.

> [!NOTE]
> Lop is under development and is not ready for use.

## Commands

```console
lop scan
lop prune
lop prune --yes
```

`scan` reports what Lop would do. `prune` runs the same pipeline and remains a
dry run unless `--yes` is present. Scheduler and Herdr integrations are optional; neither is required for manual
cleanup runs.

Lop emits newline-delimited JSON records. Every record includes a schema
version, and repository/worktree records use stable reason codes. Worktree
records include `classification_changed`; it is `true` on the first observation
or when the outcome/reason changes, and `false` on repeated identical scans so
notification consumers can suppress duplicates without losing inventory. A run
returns nonzero for operational failures, while safety refusals are reported
without making the run fail.

## Configuration

Lop requires at least one explicitly configured repository root. Copy
[`config.example.toml`](config.example.toml) to
`~/.config/lop/config.toml`. `XDG_CONFIG_HOME` is honored.

```toml
roots = ["~/dev/src"]
scan_depth = 3
fetch_timeout_seconds = 60
check_processes = true
```

There is no implicit scan root. Root paths must be absolute or begin with `~/`.
Unknown settings and invalid limits are rejected with an actionable error.

Operational state is stored in `~/.local/state/lop/state.json`, honoring
`XDG_STATE_HOME`. It records fetch/classification memory only and is never used
as repository discovery input. A process-wide lock prevents overlapping runs.

Repository roots are discovered from configuration; linked worktree paths come
from Git rather than a directory convention. For branches whose upstream has
disappeared, Lop requires Worktrunk 0.66.0 or newer and explicitly consumes its
versioned JSON schema. Only Worktrunk's positive local-Git integration proofs
advance a worktree to later safety gates; missing, incompatible, malformed, or
indeterminate results retain it.

Before removal, Lop rechecks the branch and HEAD, lock state, checkout topology,
tracked and untracked changes, and live-process working directories. Process
inspection uses `lsof` on macOS and fails closed when it is unavailable. The
`check_processes = false` setting is an explicit opt-out from that gate.

Herdr coordination is automatic but optional. When a compatible Herdr client and
socket are available, Lop refuses candidates containing a focused pane or active
agent and fails closed on incomplete candidate activity. A missing client or
unavailable socket does not weaken the independent Git, Worktrunk, and process
checks. After Worktrunk confirms removal, Lop closes only Herdr workspaces mapped
to that checkout; a coordination failure is reported without obscuring the
completed Git removal.

Lop runs a foreground, confirmed Worktrunk removal without force, force-delete,
or process-reaping flags, and reports Worktrunk's branch-deletion outcome.

## Scheduling

macOS scheduling is optional and remains separate from `scan` and `prune`:

```console
lop schedule install
lop schedule status
lop schedule edit
lop schedule uninstall
```

Installation starts in preview-only `scan` mode every 1,800 seconds. It creates
`~/Library/LaunchAgents/org.nixos.lop.plist`, stores validated settings in
`~/.config/lop/schedule.toml` (honoring `XDG_CONFIG_HOME`), and writes service
output under `~/Library/Logs/org.nixos`. `schedule edit` opens a temporary copy
with `VISUAL` or `EDITOR`; Lop accepts only `scan` or `prune` mode and intervals
from 300 through 604,800 seconds before replacing and reloading the agent.
`prune` mode always invokes `lop prune --yes` explicitly.

The generated LaunchAgent runs at load without `KeepAlive`, uses an explicit
PATH containing Lop, Git, and Worktrunk, disables terminal credential prompts,
and carries a validated `SSH_AUTH_SOCK` when present. Installation verifies the
scheduler's binaries, runtime paths, and credential mechanism, then performs a
noninteractive dry-run fetch against **every** discovered repository. Repository
fetch failures are aggregated and printed as warnings; they do not prevent a
`scan` schedule from being installed or refreshed. Scheduled runs fail closed
for each affected repository, leave all of its worktrees untouched, and continue
processing healthy repositories. This isolates an offline remote or broken SSH
host alias without hiding the repository configuration problem.

Keep the schedule in `scan` mode while reviewing preview evidence. If any
repository preflight is degraded, installing or updating `prune` mode requires
an additional deliberate acknowledgement:

```console
lop schedule install --allow-degraded-preflight
lop schedule edit --allow-degraded-preflight
```

Use the flag only after reviewing every warning. `lop schedule status` reports
the configured mode, actual installed command and interval, settings/artifact
drift, and repositories that currently fail preflight. Rerun `lop schedule
install` after credentials, network access, or remote configuration is repaired
to confirm recovery. Schedule settings and the LaunchAgent are updated as one
transaction; a failed reload restores both previous files and the prior loaded
service, or reports any rollback failure explicitly.

Lop's process-wide lock prevents overlap. Install and uninstall are safe to
repeat; uninstall removes the LaunchAgent and schedule settings while retaining
logs. Other platforms return a scheduling-unavailable error without changing
manual command behavior. To roll back unattended cleanup, use `lop schedule
edit` and set `mode = "scan"`; to stop all scheduled runs, use `lop schedule
uninstall`. After the first approved cleanup run, manually compare every
`removed` record's `branch_*` reason code with Git's surviving worktrees and
local branches.

## Declarative package

The canonical public repository is
[`https://github.com/johnallen3d/lop`](https://github.com/johnallen3d/lop). The
flake exports `packages.<system>.lop` and `packages.<system>.default`. A
`system-config` flake can package Lop declaratively by adding the canonical
repository as an input and including the package in Home Manager. Replace the
input owner only when intentionally tracking a fork:

```nix
# flake inputs: canonical Lop repository
lop.url = "github:johnallen3d/lop";

# Home Manager module arguments include `lop`
home.packages = [lop.packages.${pkgs.system}.default];
```

Scheduling remains opt-in after package installation, so `lop scan` and
`lop prune --yes` continue to work without a LaunchAgent.

## Development

```console
make check
```

Individual tasks are `make fmt`, `make test`, and `make lint`. The lint task runs
warning-denied Clippy with the `pedantic` lint group enabled.

## License

[MIT](LICENSE)
