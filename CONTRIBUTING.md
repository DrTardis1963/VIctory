# Contributing to VIctory

All contributions to VIctory must follow the clean-room rules in
[docs/asset-policy.md](docs/asset-policy.md) and the workspace licence
(`GPL-3.0-or-later`).

## 1. Asset Policy (Rule #1 — Non-Negotiable)

Never include, reference, or link to proprietary material in commits, tests,
issues, pull requests, screenshots, or documentation:

- **Forbidden**: game executables, archives, assets, textures, models, shaders,
  audio, video, strings, extracted tables, crash dumps, frame captures,
  disassembled/decompiled game code, proprietary SDK headers/binaries, or links
  to obtain game files.
- **Allowed**: original clean-room implementations of documented formats and
  system APIs, build identities (BLAKE3 / CodeView GUID+age), interoperability
  constants, and 100% synthetic test fixtures written by you.
- **Runtime outputs**: any code that writes caches, logs, shader dumps, frame
  captures, audio recordings, or crash saves must reject paths inside the
  repository via `victory_core::ensure_outside_repo`.

## 2. Local Setup & Pre-Commit Hook

Install the asset-policy pre-commit hook before making any commits:

```powershell
cargo xtask install-hooks
```

Do not put game paths or derived outputs inside the repository; configure
`victory.toml` (git-ignored) or environment variables pointing outside the repo.

## 3. Verification Before Pushing

Run the full verification gate locally before pushing any branch:

```powershell
cargo xtask policy
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --locked
```

- Tests must use **100% synthetic data**. Any optional test that reads external
  user-supplied files via environment variables must be marked `#[ignore]` and
  never store snapshots or fixture files in git.
- Keep diffs focused and free of unrelated formatting churn (`rustfmt.toml`).

## 4. Issue Etiquette & Disclosures

- Check open issues and existing fork branches before starting work to avoid
  duplicating fixes.
- When PR creation is restricted to collaborators, push verified branches to your
  own fork and leave one concise factual comment on the relevant issue.
- Clearly state in descriptions/comments:
  - whether changes were AI-assisted,
  - the exact hardware/OS/driver/Rust environment tested,
  - that only synthetic tests were run (or that no game data was committed).
- Never commit credentials, personal names/emails in headers, or private tokens.
