# VIctory-Unified Live Status & Multi-Agent Board

**Repository**: `https://github.com/DrTardis1963/victory-unified` (private)  
**Target Host**: Windows 10 Pro (`10.0.19045`), i7-9700K, RTX 3070 8 GB  
**External Test Config**: `F:\VIctory_Testing\victory.toml` (`208.62 GB` isolated testing copy verified; `F:\Quarantine\...` untouched)

---

## 1. Active Agent Claims (Lock Table)
*Claim a row before editing code so multiple providers (Antigravity, Claude Code, Codex, etc.) never collide.*

| Agent / Provider | Worktree | Branch | Scope / Task | Status | Updated |
| :--- | :--- | :--- | :--- | :--- | :--- |
| Antigravity | `.worktrees/unified` | `synthesis/unified` | Phase 2 Wave A+B visual/VRAM/CPU fixes merged & verified | `DONE` | 2026-11-25 |
| Existing local work | `.` (root) | `legal/policy-and-runtime-guards` | Uncommitted local edits (8 files) — **DO NOT TOUCH** | `RESERVED` | 2026-11-25 |
| Existing local work | `.worktrees/agy` | `fix/loader-tls-redirect-bounds-and-empty-section` | Existing worktree — **DO NOT TOUCH** | `RESERVED` | 2026-11-25 |

---

## 2. Definitive Roadmap Progress

| Phase | Milestone | Status | Notes |
| :--- | :--- | :--- | :--- |
| **Phase 0** | Host & Sandbox Setup | `DONE` | Testing copy (`109 files`, `208.62 GB`) + `victory.toml` + Agility SDK `1.614.1` SM 6.6 unlock verified on Win10 + RTX 3070. |
| **Phase 1** | First Boot & Baseline Telemetry | `DONE` | `victory warm` (`25,750` translated, `2,725` compute pipelines); visible 1080p boot + `xtask harness` baseline captured (`1.8 fps` / `527.8 ms` 3D p50, `7,820 / 6,743 MB` VRAM, `6,352 MB` PC spill, `411.4 ms` CPU recording, `128,783` null texture slots, BVH off, `#f133e35efed2a48a` `E_INVALIDARG`). |
| **Phase 2** | 100% Shaders & Full Visual Fidelity | `IN_PROGRESS` | **Wave A+B merged**: `dot4AddPacked`, DXGI formats `189..=191`, `Rule::Start` 4 KiB stale rule & `reupload_when_full` (`310c6eb`), `sync_heaps` + `record_print` + `VICTORY_HEAP_SYNC_TABLES` + `VICTORY_VIEW_OWN_COPY` + `PR #670` write-watch skip + `VICTORY_CARD_ROOM_FOR_TARGETS_MB` (`a26859f`/`9376fd6`). Next: live A/B run + Phase 2C (`#f133e35efed2a48a` & `40beb26`). |
| **Phase 3** | 30 FPS Stable @ 1080p | `PENDING` | Eliminate queue flushes (`VICTORY_QUEUE_COPIES`), shadow copy-backs, tune 8 GB VRAM residency (`VICTORY_CARD_ROOM_FOR_TARGETS_MB`). |
| **Phase 4** | 60 FPS Stable @ 1080p/1440p | `PENDING` | Decouple 30 Hz pacing, multi-core command translation, async compute queue, DXIL wave32 opts. |
| **Phase 5** | 60+ FPS, 100% Completion & Polish | `PENDING` | Uncapped pacing, `<16 GB` RAM target, full playthrough verification. |

---

## 3. Last Verification Gate
- `cargo xtask policy`: **PASS** (`183 files ok`)
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: **PASS** (0 warnings)
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`: **PASS** (0 warnings)
- `cargo test --workspace --locked --exclude victory-gpu --exclude victory-shader`: **PASS** (`163 passed, 0 failed, 5 ignored` across 9 crates, including `xtask` 10/10)
- `cargo test -p victory-shader --lib --locked`: **PASS** (`265 passed, 0 failed, 2 ignored` — includes `packed_dot_products_and_clamps_execute_cleanly` & SM 6.6 suite)
- `cargo test -p victory-gpu --lib --locked -- --test-threads=1`: **PASS** (`205 passed, 0 failed, 4 ignored` — includes `packed_neighbours_stay`, `records_copied_as_bytes_are_made_at_the_next_sync`, `a_record_that_changed_prints_otherwise`, `a_view_off_its_copys_elements_gets_its_own_bytes`)
- Shader diagnostic note (`#f133e35efed2a48a`): `dxil.dll` validation passes (`26,703` IR instructions, `52` used resources), `CreateComputePipelineState` fails with `E_INVALIDARG`; queued for Phase 2C root-signature / compute state inspection.

---

## 4. Unclaimed Next-Task Queue (Safe for Parallel Agents)
- **[T-01]** Live A/B telemetry comparison on rebuilt `diag` `tuned-8gb` profile vs `baseline_20261125_203607` — *Scope*: `F:\VIctory_Testing\` + `cargo xtask harness`
- **[T-02]** Audit `upstream-vitoriabr` `40beb26` (LDS bounds) — *Scope*: `crates/victory-shader`
- **[T-03]** Reconcile `origin/main` `940d32f` (if not already covered by spine) — *Scope*: `crates/victory-gpu`
- **[T-04]** Fix compute shader `#f133e35efed2a48a` (`CreateComputePipelineState` `E_INVALIDARG`) & validate `VICTORY_BVH_BUILDER=1` with `VICTORY_BVH_QUARANTINE=1` — *Scope*: `crates/victory-shader`, `crates/victory-gpu`
