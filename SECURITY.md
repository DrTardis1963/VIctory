# Security & Asset-Policy Incident Policy

## Scope

This policy covers two categories of issues in VIctory:

1. **Asset-policy / copyright violations**: accidental inclusion of proprietary
   game files, binaries, assets, dumps, screenshots, or third-party proprietary
   SDK material anywhere in the repository or git history (see
   [docs/asset-policy.md](docs/asset-policy.md)).
2. **Code security vulnerabilities**: memory safety bugs, path-traversal issues
   bypassing `victory_core::ensure_outside_repo` or virtual-drive containment,
   or unintended network egress.

## Reporting

- **Public repository bugs & non-sensitive issues**: open an issue on the
  upstream repository describing synthetic reproduction steps without attaching
  proprietary files, personal data, or credentials.
- **Asset-policy / copyright notices**: maintainers treat any confirmed
  proprietary content in tracked files as an immediate removal priority. Follow
  the history-purge procedure below rather than a plain follow-up delete commit.
- **Security issues**: avoid posting weaponized payloads or private keys in
  public issues; provide minimal synthetic reproduction details and affected
  crate/file paths.

## Mandatory History Purge for Proprietary Data

If proprietary material is ever committed to a branch (see
[docs/asset-policy.md](docs/asset-policy.md#if-something-slips-through)):

1. Stop distributing or merging the affected branch immediately.
2. Remove the file from all commits with `git filter-repo`:
   ```powershell
   git filter-repo --path <offending-file> --invert-paths
   ```
3. Force-push the rewritten branch (or delete the affected remote branch) and
   re-clone clean working copies.
4. Verify with `cargo xtask policy` before reopening any work.
