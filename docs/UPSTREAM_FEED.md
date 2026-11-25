# Upstream Repository Feed & Community Radar

Live tracker to prevent duplicating solved work across the community.  
All 6 tracked VIctory-family remotes are mirrored as `backup/*` branches on private `DrTardis1963/victory-unified`.

---

## 1. Tracked Git Remotes & Backup Status (Last Sync: 2026-11-25)

| Remote | Upstream Repo | Backup Branch (`unified`) | Head Commit | Integration Status |
| :--- | :--- | :--- | :--- | :--- |
| `origin` | `https://github.com/Marvisak/VIctory` | `backup/origin-main` | `940d32f` | Partial (`VI-BR-PORT` ported 33+ fixes; `940d32f` queued `[T-04]`) |
| `upstream-vi-br-port` | `https://github.com/edsuuu/VI-BR-PORT` | `backup/vi-br-port` | `7a30f50` | **Spine** (`synthesis/unified` base; 8 GB VRAM spill, ROVs, `VICTORY_QUEUE_COPIES`) |
| `upstream-vitoriabr` | `https://github.com/uiliannn/VItoriaBr` | `backup/vitoriabr` | `b8634af` | Harvested (`d0d0d23`, `faca57e`, `708b450`, `32d7848`, `a91ef70`, `467f168`) |
| `upstream-hyphen` | `https://github.com/fxceless/victory-fork` | `backup/hyphen` | `a26859f` | **NEW**: `a26859f`, `8dd30ab` fetched & queued `[T-03]` (`310c6eb` deferred) |
| `upstream-victory-amd` | `https://github.com/JoshuuDev/victory-amd` | `backup/victory-amd` | `bb0fc9c` | **NEW**: `5cfc8d8`, `9dad973` queued `[T-01]/[T-02]`; `46dfc10` already covered (`integer_targets_do_not_blend`) |
| `upstream-zettrox` | `https://github.com/Zettrox/ztrx` | `backup/zettrox` | `90b529f` | Audited (`2119b0e` + `README` & `scripts/play.ps1` only) |

---

## 2. Recent Upstream Developments (Harvest Log)

### `upstream-hyphen/main` (`fxceless/victory-fork`) — Synced 2026-11-25
- **`a26859f`**: Byte-copied descriptor record mirroring (PR #305), buffer view own-copy fallback, card room reservation for buffer copies & targets (`VICTORY_CARD_ROOM_FOR_BUFFERS`), `VICTORY_LEVELS_ON_DEMAND` (PR #747 — saves ~3.1 GB VRAM), 14 more shaders (first-half interp pairs, parked lane-masks, `f64`, lane permutations). → **Queued `[T-03]`**
- **`8dd30ab`**: Multi-view page write guard (from `victory-amd` `c6dc4d8` / PR #115), `10ea38e` kernel read fix, storage physical-page write counting. → **Queued `[T-03]`/`[T-05]`**

### `upstream-victory-amd/victory-amd` (`JoshuuDev/victory-amd`) — Synced 2026-11-25
- **`9dad973`**: 48 more shaders translate & validate (puddles, shallow water sim, deferred textured geometry via sized-only descriptors & `v_fma_mixlo/hi_f16` split interpolation). → **Queued `[T-01]`**
- **`5cfc8d8`**: DXIL `double` (`f64`), `v_permlane16_b32`/`v_permlanex16_b32`, and whole-wave `f64` sums (3/4 water shaders translate). → **Queued `[T-02]`**
- **`46dfc10` / `5c659bd`**: Refused pipeline state logging + disabling blending on unblendable integer render targets (`R8_UINT`). → **Already covered in spine** (`integer_targets_do_not_blend` in `VI-BR-PORT`).

---

## 3. External Radar (To Monitor Before Implementing New Fixes)
- **Trackers**: `https://portwatch-vi.xyz/`, `https://gta6recomp.com/repo`, `https://x.com/GTA6ModdingFeed` (recent screenshots show `"Gone Fishing"` boat chase, plane interior, and cutscene rendering in VIctory at ~1–3 FPS).
- **Secondary Reference Repos** (check only for unsolved shader/HLE gaps — clean-room reimplementation only):
  - `https://github.com/xcl-dev/xcl` (Rust Wine-style Scarlett layer, 26,698 HLSL/DXIL shaders)
  - `https://github.com/R0Y00/Xbox2Windows` (C++/DXIL static recompiler)
  - `https://github.com/brcly/TuxBox` (Linux/Proton compatibility layer)
  - `https://github.com/Freminet/ScarlettX` (HLE runtime coverage)
  - `https://github.com/Ahmet-Dedeler/bcpack-rs` (Rust BCPack reference)
