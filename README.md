# lop

Conservative cleanup of stale Git worktrees.

> [!NOTE]
> Lop is under development and is not ready for use.

## Intended interface

```console
lop scan
lop prune --yes
```

`scan` reports what Lop would do. `prune` remains a dry run unless `--yes` is present.

Repository roots are explicit configuration; linked worktree paths come from Git rather than a directory convention. See [`config.example.toml`](config.example.toml).

Scheduling is optional. The normal CLI works independently, while platform-aware schedule commands will support macOS LaunchAgents first.

## License

[MIT](LICENSE)
