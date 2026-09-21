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
dry run unless `--yes` is present. Scheduler and Herdr integrations are
optional; neither is required by the CLI.

Lop emits newline-delimited JSON records. Every record includes a schema
version, and repository/worktree records use stable reason codes. A run returns
nonzero for operational failures, while safety refusals are reported without
making the run fail.

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
from Git rather than a directory convention. Scheduling support will target
macOS LaunchAgents first while remaining separate from the normal CLI.

## Development

```console
make check
```

Individual tasks are `make fmt`, `make test`, and `make lint`. The lint task runs
warning-denied Clippy with the `pedantic` lint group enabled.

## License

[MIT](LICENSE)
