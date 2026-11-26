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
| `upstream-all-forks` | `https://github.com/WillyboPeep/victory-all-forks` | `backup/all-forks` | `ef7f036` | **Security-audited & synthetically harvested (Waves 1–3)**; excluded `.ps1`/`LEIA-ME-GTA6.md`/`ALL-FORKS.md` (asset policy) and deferred `triippywhale` GPU rewrite + PR #282 (`e4e775e`) |

---

## 2. Visual & Performance Merge Priority Plan (Diagnosed from Baseline Harness)

1. **[DONE — Wave A & B] `upstream-hyphen` (`310c6eb`, `a26859f`, `9376fd6`) & `upstream-vitoriabr` (`4b9872a`, `0d1c4e0`/`dcc0305`)**:
   - **Merged**: Packed-texture 4 KiB `Rule::Start` stale fix + `VICTORY_REUPLOAD_WHEN_FULL` (`d667f0b`); write-watched `sync_heaps` + `record_print` + `VICTORY_HEAP_SYNC_TABLES` skip + `VICTORY_VIEW_OWN_COPY` + PR #670 `known_view` write-watch skip + `VICTORY_CARD_ROOM_FOR_TARGETS_MB` (`709d5a9`); `dot4AddPacked` (`0a07d25`) and DXGI formats `189..=191` (`5cd86fe`).
   - **Verified already in `VI-BR-PORT` spine**: `5603b45` (`SLOT_ADDRESSES` stride), `9dad973` (sized-only descriptor & first-half interp), `5cfc8d8` (`f64` wave sums & `v_permlane16`/`x16`), and `6c5e963` (per-range `MAPPING` locks in `watch.rs` & `xmem.rs`). (`tiles.rs` / `VICTORY_LEVELS_ON_DEMAND` N/A because spine uses partial-mip staging.)
2. **[DONE — `WillyboPeep/victory-all-forks` Security Audit & Waves 1–3]**:
   - **Static Security Audit**: 8-vector audit clean (0 Critical, 0 High). Excluded `jogar*.ps1`, `warm-fast.ps1`, `LEIA-ME-GTA6.md`, `ALL-FORKS.md` (`docs/asset-policy.md` violations / repo-root log writes). Preserved `net.rs` ICMP egress block (`174692d`), `policy.rs` traversal fix (`2c0b487`), Agility SDK `build.rs` (`c885884`), and `xtask/src/policy.rs` (`1 MiB`).
   - **Wave 1 (HLE, Loader, Formats)**: APU `Arc` refcount UAF (`e2fa4fc`), XMA ring/loop/noise (`467f168`, `82b6356`, `dd5fc29`), host DLL load outside lock (`708b450`), `fread` 1 MiB chunking (`5db7f22`), `DStorageGetFactory` null-out (`00a8f4f`), GameInput 256-reading history & clear (`e36eae4`, `72ae7dd`), XGameSave safety/quota/migration/version fixes (`c061484`, `a478030`, `27a8f78`, `57c0ba8`, `34c669d`, `e4dffbb`, `ac594b1`), XAsync/XTaskQueue fixes (`c926217`, `09c781a`, `4033fcc`, `209393b`), constrain-before-suspend (`81be15f`), `VICTORY_SKIP_BINK` (`0b2a28c`), `ClzeroLoop`/`clwb`/TEB/case-insensitive DLL lookup with traversal guard (`bbd2408`, `154065d`, `698c86c`, `37410bb`), and tiled texture chunking (`dcc65d3`).
   - **Wave 2 (Shaders & PSV0)**: PSV0 repeated arbitrary semantic names (`7265f4b`), `image_sample_*_g16` gradients (`4d675d5`), `v_div_fixup_f32` `0/x` & `x/Inf` priority (`6989081`), `pos4` export target 16 (`0f5b186`), `calculateLOD` array slice undef (`18df163`), `annotateHandle` block locality (`329cf50`), `FLAT` bit-11 offset test (`8b699d3`).
   - **Wave 3 (GPU Safety, Correctness & Relay Perf)**: `queue::release` UAF (`6d632d3`), `FenceRef` COM refcounts (`56249f3`), `ID3D12Fence1` (`e0f5550`), `Heap::drop` eviction (`9595b98`), 4 KiB `place()` alignment (`27ba5ec`), 0-query heap guard (`33dc6a1`), empty placed buffer guard (`4684004`), `R9G9B9E5` RGBA16F fallback (`d802b4c`), `SHADER_HEAPS` lock (`f0c33b0`), `RwLock` views table (`b8f8e8a`), SIMD relay merge (`c364c00`), indexed relay flights (`4905bd5`), CPU snapshot retry (`715962f`), ready queues before present waits (`1bf0ddd`), 10-bit swap chain capture (`3dd3801`), `CreateSamplerX` LOD/bias/enum clamping (`e7c125c`), `placed_desc_key` filler dedup (`9680025`), and skipped all-zero partial texture hash (`3d67d98`).
   - **Deferred (Incompatible with `VI-BR-PORT` 8 GB VRAM Spine)**: `triippywhale`'s 37-file RTX 4090 GPU rewrite (`heap_marks.rs`, `heap_pass.rs`, `diagstream.rs`, `SLOT_VIEWS`), `b3cbb4a` (`USER_GROUP_BASE`, requires matching GPU dispatch chunking), and PR #282 `e4e775e` (`Prebuild`, overlaps `victory warm` `0af2409`).
