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
| **Phase 1** | First Boot & Baseline Telemetry | `DONE` | `victory warm` (`25,750` translated, `2,725` compute pipelines); 1080p boot succeeded (`30.0 fps` / `33.3 ms` intro/menus, `1,560 / 7,249 MB` VRAM, `0` dropped pipelines, `0` settles). |
| **Phase 2** | 100% Shaders & Full Visual Fidelity | `NEXT` | Port `[T-01]`/`[T-02]` (`victory-amd` +48 shaders & `f64` wave sums), capture 3D cutscene/world telemetry, enable RT shadows, fix green transition & banding. |
| **Phase 3** | 30 FPS Stable @ 1080p | `PENDING` | Eliminate queue flushes (`VICTORY_QUEUE_COPIES`), shadow copy-backs, tune 8 GB VRAM residency (`VICTORY_LEVELS_ON_DEMAND`). |
| **Phase 4** | 60 FPS Stable @ 1080p/1440p | `PENDING` | Decouple 30 Hz pacing, multi-core command translation, async compute queue, DXIL wave32 opts. |
| **Phase 5** | 60+ FPS, 100% Completion & Polish | `PENDING` | Uncapped pacing, `<16 GB` RAM target, full playthrough verification. |

---

## 3. Last Verification Gate
- `cargo xtask policy`: **PASS** (`181 files ok`)
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: **PASS** (0 warnings)
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`: **PASS** (0 warnings)
- `cargo test --workspace --locked --exclude victory-gpu --exclude victory-shader`: **PASS** (all 9 crates 100%)
- `cargo test -p victory-shader --lib --locked`: **PASS** (`264 passed, 0 failed, 2 ignored` — all 38 SM 6.6 tests green on RTX 3070 + WARP)
- `cargo test -p victory-gpu --lib --locked -- --test-threads=1`: **PASS** (`198 passed, 0 failed, 4 ignored` — `depth_bounds_pipeline_loads_and_filters_draws` green)

---

## 4. Unclaimed Next-Task Queue (Safe for Parallel Agents)
- **[T-01]** Port `upstream-victory-amd` `9dad973` (48 more shaders: sized-only descriptors + `v_fma_mixlo/hi_f16` split interpolation) — *Scope*: `crates/victory-shader`
- **[T-02]** Port `upstream-victory-amd` `5cfc8d8` (`f64` doubles, `v_permlane16/x16`, wave sums of doubles) — *Scope*: `crates/victory-shader`
- **[T-03]** Evaluate & port `upstream-hyphen` `a26859f` + `8dd30ab` (`VICTORY_LEVELS_ON_DEMAND` saving ~3.1 GB VRAM, byte-copied descriptor mirroring) — *Scope*: `crates/victory-gpu`
- **[T-04]** Port/reconcile `origin/main` `940d32f` (marked-page copy-backs compare only shader-written pages) onto `VI-BR-PORT` `shadow.rs`/`watch.rs` — *Scope*: `crates/victory-gpu`
- **[T-05]** Port `upstream-victory-amd` `10ea38e` (HLE kernel reads beside & copied into title memory) onto `dstorage.rs`/`files.rs` — *Scope*: `crates/victory-hle`
