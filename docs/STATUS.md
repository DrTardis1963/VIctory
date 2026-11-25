# VIctory-Unified Live Status & Multi-Agent Board

**Repository**: `https://github.com/DrTardis1963/victory-unified` (private)  
**Target Host**: Windows 10 Pro (`10.0.19045`), i7-9700K, RTX 3070 8 GB  
**External Test Config**: `F:\VIctory_Testing\victory.toml` (`208.62 GB` isolated testing copy verified; `F:\Quarantine\...` untouched)

---

## 1. Active Agent Claims (Lock Table)
*Claim a row before editing code so multiple providers (Antigravity, Claude Code, Codex, etc.) never collide.*

| Agent / Provider | Worktree | Branch | Scope / Task | Status | Updated |
| :--- | :--- | :--- | :--- | :--- | :--- |
| Antigravity | `.worktrees/unified` | `synthesis/unified` | Phase 0 Step 2: Agility SDK SM 6.6 unlock + multi-agent docs | `DONE` | 2026-11-25 |
| Existing local work | `.` (root) | `legal/policy-and-runtime-guards` | Uncommitted local edits (8 files) — **DO NOT TOUCH** | `RESERVED` | 2026-11-25 |
| Existing local work | `.worktrees/agy` | `fix/loader-tls-redirect-bounds-and-empty-section` | Existing worktree — **DO NOT TOUCH** | `RESERVED` | 2026-11-25 |

---

## 2. Definitive Roadmap Progress

| Phase | Milestone | Status | Notes |
| :--- | :--- | :--- | :--- |
| **Phase 0** | Host & Sandbox Setup | `DONE` | Testing copy (`109 files`, `208.62 GB`) + `victory.toml` + Agility SDK `1.614.1` SM 6.6 unlock verified on Win10 + RTX 3070. |
| **Phase 1** | First Boot & Baseline Telemetry | `DONE` | `victory warm` (`25,750` translated, `2,725` compute pipelines); visible 1080p boot + `xtask harness` baseline captured (`1.8 fps` / `527.8 ms` 3D p50, `7,820 / 6,743 MB` VRAM, `6,352 MB` PC spill, `411.4 ms` CPU recording, `128,783` null texture slots, BVH off, `#f133e35efed2a48a` `E_INVALIDARG`). |
| **Phase 2** | 100% Shaders & Full Visual Fidelity | `IN_PROGRESS` | `cargo xtask harness` built (`analyze`/`compare`/`prepare`/`shader`); next: merge Wave A–C (`hyphen` `9376fd6`, `vitoriabr` `d396d11`, `victory-amd` `003beef`) + fix `#f133e35efed2a48a` + test `bvh-test`. |
| **Phase 3** | 30 FPS Stable @ 1080p | `PENDING` | Eliminate queue flushes (`VICTORY_QUEUE_COPIES`), shadow copy-backs, tune 8 GB VRAM residency (`VICTORY_LEVELS_ON_DEMAND`). |
| **Phase 4** | 60 FPS Stable @ 1080p/1440p | `PENDING` | Decouple 30 Hz pacing, multi-core command translation, async compute queue, DXIL wave32 opts. |
| **Phase 5** | 60+ FPS, 100% Completion & Polish | `PENDING` | Uncapped pacing, `<16 GB` RAM target, full playthrough verification. |

---

## 3. Last Verification Gate
- `cargo xtask policy`: **PASS** (`182 files ok`)
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: **PASS** (0 warnings)
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`: **PASS** (0 warnings)
- `cargo test --workspace --locked --exclude victory-gpu --exclude victory-shader`: **PASS** (all 9 crates 100%, including `xtask` 10/10)
- `cargo test -p victory-shader --lib --locked`: **PASS** (`264 passed, 0 failed, 2 ignored` — all 38 SM 6.6 tests green on RTX 3070 + WARP)
- `cargo test -p victory-gpu --lib --locked -- --test-threads=1`: **PASS** (`198 passed, 0 failed, 4 ignored` — `depth_bounds_pipeline_loads_and_filters_draws` green)

---

## 4. Unclaimed Next-Task Queue (Safe for Parallel Agents)
- **[T-01]** Port `upstream-victory-amd` `9dad973` & `5cfc8d8` (+48 water/puddle/deferred shaders + `f64` wave sums) — *Scope*: `crates/victory-shader`
- **[T-02]** Port `upstream-vitoriabr` `5603b45`, `4b9872a`, `40beb26`, `0d1c4e0`/`dcc0305` (indexed stride, `dot4AddPacked`, LDS bounds, formats) — *Scope*: `crates/victory-shader`, `crates/victory-formats`
- **[T-03]** Merge `upstream-hyphen` `310c6eb`..`9376fd6` (packed-texture stale fix, `VICTORY_LEVELS_ON_DEMAND`, ~3 ms descriptor sync, PR #670/#648) — *Scope*: `crates/victory-gpu`
- **[T-04]** Port `upstream-victory-amd` `003beef` / `6c5e963` (per-16 MB `MAPPING` locks) + reconcile `origin/main` `940d32f` — *Scope*: `crates/victory-hle`, `crates/victory-gpu`
- **[T-05]** Fix compute shader `#f133e35efed2a48a` (`CreateComputePipelineState` `E_INVALIDARG`) & test `VICTORY_BVH_BUILDER=1` — *Scope*: `crates/victory-shader`, `crates/victory-gpu`
