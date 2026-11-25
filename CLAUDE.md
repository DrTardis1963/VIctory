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
  - Agility SDK DLLs live at `F:\VIctory_Testing\D3D12\` (copy to gitignored `target/.../D3D12/` only): `cargo xtask agility --from F:\VIctory_Testing\D3D12` checks the version matches `D3D12SDKVersion` and copies it into every build folder (the folder is remembered; `--check` only reports).
  - Never touch `F:\Quarantine\...`.
  - `.claude/hooks/asset_guard.py` (a PreToolUse hook in `.claude/settings.json`) refuses Write/Bash/PowerShell calls that would put files the asset policy bars into a checkout, ignored folders included. A refusal is the policy, not a bug: write outside the repository.
- **Agent tools** (`cargo xtask <cmd> --help`):
  - `cargo xtask gate [--changed] [--json]` — the verification gate below in one command; prints only what failed (logs in `target/xtask-gate/`) and installs the Agility runtime first.
  - `cargo xtask todo [--json]` — ranks the unimplemented imports titles called, from the `unimplemented.toml` that `victory run --execute` writes beside `crashes.log`, and names the HLE module file per DLL.
  - `cargo xtask opcodes [--all] [--json]` — GFX10 coverage: per encoding, the opcodes the decoder knows, the shader unit tests lower, and the lowering only names (runs victory-shader's tests, about 5 minutes; `--from <list>` reuses a run).
- **Use the Clean-Room Diagnostic Harness** (`cargo xtask harness --help`):
  - `cargo xtask harness analyze <log> --out <external-dir>` — parses frame waterfall, GPU/CPU/settle waits, 8 GB VRAM/PCIe spill, and dropped shaders.
  - `cargo xtask harness compare <a/report.json> <b/report.json>` — side-by-side A/B delta table.
  - `cargo xtask harness prepare --profile tuned-8gb ...` — generates isolated launcher + `run_meta.json` outside the repo.
  - `cargo xtask harness shader <hash> --config <toml>` — diagnoses a single refused/dropped shader container.
- **Power-Loss Durability, Shared Credentials & Dual Backups**:
  - `.git/config` enforces `core.fsync = all`, `core.fsyncMethod = fsync`, `fetch.fsckObjects = true`, and `transfer.fsckObjects = true` so git metadata survives sudden power loss.
  - Private-repo authentication (`DrTardis1963/victory-unified`, `DrTardis1963/VIctory`) uses the shared system `credential.helper = manager` (Git Credential Manager); never embed tokens or passwords in `.git/config`, URLs, or CLI commands.
  - Every synthesis milestone is backed up in two places: `unified` remote `refs/heads/backup/*` branches and offline local git bundles + `.git/config` backup in `%LOCALAPPDATA%\victory-backups\`.
- **Always run before committing** (`cargo xtask gate` runs all of these, plus clippy with `--all-features`):
  ```powershell
  $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
  cargo xtask policy
  cargo clippy --workspace --all-targets --locked -- -D warnings
  cargo test --workspace --locked --exclude victory-gpu --exclude victory-shader
  cargo test -p victory-shader --lib --locked
  cargo test -p victory-gpu --lib --locked -- --test-threads=1
  ```
