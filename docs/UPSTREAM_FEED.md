# Upstream Repository Feed & Community Radar

Live tracker to prevent duplicating solved work across the community.  
All 6 tracked VIctory-family remotes are mirrored as `backup/*` branches on private `DrTardis1963/victory-unified`.

---

## 1. Tracked Git Remotes & Backup Status (Last Sync: 2026-11-25)

| Remote | Upstream Repo | Backup Branch (`unified`) | Head Commit | Integration Status |
| :--- | :--- | :--- | :--- | :--- |
| `origin` | `https://github.com/Marvisak/VIctory` | `backup/origin-main` | `940d32f` | Partial (`VI-BR-PORT` ported 33+ fixes; `940d32f` queued `[T-04]`) |
| `upstream-vi-br-port` | `https://github.com/edsuuu/VI-BR-PORT` | `backup/vi-br-port` | `7a30f50` | **Spine** (`synthesis/unified` base; 8 GB VRAM spill, ROVs, `VICTORY_QUEUE_COPIES`, sharded remap locks, partial-mip staging, `f64` wave sums) |
| `upstream-vitoriabr` | `https://github.com/uiliannn/VItoriaBr` | `backup/vitoriabr` | `d396d11` | **Ported key fixes** (`4b9872a` `dot4AddPacked`, `0d1c4e0`/`dcc0305` formats `189..=191`; verified `5603b45` already covered by `VI-BR-PORT` `SLOT_ADDRESSES`; `40beb26` queued `[T-02]`) |
| `upstream-hyphen` | `https://github.com/fxceless/victory-fork` | `backup/hyphen` | `9376fd6` | **Ported** (`310c6eb` 4 KiB stale rule & reupload-when-full, `a26859f` `sync_heaps` + `VICTORY_VIEW_OWN_COPY` + `VICTORY_CARD_ROOM_FOR_TARGETS_MB`, `9376fd6` multiply-fold `record_print` + draw-table skip + PR #670 write-watch skip; `tiles.rs` N/A on spine) |
| `upstream-victory-amd` | `https://github.com/JoshuuDev/victory-amd` | `backup/victory-amd` | `003beef` | **Verified covered in spine** (`9dad973` sized-only/first-half interp, `5cfc8d8` `f64` wave sums/`v_permlane16`, `6c5e963`/`948e9d3` per-range `MAPPING` locks) |
| `upstream-zettrox` | `https://github.com/Zettrox/ztrx` | `backup/zettrox` | `90b529f` | Audited (`2119b0e` + `README` & `scripts/play.ps1` only) |

---

## 2. Visual & Performance Merge Priority Plan (Diagnosed from Baseline Harness)

1. **[DONE — Wave A & B] `upstream-hyphen` (`310c6eb`, `a26859f`, `9376fd6`) & `upstream-vitoriabr` (`4b9872a`, `0d1c4e0`/`dcc0305`)**:
   - **Merged**: Packed-texture 4 KiB `Rule::Start` stale fix + `VICTORY_REUPLOAD_WHEN_FULL` (`d667f0b`); write-watched `sync_heaps` + `record_print` + `VICTORY_HEAP_SYNC_TABLES` skip + `VICTORY_VIEW_OWN_COPY` + PR #670 `known_view` write-watch skip + `VICTORY_CARD_ROOM_FOR_TARGETS_MB` (`709d5a9`); `dot4AddPacked` (`0a07d25`) and DXGI formats `189..=191` (`5cd86fe`).
   - **Verified already in `VI-BR-PORT` spine**: `5603b45` (`SLOT_ADDRESSES` stride), `9dad973` (sized-only descriptor & first-half interp), `5cfc8d8` (`f64` wave sums & `v_permlane16`/`x16`), and `6c5e963` (per-range `MAPPING` locks in `watch.rs` & `xmem.rs`). (`tiles.rs` / `VICTORY_LEVELS_ON_DEMAND` N/A because spine uses partial-mip staging.)
2. **[Phase 2C] Remaining Shader & Validation Work**:
   - Fix compute shader `#f133e35efed2a48a` (`CreateComputePipelineState` `E_INVALIDARG` despite clean `dxil.dll` validation), audit `upstream-vitoriabr` `40beb26` (LDS bounds), and reconcile `origin/main` `940d32f`.
3. **[Wave D — Ray-Traced Shadows & Lighting] Enable & Harden `VICTORY_BVH_BUILDER=1`**:
   - Validate safely with `VICTORY_BVH_QUARANTINE=1` (`cargo xtask harness prepare --profile bvh-test`) once Wave A/B A/B comparison completes.

---

## 3. External Radar (To Monitor Before Implementing New Fixes)
- **Trackers**: `https://portwatch-vi.xyz/`, `https://gta6recomp.com/repo`, `https://x.com/GTA6ModdingFeed` (recent screenshots show `"Gone Fishing"` boat chase, plane interior, and clear lit daytime/cutscene rendering in VIctory at ~1–3 FPS).
- **Secondary Reference Repos** (clean-room reimplementation only): `xcl-dev/xcl`, `R0Y00/Xbox2Windows`, `brcly/TuxBox`, `Freminet/ScarlettX`, `Ahmet-Dedeler/bcpack-rs`.
