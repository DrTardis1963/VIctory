---
name: victory-run
description: Run the title under VIctory for a test (interactive or unattended), with screenshots during the run, no auto-close timers, results outside the repo, and the follow-up reports (crash log, unimplemented-import ranking, frame metrics, comparison with the owner's baseline picture). Use whenever asked to run, test or launch the game.
---

# Running the title

Everything a run writes goes under `F:\VIctory_Testing\` (runs, shots, logs, captures), never inside the repository. Config: `VICTORY_CONFIG=F:\VIctory_Testing\victory.toml`.

## 1. Build and prepare
1. Build the binary to test. Normally `cargo build --profile diag -p victory --features diagnostics` in your own worktree.
2. Make sure the Agility SDK runtime sits beside it: `cargo xtask agility --from F:\VIctory_Testing\D3D12`. Without it, Windows 10 stops at shader model 6.5 and the run fails.
3. Prepare an isolated run folder: `cargo xtask harness prepare --profile <baseline|tuned-8gb|...> ... --capture`. For Series S, add `--console series-s` to the launch line in the generated run.ps1.

## 2. Run
- **Owner watching or playing**: `F:\VIctory_Testing\tools\run-with-shots.ps1 -RunDir <run dir>`.
  - It takes PrintWindow screenshots every 60 s into `<run dir>\shots`.
  - It stops the headless `victory` process 20 s after the window closes.
- **Unattended**: `F:\VIctory_Testing\tools\auto-run.ps1 -RunDir <run dir>`.
  - It presses Enter on the title screens, detects the world at 1000 or more draws per frame, and measures 240 s of world.
  - It moves Lucia: S first, then turn right, walk, turn back.
  - It closes cleanly and writes `result.json`. If no world appears within 15 minutes it reports `stuck_loading`.
  - For a queue of builds, use `matrix.ps1 -Queue <json>`; results are appended to `F:\VIctory_Testing\loop\results.jsonl`.

Never add a short auto-close timer or "watcher". Loading into the world takes 3–10 minutes and logs nothing meanwhile; only screenshots show where it is. Closing the window does not end the process: stop the leftover `victory.exe`. Never run a full capture harness: name the screens you need (`-Only <shot>,...`) and say why first.

## 3. After the run
1. **Pictures**: look at `shots\` at full size, cropping for detail, from the first gameplay frame on. Compare with the owner's target, `F:\VIctory_Testing\reference\baseline_rtx5080_cell_6-7fps.webp`: lit cell walls, an orange "INMATE" jumpsuit with readable print, the chain-link fence and the yard. Report fps next to 6–7. Do not call a picture right without comparing it.
2. **Frames**: `cargo xtask harness analyze <run.log> --out <run dir>` gives the frame waterfall, VRAM, waits and dropped shaders. `cargo xtask harness compare <a>\report.json <b>\report.json` gives an A/B table.
3. **Crashes**: read `crashes.log` in the title's data folder. A crash report lists the faulting module+rva, the stack, and the unimplemented imports called so far.
4. **Missing functions**: `cargo xtask todo` ranks the unimplemented imports from every run's `unimplemented.toml`; hand them to the `hle-implementer` agent.
5. **Shaders**: dropped draws name shader hashes. `cargo xtask harness shader <hash> --config <toml>` diagnoses one. Use the `shader-op-implementer` agent for gaps.
6. Compress kept captures with `F:\VIctory_Testing\tools\compress-captures.ps1` (AVIF).

Milestones, in order: 1) every shader translates and gets a pipeline, and runs neither crash nor hang; 2) 30 fps. Rank findings by them.
