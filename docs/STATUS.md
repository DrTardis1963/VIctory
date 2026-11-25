# VIctory-Unified Live Status & Multi-Agent Board

**Repository**: `https://github.com/DrTardis1963/victory-unified` (private)  
**Target Host**: Windows 10 Pro (`10.0.19045`), i7-9700K, RTX 3070 8 GB  
**External Test Config**: `F:\VIctory_Testing\victory.toml` (`208.62 GB` isolated testing copy verified; `F:\Quarantine\...` untouched)

---

## 1. Active Agent Claims (Lock Table)
*Claim a row before editing code so multiple providers (Antigravity, Claude Code, Codex, etc.) never collide.*

| Agent / Provider | Worktree | Branch | Scope / Task | Status | Updated |
| :--- | :--- | :--- | :--- | :--- | :--- |
| Antigravity | `.worktrees/unified` | `synthesis/unified` | `WillyboPeep/victory-all-forks` security audit & Waves 1–3 synthesis | `DONE` | 2026-11-25 |
| Existing local work | `.` (root) | `legal/policy-and-runtime-guards` | Uncommitted local edits (8 files) — **DO NOT TOUCH** | `RESERVED` | 2026-11-25 |
| Existing local work | `.worktrees/agy` | `fix/loader-tls-redirect-bounds-and-empty-section` | Existing worktree — **DO NOT TOUCH** | `RESERVED` | 2026-11-25 |
| Claude Code | `.worktrees/claude-heap-pass-copyback` | `claude/heap-pass-copyback` | Port all-forks heap buffer pass on marked slots (7447a7a) and 4 KiB copy-back parts (6ca2743), left out of Wave 3 | `IN PROGRESS` | 2026-11-26 |

---

## 2. Definitive Roadmap Progress

| Phase | Milestone | Status | Notes |
| :--- | :--- | :--- | :--- |
| **Phase 0** | Host & Sandbox Setup | `DONE` | Testing copy (`109 files`, `208.62 GB`) + `victory.toml` + Agility SDK `1.614.1` SM 6.6 unlock verified on Win10 + RTX 3070. |
| **Phase 1** | First Boot & Baseline Telemetry | `DONE` | `victory warm` (`25,750` translated, `2,725` compute pipelines); visible 1080p boot + `xtask harness` baseline captured (`1.8 fps` / `527.8 ms` 3D p50, `7,820 / 6,743 MB` VRAM, `6,352 MB` PC spill, `411.4 ms` CPU recording, `128,783` null texture slots, BVH off, `#f133e35efed2a48a` `E_INVALIDARG`). |
| **Phase 2** | 100% Shaders & Full Visual Fidelity | `IN_PROGRESS` | **Wave A+B + 8 GB Fixes + `all-forks` Synthesis (Waves 1–3)**: Static 8-vector security audit clean (excluded `.ps1`/`LEIA-ME-GTA6.md`/`ALL-FORKS.md`, preserved `net.rs` ICMP block & `policy.rs` traversal fix). Merged Wave 1 (`59d6366`: HLE/Loader/Formats), Wave 2 (`6673949`: Shader/PSV0), Wave 3 (`2bffe36`: GPU COM safety, 10-bit capture, sampler clamp, SIMD relay merge, indexed flights). Hardened `.git/config` with `core.fsync=all`, `fsyncMethod=fsync`, `fsckObjects=true`, plus dual `backup/*` + `%LOCALAPPDATA%\victory-backups\*.bundle`. Next: A/B `tuned-8gb` + `bvh-test` + Phase 2C (`#f133e35efed2a48a`). |
| **Phase 3** | 30 FPS Stable @ 1080p | `PENDING` | Eliminate queue flushes (`VICTORY_QUEUE_COPIES`), shadow copy-backs, tune 8 GB VRAM residency (`VICTORY_CARD_ROOM_FOR_TARGETS_MB`). |
| **Phase 4** | 60 FPS Stable @ 1080p/1440p | `PENDING` | Decouple 30 Hz pacing, multi-core command translation, async compute queue, DXIL wave32 opts. |
| **Phase 5** | 60+ FPS, 100% Completion & Polish | `PENDING` | Uncapped pacing, `<16 GB` RAM target, full playthrough verification. |

---

## 3. Last Verification Gate
- `cargo xtask policy`: **PASS** (`187 files ok`)
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: **PASS** (0 warnings)
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`: **PASS** (0 warnings)
- `cargo test --workspace --locked --exclude victory-gpu --exclude victory-shader`: **PASS** (`230 passed, 0 failed, 5 ignored`)
- `cargo test -p victory-shader --lib --locked`: **PASS** (`270 passed, 0 failed, 2 ignored`)
- `cargo test -p victory-gpu --lib --locked -- --test-threads=1`: **PASS** (`221 passed, 0 failed, 4 ignored` — includes `targets_room_reservation_respects_budget_and_override` & `zero_address_views_do_not_make_each_other_stale`)
- Shader diagnostic note (`#f133e35efed2a48a`): `dxil.dll` validation passes (`26,703` IR instructions, `52` used resources), `CreateComputePipelineState` fails with `E_INVALIDARG`; queued for Phase 2C root-signature / compute state inspection.

---

## 4. Unclaimed Next-Task Queue (Safe for Parallel Agents)
- **[T-01]** Live A/B telemetry comparison on rebuilt `diag` `tuned-8gb` profile vs `baseline_20261125_203607` — *Scope*: `F:\VIctory_Testing\` + `cargo xtask harness`
- **[T-02]** Audit `upstream-vitoriabr` `40beb26` (LDS bounds) — *Scope*: `crates/victory-shader`
- **[T-03]** Reconcile `origin/main` `940d32f` (if not already covered by spine) — *Scope*: `crates/victory-gpu`
- **[T-04]** Fix compute shader `#f133e35efed2a48a` (`CreateComputePipelineState` `E_INVALIDARG`) & validate `VICTORY_BVH_BUILDER=1` with `VICTORY_BVH_QUARANTINE=1` — *Scope*: `crates/victory-shader`, `crates/victory-gpu`
