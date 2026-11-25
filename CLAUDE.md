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
- **Use the Clean-Room Diagnostic Harness** (`cargo xtask harness --help`):
  - `cargo xtask harness analyze <log> --out <external-dir>` — parses frame waterfall, GPU/CPU/settle waits, 8 GB VRAM/PCIe spill, and dropped shaders.
  - `cargo xtask harness compare <a/report.json> <b/report.json>` — side-by-side A/B delta table.
  - `cargo xtask harness prepare --profile tuned-8gb ...` — generates isolated launcher + `run_meta.json` outside the repo.
  - `cargo xtask harness shader <hash> --config <toml>` — diagnoses a single refused/dropped shader container.
- **Power-Loss Durability, Shared Credentials & Dual Backups**:
  - `.git/config` enforces `core.fsync = all`, `core.fsyncMethod = fsync`, `fetch.fsckObjects = true`, and `transfer.fsckObjects = true` so git metadata survives sudden power loss.
  - Private-repo authentication (`DrTardis1963/victory-unified`, `DrTardis1963/VIctory`) uses the shared system `credential.helper = manager` (Git Credential Manager); never embed tokens or passwords in `.git/config`, URLs, or CLI commands.
  - Every synthesis milestone is backed up in two places: `unified` remote `refs/heads/backup/*` branches and offline local git bundles + `.git/config` backup in `%LOCALAPPDATA%\victory-backups\`.
- **Always run before committing**:
  ```powershell
  $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
  cargo xtask policy
  cargo clippy --workspace --all-targets --locked -- -D warnings
  cargo test --workspace --locked --exclude victory-gpu --exclude victory-shader
  cargo test -p victory-shader --lib --locked
  cargo test -p victory-gpu --lib --locked -- --test-threads=1
  ```