3. **[Phase 2C] Remaining Shader & Validation Work**:
   - Fix compute shader `#f133e35efed2a48a` (`CreateComputePipelineState` `E_INVALIDARG` despite clean `dxil.dll` validation), and reconcile `origin/main` `940d32f`. (Note: `upstream-vitoriabr` `40beb26` LDS bounds is already covered in spine by `fa4a359`.)
4. **[Wave D — Ray-Traced Shadows & Lighting] Enable & Harden `VICTORY_BVH_BUILDER=1`**:
   - Validate safely with `VICTORY_BVH_QUARANTINE=1` (`cargo xtask harness prepare --profile bvh-test`) once Wave A/B A/B comparison completes.
5. **[DONE 2026-11-26] Table constant buffers as raw views** (`fix/table-cbv-raw-views`): the idea of upstream `683ad7a9` ("table constants read in place"), ported spine-native because `683ad7a9` rests on upstream `c75fdba3` (heap constants as raw views + change journal), which the spine does not have. Fresh table slots hold a raw view of the shadow copy (16-byte alignment) instead of a per-draw upload copy for buffers off 256 bytes.
6. **[2026-11-26 all-forks scan]** 1,122 GitHub forks compared with upstream `main`: 79 with own commits, 34 with 10+. Ranked findings (PainDe0Mie staging slabs/reaper, hyphen `f85101b6`/`a2537066`/`0b0074e0`, Belobug guard-hole cache, Machad3x NVIDIA fixes, bodnjenie14 LDS lead for `#f133e35efed2a48a`) in `docs/fork-survey-2026-11-26.md`.

7. **[2026-11-26 community pass 1]** Ported onto topic branches off `synthesis/unified` `97b90e61`, pushed to `fork`, queued for in-game A/B (`F:\VIctory_Testing\loop\requests.jsonl`), not merged:
   - `port/quad-reads-helper-lanes`: vitoriabr/beloooo `2b3c339e` (a pixel's quad read through quad reads, helper lanes included) + a test.
   - `port/staging-pool-slabs`: PainDe0Mie `66ad6e85` (staging pool only), `ec77dc0b` (64 MB upload slabs), `d76c6dd5` (reaper thread); `VICTORY_STAGING_POOL=0` / `VICTORY_REAPER=0` restore the old path.
   - `port/no-self-copy`: fxceless `a2537066`, self-copy part only (a copy between two title textures sharing one host texture is left out).
   - Already in the spine: all-forks `faca57e`, `7291337`, `b3400bb`; Belobug `2b6adbb7` (as `watch::Guards.absent`).
   - Deferred: all-forks `7cdc3ef`/`a1ce5315`/`a498c0cf` (need the heap pass of `claude/heap-pass-copyback`); bodnjenie14 `52e65197` (no measured case); hyphen `f85101b6` async submit (size); ScarlettPort `precompiled.rs` (Series S parked).
   - Next lead: upstream `main`'s texture residency series since `940d32f` (`fdf34ab7`, `8a3d9759`, `52d14b10`, `4959d349`, `69d1e8e7`, `cc5f9612`).
   - Digests: `F:\VIctory_Testing\community\digest-*.md`; state: `F:\VIctory_Testing\community\state.json`.
8. **[2026-11-26 shader-complete survey]** (`F:\VIctory_Testing\community\shader-complete-survey.md`). A 3070 run drops 9 console wave64 compute programs; the corpus baseline's 399 compute `E_INVALIDARG` come from translating without `--host-lanes 32-32` (`WaveSize(64)`). `upstream-vi-br-port` (our base, `7a30f506`) claims 25,758 of 25,758 on a 32-lane card, and its commits name our exact refusals. Ported, not merged:
   - `port/vibr-shaders`: edsuuu `0615710b`, `f237fecc`, `dc75ed82`, `592a5796` + `a75eb970` (dispatch by thread count), `8a1b59cf`, `256af633`, `96981b74`, `6e9de756`, `40d2b1b0`, `783a54ca`, `8b349eee` (`dcfae27a` skipped: duplicate of `0615710b`; `7bcc909c` = `port/quad-reads-helper-lanes`).
   - `port/vibr-lighting`: edsuuu `8b4c86c6`, `561b6fe9`, `1209a642` (after Belobug): constant indirect diffuse while rays are off, reflections held to ambient (`VICTORY_AMBIENT=off` for A/B).
   - Deferred: fxceless/Pilote `26600c61` (paired form + mesh linking; 4,562 lines duplicating the spine's ROV, f64 and descriptor-as-data code), PiloteCode `1f0773bd` (195 voxel GS), `5177d899` (71 mesh > 32 KiB), KomanderDG88 `f3d0483c` (FidelityFX), and the rest of `upstream-vi-br-port` since `7a30f506` (GPU perf, present composition, bakes).

---

## 3. External Radar (To Monitor Before Implementing New Fixes)
- **Trackers**: `https://portwatch-vi.xyz/`, `https://gta6recomp.com/repo`, `https://x.com/GTA6ModdingFeed` (recent screenshots show `"Gone Fishing"` boat chase, plane interior, and clear lit daytime/cutscene rendering in VIctory at ~1–3 FPS).
- **Secondary Reference Repos** (clean-room reimplementation only): `xcl-dev/xcl`, `R0Y00/Xbox2Windows`, `brcly/TuxBox`, `Freminet/ScarlettX`, `Ahmet-Dedeler/bcpack-rs`.
