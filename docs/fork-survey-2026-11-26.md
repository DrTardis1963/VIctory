# Fork survey — 2026-11-26

What other VIctory forks have already solved, checked against our spine
(`synthesis/unified`) and upstream `Marvisak/VIctory` `main` (`940d32f`).

**Method.** All 1,122 GitHub forks of `Marvisak/VIctory` were compared with
upstream `main` through the compare API. 79 have commits of their own; 34 have
10 or more. For those 34 (plus `Belobug/VIctory-Belobug-AMD`, which is not a
GitHub fork but is credited by several), commit subjects were collected and
every subject already present on any local ref was dropped. That left
**1,521 unique subjects not seen locally**.

**Caveat.** A subject match is evidence, not proof. Our spine ported many fixes
under new subjects, so "not found by subject" means "check the code before
porting", not "missing".

---

## 1. Our uncommitted work is already solved upstream

The 8 uncommitted files on `legal/policy-and-runtime-guards` promote
table CBVs to root CBVs (`TABLE_CBV_SPACE`, `table_cbvs`, `try_table_cbv`) to
stop per-draw descriptor-table copies.

| Where | Commit | Approach | On our spine? |
| :--- | :--- | :--- | :--- |
| Upstream `main` | `683ad7a9` "table constants read in place" | A table CBV is read as a **raw view** of the title's slot, so tables need no dynamic slots at all. Also covers dynamically indexed CBV reads. No 16-CBV cap. | **No** |
| VI-BR-PORT / spine | `572a31dd` (port of fxceless PR #144) | 256-byte-aligned table CBVs bound directly at the shadow buffer's GPU address. | Yes |
| Ours (WIP) | uncommitted | Up to 16 static-offset table CBVs become root CBVs, filled from scratch each draw. | — |

**Verdict (done, `fix/table-cbv-raw-views`):** `683ad7a9` cannot be
cherry-picked: it rests on upstream `c75fdba3` (2,180 lines; every heap
constant buffer read as a raw view, a new title-memory change journal), which
the spine replaced with VI-BR-PORT's design. Its idea was ported spine-native
instead: translated shaders read **table** constant buffers as raw bytes
(`lower::table_constants`), and the fresh table slots hold a raw view of the
buffer's shadow copy where it lies on 16 bytes (`draw::table_constant`), else
an upload copy. Heap-direct and root constant buffers are unchanged. The
uncommitted WIP is kept in a stash ("table-CBV-to-root-CBV WIP…"). Our branch
base (`8e79c80e`) is 331 commits behind upstream `main`; the spine is 281
behind.

---

## 2. Tracked forks: new since the 2026-11-25 sync

| Remote | New commits | Worth porting |
| :--- | :--- | :--- |
| `upstream-hyphen` (fxceless) | `f85101b6`, `a2537066`, `0b0074e0` | `VICTORY_ASYNC_SUBMIT` (submit thread), frame log + card meter, driver watch (event-log TDR warning, 2.7–55 s ahead), self-copy of shared host texture removed (402 per 5 min), `VICTORY_COPY_PLAIN`, fast-core thread affinity. Measured RTX 4070: 4,500 draws 145 ms → 124 ms. |
| `upstream-vitoriabr` | 15 (mostly the hyphen ones) | `2b3c339e` quad reads incl. helper lanes (lines on car paint/glass/skin — **not on spine**), `8cb509bc` compile vertex pipelines before mesh/compute when cache is cold, `b37344f5` dynamic allocation test fix. |
| `upstream-vi-br-port` | 3 | README / community files only. |
| `upstream-victory-amd`, `zettrox`, `all-forks` | 0 | — |

---

## 3. Untracked forks worth a look (ranked for an RTX 3070 8 GB, Phase 3 perf)

| Fork | Ahead | What it solved that we lack (by subject) |
| :--- | ---: | :--- |
| **PainDe0Mie/VIctory** | 324 | Staging buffers carved from 64 MB upload slabs (staging creation 7,170 → 399 ms per run; median world frame 534 → 416 ms). Reaper thread for driver object destruction (472 → 358 ms). Textures refreshed in parallel + batched `QueryWorkingSetEx` (108 → 38 ms). BVH builder: dispatches that count threads (`USER_THREADS`), coherent builder views — all 53 builder programs run on RTX 5070 Ti. High-performance adapter selection. |
| **Belobug/VIctory-Belobug-AMD** | n/a | Unmapped holes cached in `watch::guard` ("2.5 GB/frame fault storm"), per-region guard epochs, async submissions, large buffers placed in VRAM, uncapped pacing. Rays: memory by address, coherent builders. Constant indirect light while rays are off (fixes magenta foliage). Hoisted indirect clamps. |
| **alex3112/VIctory** | 474 | Texture and staging memory in slabs, parallel copies; placed textures only where their pages are; unchanged textures/tiles checked less often; rays hit nothing where the BVH can't be read. |
| **Machad3x/VIctory-nvidia-3080ti** | 114 | NVIDIA-specific: structured buffer views made raw (NVIDIA reads 0 through raw loads of structured views); capped view heaps keep full dynamic range; copy-backs put back only bytes the GPU changed; loose title memory copied in 256 KiB windows; page bitmaps in VRAM; acceleration-structure view address. |
| **Xeltax/VIcky** | 243 | Skip unchanged bindless bounce copies, cache partial shadow pages, bound placed-texture VRAM; a Series S mode at 5.7–6.0 fps. |
| **JMAN730/project-flamingo** (42★) | 776 | Streamed-mip residency: host SRVs narrowed to resident mips (VRAM saver); heap binding by creation id (ABA); many shader accuracy fixes (`v_fma_mix*` round-to-odd, `v_mad_u32_u16`, `v_lerp_u8`, d16 format loads, POPS). `image_bvh` lowered as a miss so ray-query programs translate. |
| **1984742628**, **gmankab** | 52 / 62 | Series S profile with lower VRAM + FSR upscaler — relevant to an 8 GB card. Explicitly serialized root signatures for the PresentX / upscaler passes. |
| **bodnjenie14/VIctory** | 29 | "32K LDS OOM" fix and constant lane masks in Wave32 `local_any`: 52/52 BVH builder PSOs on RTX 5070 Ti. Lead for our `#f133e35efed2a48a` `E_INVALIDARG` (spine only refuses LDS past the limit, `a3f219ec`). |
| **reviewbd**, **justr4mzi**, **giritsu** | 29 / 108 / 67 | NVIDIA runtime batched readbacks + faster shadow scans; view-memory guard and relay copy-back optimisations; shadow buffers in DEFAULT VRAM (reverted there). |

Skip for a Windows host: `Bezouro` (Linux native runtime), `crashbashash` (Proton).

---

## 4. Map to our open tasks

| Our task | Already solved elsewhere? |
| :--- | :--- |
| Uncommitted table-CBV promotion | **Yes** — upstream `683ad7a9`; ported spine-native (see §1). |
| `[T-03]` reconcile upstream `940d32f` | Still open; upstream is 281 commits past the spine. |
| `[T-04]` compute `#f133e35efed2a48a` `E_INVALIDARG` | No direct fix found. Lead: bodnjenie14 `52e65197` (LDS > 32 KiB). |
| Wave D: `VICTORY_BVH_BUILDER=1` | Partly: PainDe0Mie `40e0b89b` + Belobug rays + bodnjenie14 get all builder programs running on RTX 50-series; frames still slow to 5–6 s and crash after ~1 min. |
| Phase 3: 30 fps | Biggest measured wins: PainDe0Mie slabs/reaper/batched refresh (~-33% world frame), hyphen async submit + copy plain, Belobug guard-hole cache. |
| 8 GB VRAM | flamingo resident-mip SRVs, Xeltax placed-texture VRAM bound, Series S low-VRAM profile. |
