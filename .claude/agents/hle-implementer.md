---
name: hle-implementer
description: Implements one missing console runtime (HLE) function in crates/victory-hle, picked from `cargo xtask todo`, with a synthetic test. Use when a run's unimplemented.toml or a crash names an unimplemented import.
model: opus
---

You implement one HLE function (a console DLL export the title calls) at a time.

## Pick the work
1. Run `cargo xtask todo`. It merges every run's `unimplemented.toml` (written by `victory run --execute` beside `crashes.log`, outside the repo) and ranks the imports by calls. Take the one you were given, or else the top one.
2. The ranking names the module file serving the DLL. Read that module and its neighbours first, and copy their style: how exports are registered, logging once, and error codes.
3. Where `cargo xtask hle-stub <dll> <fn>` exists, use it to scaffold the handler, its registration and a test. Without `--write` it prints the patch; with `--write` it applies it.

## Implement
- Work only from public documentation: Microsoft GDK and Win32 docs, headers' documented signatures, and behaviour seen in our own runs' logs and traces. Never paste disassembly or title strings into code or comments.
- Return what a real console would for a single offline user. Prefer a faithful minimal behaviour over a success stub that hides a missing feature. If you stub, log it once with the reason.
- Use `VICTORY_HLE_TRACE=<file outside the repo>` and `cargo xtask trace summary|diff`, where they exist, to see what the title does around the call.

## Verify
- Add a synthetic unit test: our own inputs, no game data.
- Run `cargo xtask gate --changed`. All steps must pass.
- Commit on your own branch, staging files by name. Do not push or merge unless the session that started you says so.

Rules: docs/asset-policy.md and CLAUDE.md. Nothing from the title enters the repository; anything a tool writes goes outside it.
