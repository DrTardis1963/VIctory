# CLAUDE.md

This file guides Claude Code and other AI coding agents working in this repository.

## Required Reading (Read Before Any Edit)
1. [AGENTS.md](AGENTS.md) — Hard clean-room rules (`docs/asset-policy.md`), multi-agent concurrency protocol, host setup, and mandatory verification gates.
2. [docs/STATUS.md](docs/STATUS.md) — Current phase, active agent claims/worktrees, latest test results, and next unclaimed tasks.
3. [docs/UPSTREAM_FEED.md](docs/UPSTREAM_FEED.md) — Synced upstream remotes (`backup/*` on `unified`), newly harvested commits, and community tracker status (prevents duplicating already-solved work).

## Quick Rules for Concurrent Sessions
- **Work in your own git worktree** (`.worktrees/claude-<task>`). Never modify `.worktrees/unified`, `.worktrees/agy`, or the root checkout's uncommitted files unless you own that worktree.
- **Claim your task** in `docs/STATUS.md` before editing code, and update it when finished.
- **Keep game data & Agility SDK DLLs out of git**:
  - External config: `$env:VICTORY_CONFIG = "F:\VIctory_Testing\victory.toml"`
  - Agility SDK DLLs live at `F:\VIctory_Testing\D3D12\` (copy to gitignored `target/.../D3D12/` only).
  - Never touch `F:\Quarantine\...`.
- **Always run before committing**:
  ```powershell
  $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
  cargo xtask policy
  cargo clippy --workspace --all-targets --locked -- -D warnings
  cargo test --workspace --locked --exclude victory-gpu --exclude victory-shader
  cargo test -p victory-shader --lib --locked
  cargo test -p victory-gpu --lib --locked -- --test-threads=1
  ```
