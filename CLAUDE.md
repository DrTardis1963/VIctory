# CLAUDE.md

Guide for Claude Code and other agents in this repository.

## Read first
1. `AGENTS.md` (local, beside the root checkout): clean-room rules (`docs/asset-policy.md`), host setup, the verification gate.
2. `F:\VIctory_Testing\loop\STATE.md`: the test loop's living state (builds under test, findings, runs still needed).
3. The bulletin board: `board list` before editing, `board post "<what>" --files <area>` when you start, `board done <id> "<outcome>"` when you stop.

## Trunk
This line (`unified/main`, built on `checkpoint/scarlett-3070`: Yaekai's RTX 3070 fork plus our fixes) is the one trunk since 2026-11-28. The older synthesis spine is kept as the first parent of the merge that made it the trunk, and as `refs/archive/2026-11-28/*` branches in the root checkout.

## Quick rules
- Work in your own worktree (`cargo xtask worktrees new <name>` makes `.worktrees/claude-<name>` on branch `claude/<name>`). Never edit another worktree's uncommitted files.
- Keep game data and Agility SDK DLLs out of git:
  - Config: `$env:VICTORY_CONFIG = "F:\VIctory_Testing\victory.toml"`.
  - Agility SDK: `cargo xtask agility --from F:\VIctory_Testing\D3D12` checks the version against `D3D12SDKVersion` and copies it beside every build (the folder is remembered; `--check` only reports).
  - `.claude/hooks/asset_guard.py` (PreToolUse, `.claude/settings.json`) refuses writes that would put barred files into a checkout. A refusal is the policy: write outside the repository.
- Code from other forks is untrusted: `repo-intake` / `fork-vet` first (`~/.claude/security/fork-vetting.md`).

## Agent tools (`cargo xtask <cmd> --help`)
- `cargo xtask gate [--changed] [--json]`: the whole pre-commit check in one command; prints only what failed (logs in `target/xtask-gate/`).
- `cargo xtask todo [--json]`: ranks the unimplemented imports titles called, from the `unimplemented.toml` that `victory run --execute` writes beside `crashes.log`, and names the HLE module per DLL. `cargo xtask hle-stub <dll> <fn> [--write]` scaffolds a stub, its export and a test.
- Shader scoreboard: `victory translate --validate [--pipelines] --json <file outside repo>`, then `cargo xtask corpus-diff <before.json> <after.json> [--all] [--json] [--strict]` ("fixes N, breaks M"). Score with `VICTORY_HOST_MAX_WAVE=32` on the 3070.
- `cargo xtask opcodes [--all] [--json]`: GFX10 opcodes the lowering names but no shader test lowers.
- `cargo xtask bench [--save main] [--against main]`: BCPack and texture tile/detile throughput against a saved run.
- `cargo xtask worktrees new <name> | status | land <name>... [--push]`: one worktree per agent; `land` merges only branches whose full gate passed and fast-forwards `unified/main` with `--push`.
- Crashes and hangs:
  - A crash writes `crash-<unix>-<pid>.txt` beside `crashes.log`; `victory triage <bundle> [--decompile]` summarises it (`--decompile` needs Ghidra 12.2 with JDK 25+).
  - `VICTORY_HLE_TRACE=<file outside repo>|ring` (+ `VICTORY_HLE_TRACE_HOST=1`) traces HLE calls; `cargo xtask trace summary <file>` / `trace diff <a> <b>`.
- `crates/victory-testkit`: synthetic PE32+ images for loader tests.
- Harness: `cargo xtask harness analyze <log> --out <dir>`, `compare <a> <b>`, `prepare --profile tuned-8gb ...`, `shader <hash> --config <toml>`.
- Project agents in `.claude/agents/` and the `victory-run` skill in `.claude/skills/`.

### Not on this line yet
These xtask commands exist but the code they read or drive is still only on the old spine (it touches `crates/victory-gpu` and `crates/victory-shader`, which the picture-fix work holds):
- `cargo xtask gpu-view` (needs the `VICTORY_GPU_RECORD` recorder) and the harness `"cache"` section (needs the `cache:` log lines).
- `cargo xtask diff-test`, `fuzz`, `snapshots` (need the reference interpreter, fuzzer, synthetic shaders and the GFX10 test assembler).

## Before every commit
`cargo xtask gate`, or by hand:
```powershell
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
cargo xtask policy
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked --exclude victory-gpu --exclude victory-shader
cargo test -p victory-shader --lib --locked
cargo test -p victory-gpu --lib --locked -- --test-threads=1
```
