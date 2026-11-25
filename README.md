# VIctory-fork: the 8 GB card build

A fork of [VIctory](https://github.com/Marvisak/VIctory) (by way of
[fxceless/victory-fork](https://github.com/fxceless/victory-fork)), the
compatibility layer that runs an Xbox Series X|S title natively on Windows
from files the user supplies. This fork aims at one kind of machine: a
mid-range NVIDIA card with **8 GB** of memory and 32-lane waves (tested on an
RTX 4060 Ti) driving a **1080p** monitor, where the upstream trees run out of
card memory, stall on texture checks and wait minutes for shaders.

**This repository contains no game files, code or assets.** You need your own
copy of the game. It is not affiliated with or endorsed by the game's
developer or publisher.

> **Resumo (pt-BR):** fork do VIctory focado em placas NVIDIA de 8 GB
> (RTX 4060 Ti) em monitor 1080p. Chega ao mundo e passa a cutscene de
> abertura; ainda não é jogável (cerca de 3 fps). A branch de trabalho é
> `base/fxceless`.

The working branch is **`base/fxceless`**: fxceless' tree, every upstream
fix that applied, and this fork's own changes below.

## Progress (2026-11-25)

Windows 11, Intel i7-13700K, RTX 4060 Ti 8 GB, Series X profile. The
percentages are our own estimate of each stage, not a benchmark.

| Stage | Progress | Notes |
|-------|---------:|-------|
| Loads and boots | 100 % | about 10 s to the first frame |
| Intro video and menus | 100 % | steady 30 fps (the title's own limit) |
| Opening cutscene | 70 % | plays to the end; skin and clothes come out black and video frames flash green when the card is full (see below); colour banding in the grading is being tracked down |
| Reaching the world | 100 % | pipelines come from the saved library; no black stall |
| Playable (input, HUD, menus) | 50 % | pad input, HUD and pause menu work, but at about 3 fps |
| Picture quality | 50 % | no ray-traced light yet (shadows black), one mip dropped on 8 GB cards |
| Frame rate in the world | 10 % | about 3 of the 30 fps target: GPU ~120-200 ms, preparing ~100-250 ms, copying back ~50-250 ms per frame |
| Stability | 90 % | hour-long runs with no hang or device loss |
| **Overall** | **74 %** | the average of the rows above |

## What this fork adds

Memory, for 8 GB cards:
- The display mode the title is told about can match the monitor
  (`VICTORY_DISPLAY_MODE=1920x1080`): at the default 4K mode its render
  targets alone took 3.4 GB of a 6.6 GB budget, and textures were given up.
- One mip is dropped from large textures on cards of 8 GB or less (floor 2048).
- Render targets whose title memory is unmapped and idle are let go; targets
  at the same address with the same description share one host copy.
- Textures picked by index are given up oldest first when the card is full,
  and come back when the title names them again.
- A line in the log tells the card's memory by kind.

Speed:
- The shader translator runs about 2.7x faster cold, with the same output.
- Texture checks hold only the ranges they read while the runtime remaps
  title memory, and wait for nothing (`VICTORY_TEXTURE_NO_WAIT`); a per-frame
  budget (`VICTORY_TEXTURE_BUDGET_MS`, 10 by default) carries the rest over.
- Console texture detiling runs in parallel bands.
- Copy-back of card copies runs on its own copy queue; the card compares
  pieces with wide groups.
- Cutscene frames that waited 1.6-7.9 s for texture uploads now take 5-67 ms.
- Spare pipeline workers during loads; the world opens with its pipelines
  from the library instead of a black stall.

Correctness:
- Programs written for 64-lane waves run as several 32-lane waves with group
  barriers (22 of the 24 hair programs now create).
- Textures a list uploads in its prelude no longer flicker in the intro.
- A crash in guarded DirectStorage reads is fixed.
- Compute dispatches past the frame's wait budget are dropped instead of
  stalling the frame.

From upstream and other forks: fxceless' tree and fixes (skin and hair
textures looked at again after their upload, view-heap clamp, f64
instructions, idle eviction under memory pressure, ...), and more than forty
VIctory pull requests (helper lanes in `v_readfirstlane`, mesh edge flags,
d16 loads, title writes under shader-written ranges, ...).

### Recommended switches for an 8 GB card

```powershell
$env:VICTORY_RELAY = '1'          # 2 shows defects on 8 GB cards
$env:VICTORY_WIDE_BACK = '1'
$env:VICTORY_WIDE_REFRESH = '1'
$env:VICTORY_DISPLAY_MODE = '1920x1080'   # your monitor's size
$env:VICTORY_NO_WATCHDOG = '1'    # the first world frames are slow
```

The first run after the shader translator changes rebuilds the pipeline
library: the opening cutscene is mostly black for a few minutes while about
3,000 pipelines are made. Later runs load them.

### Known problems

- Everything in shadow is black: the title's ray-traced light does not run yet.
- Colour banding in the cutscene's grading.
- Two 64-lane programs (triangle collection and meshlets) still do not fit
  32-lane waves and are refused.
- Around 24 GB of system memory is in use during play.

## Credits

- **[VIctory](https://github.com/Marvisak/VIctory)** by Marvisak and its
  contributors is the base of everything here: the loader, the runtime
  services, the Direct3D 12 back end and the shader recompiler. Licensed
  GPL-3.0-or-later; so is this fork.
- **[fxceless/victory-fork](https://github.com/fxceless/victory-fork)** and the
  forks it gathers (asdaasdsaddf, triippywhale and others) for the world
  rendering, relay and memory work this fork starts from.
- **[XCL](https://github.com/ivall/xcl)** (GPL-2.0-only) was used only as a
  reference for how the title behaves under another layer; nothing was copied.
- This fork's changes were developed with [Claude Code](https://claude.com/claude-code).

Everything below this line is upstream VIctory's own README, unchanged; its
status section describes upstream, not this fork.

---

# VIctory

Runs the title (Xbox Series X|S) natively on Windows, from files
the user supplies.

The title's x86-64 code runs unchanged inside our process; VIctory supplies
the platform underneath it and converts the title's data, ahead of time where
it can and on first use where it must (shaders). See
[docs/architecture.md](docs/architecture.md).

## Status (2026-11-23)

**Not playable yet.** The title boots, runs its menus at its own 30 fps and
loads the world, where rendering fails.

| Stage | State |
|-------|-------|
| Boot, first-boot screens (accessibility, language, settings, legal) | works, 30 fps |
| Intro video with sound | works |
| Start screen, profile and saves | works |
| Loading into the world (about 50 s) | works |
| The world (first mission's intro) | reached and runs with indirect draws; its title card renders, the scene after it is still dark; 0.5-1.8 s per frame |

Tested on one machine: Windows 11, AMD RX 6700 XT.

> **Warning.** Entering the world on a real GPU can currently hang it, and
> Windows then resets the display driver (every monitor goes black for a
> moment). To look further, use a diagnostics build with
> `VICTORY_D3D12_WARP=1`, which renders on the software adapter.

What each part does today and what it still needs:

| Subsystem | Today | Needs |
|-----------|-------|-------|
| Loader | Runs the title's executable and its DLLs in our process; crash reports name the faulting code and are kept in `crashes.log` | – |
| Runtime services | One signed-in offline user, licences, packages, saves that survive a kill at any step, text entry dialog | Speech is silent; the dialog and relaunch are untested live |
| Network | Offline by design: the title cannot reach the network | – |
| Files and streaming | Archives and streamed data read, zlib and BCPack decoded in parallel | Loading is limited by writes into title memory |
| Input | XInput pads, keyboard, keyboard stand-in pad | Trigger rumble |
| Audio | Menu, video and loading sound; XMA voices and Opus streams decode | Reverb is silent; Opus results unverified against the title |
| GPU: menus | Draws, compute and presentation on host D3D12; only changed title memory is copied | Frame pacing drifts on one screen in release builds |
| GPU: world | Mesh pipelines, shader-written buffers, descriptor lifetimes, indirect execution (arguments clamped), pipelines made by worker threads and kept on disk | Most of the scene is still missing (untranslated shaders drop draws); frames take seconds; real queries, depth bounds, HDR and scaling are missing |
| Shaders | Console GPU code recompiled to DXIL at first use, cached on disk. Menus: all. World: mesh 8,191 of 8,506, vertex/pixel 10,617 of 12,346, compute 1,643 of 2,539 | 64-bit image atomics, merged geometry stages, the ray-tracing builder's shaders; some compute programs need a GPU with 64-lane waves |
| Textures | Console tiled layouts and detiling for the modes the title uses | Render-target metadata planes |
| Series S mode | Runs through the menus to the loading screen | The world, as above |

## Getting started

Requirements: Windows 10/11 x64, a CPU with AVX2, a Direct3D 12 GPU with
shader model 6.6 (and mesh shaders for the world), Rust (pinned in
`rust-toolchain.toml`).

The title also needs several GiB of available Windows commit at startup.
Check **Committed** under **Task Manager → Performance → Memory**. If
`crashes.log` reports Windows error 1455, close other applications or increase
the Windows paging file size, then retry.

```powershell
cargo xtask install-hooks                # asset-policy check on every commit
copy victory.example.toml victory.toml   # then set your paths (outside the repo)
cargo test --workspace                   # needs no game data
cargo run -p victory -- config           # check your paths
```

Run the title:

```powershell
cargo build --release -p victory
target\release\victory.exe run <game dir>\<the title's exe> --execute
```

The window caption shows the framerate. Saves, settings and `crashes.log`
are kept under the data folder (default `%LOCALAPPDATA%\victory\data`), derived
data under the cache folder.

Input: XInput pads. With none connected the keyboard is a pad while the
title's window has focus:

| Keys | Pad |
|------|-----|
| W A S D / I J K L | left / right stick |
| arrows | D-pad |
| Enter, Backspace, X, Y | A, B, X, Y |
| Q / E, Z / C | shoulders, triggers |
| F / H | stick clicks |
| Esc, Tab | Menu, View |

## Commands

```
victory inspect <exe|dll> [--imports] [--exports]
victory census [paths...] [--missing] [--strict]
victory lib-exports <import.lib> [--list]
victory shader-info [paths...] [--verbose] [--disasm]
victory translate [paths...] [--validate] [--pipelines] [--list] [--jobs N]
victory run <exe> [--execute] [--unimplemented trap|zero] [--strict]
            [--console series-x|series-s] [-- <title arguments>]
victory config
```

Logging: `VICTORY_LOG=victory_loader=trace,info` (a release build logs
info and above).

## Debugging

A release build has no debugging aids. Build them in with

```powershell
cargo build --profile diag -p victory --features diagnostics   # target\diag\victory.exe
```

The on/off switches below (`...=1`) are off when unset or set to `0`.

| Variable | Effect |
|----------|--------|
| `VICTORY_LOG=<target>=debug,...` | debug and trace output |
| `VICTORY_STACKS=20,60` | every thread's stack at those seconds |
| `VICTORY_WATCH=<rva>,...` | logs a title u32 when it changes |
| `VICTORY_KEYS=30:Enter,35:X` | presses keys at those seconds, focus or not |
| `VICTORY_CRASH_SAVE=<rva>:<len>,...` | saves title memory to the cache folder on a crash |
| `VICTORY_CAPTURE=<dir>` | saves presented frames (powers of two, or `VICTORY_CAPTURE_FRAMES=<list>`) |
| `VICTORY_CAPTURE_SYNC=1` | finishes each presented frame's PNG before presenting it (normally PNG encoding runs on a worker) |
| `VICTORY_DUMP_TARGETS=1` | with the two above, saves every render target of those frames |
| `VICTORY_SHOW_TARGET=2560x1440-f28` | shows the last render target of that size and DXGI format each frame instead of the title's frame (copied 1:1 to the top left, only when its format matches the window's), e.g. the world's geometry while its lit image is black |
| `VICTORY_D3D12_DEBUG=1` | host debug layer |
| `VICTORY_D3D12_GBV=1` | with the above, GPU-based validation (bad descriptor use reported, not faulted; slow) |
| `VICTORY_D3D12_DRED=1` | on a GPU failure, the allocation hit and the commands in flight |
| `VICTORY_PIPELINE_WAIT_MS=24` | set the per-frame wait for background pipeline creation (default: 500 ms for the first 60 frames, then 8 ms). A draw can be omitted until its pipeline is ready |
| `VICTORY_SKIP_PIPELINES=<hash>,...` | omit draws and dispatches, including indirect ones, whose pipeline contains a listed 16-digit shader hash |
| `VICTORY_GPU_TIMING=1` | with the above, each list of a submission run and timed on its own; slow lists logged with their slowest commands |
| `VICTORY_RELAY_COPY_QUEUE=0` | with the relay, card copies' changed pieces are fetched on the direct queue, behind every list sent since, as before (default: a copy queue of their own) |
| `VICTORY_CARD_HASH_WIDE=0` | the passes that find what shaders wrote to copies in the card's memory hash with one thread per 64 KiB, as before |
| `VICTORY_D3D12_WARP=1` | host device on the software adapter (slow; cannot reset the driver) |
| `VICTORY_SHADER_LOOP_LIMIT=<n>` | translated shader loops stop after `n` iterations per lane instead of 2^20 (own shader cache entries); if a lower `n` makes world frames much faster, loops hitting the limit are what makes them slow |
| `VICTORY_AUDIO_CAPTURE=<file.wav>` | records what the title plays (our process only) |
| `VICTORY_XMA_DUMP=<dir>` | saves the first 32 XMA voices' context and input |

Every build appends crashes and host GPU failures to `crashes.log` in the
title's data folder.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
