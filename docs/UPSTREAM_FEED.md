# Upstream Repository Feed & Community Radar

Live tracker to prevent duplicating solved work across the community.  
All 6 tracked VIctory-family remotes are mirrored as `backup/*` branches on private `DrTardis1963/victory-unified`.

---

## 1. Tracked Git Remotes & Backup Status (Last Sync: 2026-11-25)

| Remote | Upstream Repo | Backup Branch (`unified`) | Head Commit | Integration Status |
| :--- | :--- | :--- | :--- | :--- |
| `origin` | `https://github.com/Marvisak/VIctory` | `backup/origin-main` | `940d32f` | Partial (`VI-BR-PORT` ported 33+ fixes; `940d32f` queued `[T-04]`) |
| `upstream-vi-br-port` | `https://github.com/edsuuu/VI-BR-PORT` | `backup/vi-br-port` | `7a30f50` | **Spine** (`synthesis/unified` base; 8 GB VRAM spill, ROVs, `VICTORY_QUEUE_COPIES`) |
| `upstream-vitoriabr` | `https://github.com/uiliannn/VItoriaBr` | `backup/vitoriabr` | `d396d11` | **NEW**: Synced `hyphen` `9376fd6` + `5603b45` (indexed view stride), `4b9872a` (`dot4AddPacked`), `40beb26` (LDS bounds), `0d1c4e0`/`dcc0305` (formats) |
| `upstream-hyphen` | `https://github.com/fxceless/victory-fork` | `backup/hyphen` | `9376fd6` | **NEW**: `310c6eb` + `8dd30ab` + `a26859f` + `9376fd6` (first-room textures, `VICTORY_LEVELS_ON_DEMAND`, ~3 ms heap sync, PR #670/#648) |
| `upstream-victory-amd` | `https://github.com/JoshuuDev/victory-amd` | `backup/victory-amd` | `003beef` | **NEW**: `9dad973` (+48 shaders), `5cfc8d8` (`f64` wave sums), `948e9d3`/`476a020`/`70ff6e2`/`6c5e963` (per-16 MB remap locks, >50% p90 frame-time cut) |
| `upstream-zettrox` | `https://github.com/Zettrox/ztrx` | `backup/zettrox` | `90b529f` | Audited (`2119b0e` + `README` & `scripts/play.ps1` only) |

---

## 2. Visual & Performance Merge Priority Plan (Diagnosed from Baseline Harness)

1. **[Wave A — Visual Parity & 8 GB VRAM Relief] `upstream-hyphen/main` (`310c6eb`..`a26859f`) / `upstream-vitoriabr/main` (`b401954`)**:
   - **Solves**: Peak `7,820 MB > 6,743 MB` VRAM over-budget (`6,352 MB` PCIe spill) & `128,783` texture slots made null (missing skin/character/environment textures).
   - **Key mechanisms**: Packed-texture stale-range fix (`310c6eb`), byte-copied descriptor mirroring + own-copy buffer views + `VICTORY_LEVELS_ON_DEMAND=1` saving ~3.1 GB VRAM (`a26859f`), multi-view page guards (`8dd30ab`).
2. **[Wave B — CPU Recording & Heap Pass Speedup] `upstream-hyphen/main` (`9376fd6`) + `upstream-victory-amd` (`003beef` / `6c5e963`)**:
   - **Solves**: `411.4 ms/frame` CPU recording and `122.1 ms/frame` heap-buffer pass.
   - **Key mechanisms**: Multiply-and-fold descriptor-sync fingerprint + draw-table slot skip (heap pass `70–220 ms -> ~3 ms`), write-watch skip (PR #670), optional inflight card fetches (`VICTORY_CARD_FETCH_INFLIGHT=1`), and per-16 MB `MAPPING` locks (`6c5e963`).
3. **[Wave C — Shader Completeness & Cascade/Water Visuals] `upstream-victory-amd` (`9dad973`, `5cfc8d8`) + `upstream-vitoriabr` (`5603b45`, `4b9872a`, `40beb26`, `0d1c4e0`) + `#f133e35efed2a48a` fix**:
   - **Solves**: +48 puddle/water/deferred shaders (`9dad973`), `f64` wave sums (`5cfc8d8`), indexed view byte stride (`5603b45`), `dot4AddPacked` (`4b9872a`), LDS bounds (`40beb26`), and compute shader `#f133e35efed2a48a` (`CreateComputePipelineState` `E_INVALIDARG`).
4. **[Wave D — Ray-Traced Shadows & Lighting] Enable & Harden `VICTORY_BVH_BUILDER=1`**:
   - **Solves**: `2,534.5` dropped dispatches/frame and dark/black unlit shadows (`VICTORY_BVH_BUILDER` defaults off). Validate safely with `VICTORY_BVH_QUARANTINE=1` (`cargo xtask harness prepare --profile bvh-test`).

---

## 3. External Radar (To Monitor Before Implementing New Fixes)
- **Trackers**: `https://portwatch-vi.xyz/`, `https://gta6recomp.com/repo`, `https://x.com/GTA6ModdingFeed` (recent screenshots show `"Gone Fishing"` boat chase, plane interior, and clear lit daytime/cutscene rendering in VIctory at ~1–3 FPS).
- **Secondary Reference Repos** (clean-room reimplementation only): `xcl-dev/xcl`, `R0Y00/Xbox2Windows`, `brcly/TuxBox`, `Freminet/ScarlettX`, `Ahmet-Dedeler/bcpack-rs`.
