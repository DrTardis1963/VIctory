# Legal, Licensing & Platform Compliance

This document summarizes VIctory's clean-room boundaries, third-party dependency
licensing, known binary-table risk, and GitHub Terms of Service guidelines. It
is technical compliance documentation for contributors, not formal legal advice.

## 1. Clean-Room Interoperability Scope

- **Original implementation**: VIctory provides original Rust implementations of
  console runtime interfaces, file/storage adapters, graphics command
  translation, shader translation, and container decoders.
- **User-supplied inputs**: VIctory contains no game binaries, assets, media,
  decryption keys, or DRM circumvention tooling. Any title files or system tools
  must be legitimately supplied by the user outside the repository.
- **Offline by design**: network HLE modules block outbound game telemetry and
  store connections rather than spoofing online services.
- **Interoperability intent**: format and API support is authored purely for
  interoperability on PC hardware (consistent with clean-room engineering
  practices, e.g., 17 U.S.C. §107 and §1201(f) interoperability principles).

## 2. Trademarks & Non-Affiliation

VIctory is an independent open-source project (`GPL-3.0-or-later`) and is not
affiliated with, endorsed by, or sponsored by Rockstar Games, Take-Two
Interactive, Microsoft Corporation, AMD, or NVIDIA. All product names, logos,
and trademarks belong to their respective owners and are used strictly for
compatibility identification.

## 3. Dependency License Audit

An automated audit of the locked Cargo dependency graph (`cargo metadata
--format-version 1 --locked`, 90 resolved external packages in `Cargo.lock`)
shows all third-party Rust dependencies use permissive or GPL-compatible
licenses:

- `MIT`, `Apache-2.0`, `MIT OR Apache-2.0`, `Apache-2.0 WITH LLVM-exception`
- `BSD-3-Clause` (`opus-pure 0.2.4`)
- `0BSD` (`adler2`), `Zlib` (`miniz_oxide`), `CC0-1.0` (`blake3`,
  `constant_time_eq`), `Unlicense` (`aho-corasick`, `memchr`, `same-file`,
  `walkdir`, `winapi-util`), `Unicode-3.0` (`unicode-ident`), and
  `LGPL-2.1-or-later` option (`r-efi`).

No resolved crate in `Cargo.lock` carries a proprietary or `GPL-3.0-or-later`
incompatible license.

## 4. Known Exception: `bcpack-tables.bin`

As documented in [docs/asset-policy.md](asset-policy.md),
`crates/victory-formats/src/bcpack-tables.bin` (77,982 bytes) is the sole
non-source exception in `xtask/src/policy.rs`. As noted there, the repository's
GPL-3.0 license does not cover those constant format tables.

**Legal/distribution risk & mitigation**:

- Distributors shipping pure GPL-3.0 builds without third-party format tables
  should avoid bundling `bcpack-tables.bin` directly in binaries when relying
  strictly on GPL-3.0 uniformity.
- Supplying the tables externally at runtime (`VICTORY_BCPACK_TABLES`) or gating
  the embedded `include_bytes!` behind a Cargo feature flag cleanly separates
  our GPL-3.0 code from the format's constant tables.

## 5. GitHub Terms of Service & Acceptable Use

Contributors working on forks must adhere to GitHub's Terms of Service and
Acceptable Use Policies:

- **No spam or automation abuse**: never post bulk/automated comments, open
  duplicate issues, or attempt to bypass repository collaborator restrictions.
- **No CI waste**: run `cargo xtask policy`, `cargo clippy`, and `cargo test`
  locally before pushing commits to public forks.
- **No leaked secrets or personal data**: never commit API tokens, private
  keys, or personal details in headers, User-Agent strings, or test fixtures.
- **Immediate takedown hygiene**: never re-upload or mirror DMCA-removed code
  or proprietary content.
