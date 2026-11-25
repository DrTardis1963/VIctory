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
| Claude Code | `.worktrees/claude-window-trim` | `claude/window-trim-profile` | `trim-8gb` harness profile (1 MiB window grid + window tails) and `nvidia-smi dmon` PCIe log in every prepared run; awaiting owner A/B run vs `tuned-8gb` | `IN PROGRESS` | 2026-11-25 |

---

## 2. Definitive Roadmap Progress

| Phase | Milestone | Status | Notes |
| :--- | :--- | :--- | :--- |
| **Phase 0** | Host & Sandbox Setup | `DONE` | Testing copy (`109 files`, `208.62 GB`) + `victory.toml` + Agility SDK `1.614.1` SM 6.6 unlock verified on Win10 + RTX 3070. |
| **Phase 1** | First Boot & Baseline Telemetry | `DONE` | `victory warm` (`25,750` translated, `2,725` compute pipelines); visible 1080p boot + `xtask harness` baseline captured (`1.8 fps` / `527.8 ms` 3D p50, `7,820 / 6,743 MB` VRAM, `6,352 MB` PC spill, `411.4 ms` CPU recording, `128,783` null texture slots, BVH off, `#f133e35efed2a48a` `E_INVALIDARG`). |
| **Phase 2** | 100% Shaders & Full Visual Fidelity | `IN_PROGRESS` | **Wave A+B + 8 GB Regression/Egress Fixes**: Fixed `targets_room_kept` 8 GB starvation (`0` on `<9 GiB`), skipped `start=0x0` false stale evictions, widened texture eviction age to `30 s` (`spill_room` active), added `VICTORY_TEXTURE_MIP_DROP_MIN_SIZE=1024` & `VICTORY_TEXTURE_ROOM_PERCENT=94` to `tuned-8gb`/`bvh-test`, and blocked `iphlpapi.dll` `Icmp*` raw ICMP egress (`net.rs`). Next: A/B `tuned-8gb` + `bvh-test` + Phase 2C (`#f133e35efed2a48a` & `40beb26`). |
| **Phase 3** | 30 FPS Stable @ 1080p | `PENDING` | Eliminate queue flushes (`VICTORY_QUEUE_COPIES`), shadow copy-backs, tune 8 GB VRAM residency (`VICTORY_CARD_ROOM_FOR_TARGETS_MB`). |
| **Phase 4** | 60 FPS Stable @ 1080p/1440p | `PENDING` | Decouple 30 Hz pacing, multi-core command translation, async compute queue, DXIL wave32 opts. |
| **Phase 5** | 60+ FPS, 100% Completion & Polish | `PENDING` | Uncapped pacing, `<16 GB` RAM target, full playthrough verification. |

---

## 3. Last Verification Gate
- `cargo xtask policy`: **PASS** (`183 files ok`)
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: **PASS** (0 warnings)
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`: **PASS** (0 warnings)
- `cargo test --workspace --locked --exclude victory-gpu --exclude victory-shader`: **PASS** (`198 passed, 0 failed, 5 ignored` prior run; latest post-edit `victory-hle`: `98 + 1 passed`, `xtask`: `10 passed`)
- `cargo test -p victory-shader --lib --locked`: **PASS** (`265 passed, 0 failed, 2 ignored` — unchanged since prior workspace run)
- `cargo test -p victory-gpu --lib --locked -- --test-threads=1`: **PASS** (`207 passed, 0 failed, 4 ignored` — includes `targets_room_reservation_respects_budget_and_override` & `zero_address_views_do_not_make_each_other_stale`)
- Shader diagnostic note (`#f133e35efed2a48a`): `dxil.dll` validation passes (`26,703` IR instructions, `52` used resources), `CreateComputePipelineState` fails with `E_INVALIDARG`; queued for Phase 2C root-signature / compute state inspection.

---

## 4. Unclaimed Next-Task Queue (Safe for Parallel Agents)
- **[T-01]** Live A/B telemetry comparison on rebuilt `diag` `tuned-8gb` profile vs `baseline_20261125_203607` — *Scope*: `F:\VIctory_Testing\` + `cargo xtask harness`
- **[T-02]** Audit `upstream-vitoriabr` `40beb26` (LDS bounds) — *Scope*: `crates/victory-shader`
- **[T-03]** Reconcile `origin/main` `940d32f` (if not already covered by spine) — *Scope*: `crates/victory-gpu`
- **[T-04]** Fix compute shader `#f133e35efed2a48a` (`CreateComputePipelineState` `E_INVALIDARG`) & validate `VICTORY_BVH_BUILDER=1` with `VICTORY_BVH_QUARANTINE=1` — *Scope*: `crates/victory-shader`, `crates/victory-gpu`
