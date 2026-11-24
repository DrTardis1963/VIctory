//! Rule #1, enforced at runtime: game-derived data never lands in the repo.
//!
//! Everything VIctory produces from the user's files (converted assets,
//! prepared images, shader caches, crash dumps) is as much "from the game"
//! as the files themselves. Every tool that writes such output must call
//! [`ensure_outside_repo`] on the destination first. The static half of the
//! rule is `cargo xtask policy`, which checks what git tracks.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

/// Marker file at the root of the source tree.
pub const ROOT_MARKER: &str = ".victory-root";

/// The source tree this binary was compiled from.
fn compiled_workspace_root() -> PathBuf {
    // crates/victory-core -> workspace root
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error(
        "refusing to use {path} for game-derived data: it is inside the VIctory source tree \
         ({root}). Keep game files, caches and outputs outside the repository \
         (see docs/asset-policy.md)"
    )]
    InsideRepo { path: PathBuf, root: PathBuf },
    #[error("cannot resolve {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Returns `Ok` only if `path` is outside every VIctory source tree.
///
/// `path` does not have to exist yet. Two checks are made:
/// 1. it is not below the source tree this binary was built from;
/// 2. none of its ancestors contains the [`ROOT_MARKER`] file (catches
///    other clones/copies of the repository).
pub fn ensure_outside_repo(path: &Path) -> Result<(), PolicyError> {
    let resolved = resolve(path)?;

    if let Ok(root) = std::fs::canonicalize(compiled_workspace_root())
        && root.join(ROOT_MARKER).is_file()
        && resolved.starts_with(&root)
    {
        return Err(PolicyError::InsideRepo {
            path: path.to_path_buf(),
            root,
        });
    }

    for ancestor in resolved.ancestors() {
        if ancestor.join(ROOT_MARKER).is_file() {
            return Err(PolicyError::InsideRepo {
                path: path.to_path_buf(),
                root: ancestor.to_path_buf(),
            });
        }
    }
    Ok(())
}

enum Tail {
    Normal(OsString),
    Parent,
}

/// Canonicalizes the longest existing prefix of `path` and re-appends the
/// rest, so that paths which do not exist yet can still be compared.
fn resolve(path: &Path) -> Result<PathBuf, PolicyError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|source| PolicyError::Io {
                path: path.to_path_buf(),
                source,
            })?
            .join(path)
    };

    let (resolved, saw_parent) = canonicalize_prefix(path, &absolute)?;
    // Collapsing `..` past missing segments can reveal an existing folder or
    // reparse point with a different canonical path or Windows casing.
    let resolved = if saw_parent {
        canonicalize_prefix(path, &resolved)?.0
    } else {
        resolved
    };
    Ok(resolved)
}

fn canonicalize_prefix(original: &Path, path: &Path) -> Result<(PathBuf, bool), PolicyError> {
    let mut existing = path;
    let mut rest: Vec<Tail> = Vec::new();
    let mut saw_parent = false;
    loop {
        match std::fs::canonicalize(existing) {
            Ok(mut out) => {
                for segment in rest.iter().rev() {
                    match segment {
                        Tail::Normal(name) => out.push(name),
                        Tail::Parent => {
                            if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                                out.pop();
                            }
                        }
                    }
                }
                return Ok((out, saw_parent));
            }
            Err(source) => {
                let mut comps = existing.components();
                match comps.next_back() {
                    Some(Component::Normal(name)) => {
                        rest.push(Tail::Normal(name.to_os_string()));
                        existing = comps.as_path();
                    }
                    Some(Component::ParentDir) => {
                        saw_parent = true;
                        rest.push(Tail::Parent);
                        existing = comps.as_path();
                    }
                    Some(Component::CurDir) => {
                        existing = comps.as_path();
                    }
                    Some(Component::RootDir | Component::Prefix(_)) | None => {
                        return Err(PolicyError::Io {
                            path: original.to_path_buf(),
                            source,
                        });
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_paths_inside_this_repo() {
        let inside = compiled_workspace_root().join("cache").join("textures");
        assert!(matches!(
            ensure_outside_repo(&inside),
            Err(PolicyError::InsideRepo { .. })
        ));
        let sneaky = std::env::temp_dir().join("..").join("x"); // sanity: no panic
        let _ = ensure_outside_repo(&sneaky);
    }

    #[test]
    fn rejects_dotdot_escapes_back_into_repo() {
        let root = compiled_workspace_root();
        let path = root
            .join("crates")
            .join("..")
            .join("not-yet-created")
            .join("out");
        assert!(ensure_outside_repo(&path).is_err());
    }

    #[test]
    fn rejects_other_clones_via_marker() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(ROOT_MARKER), "").unwrap();
        assert!(ensure_outside_repo(&tmp.path().join("cache")).is_err());
    }

    #[test]
    fn accepts_unrelated_directories() {
        let tmp = tempfile::tempdir().unwrap();
        ensure_outside_repo(&tmp.path().join("does").join("not").join("exist")).unwrap();
    }

    #[test]
    fn accepts_nonexistent_parent_traversal_outside_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("missing1").join("missing2").join("..").join("out");
        ensure_outside_repo(&path).unwrap();
    }

    #[test]
    fn rejects_nonexistent_parent_traversal_inside_repo() {
        let root = compiled_workspace_root();
        assert!(matches!(
            ensure_outside_repo(&root.join("missing1").join("missing2").join("..")),
            Err(PolicyError::InsideRepo { .. })
        ));
        assert!(matches!(
            ensure_outside_repo(
                &root.join("missing1").join("missing2").join("..").join("cache")
            ),
            Err(PolicyError::InsideRepo { .. })
        ));
    }

    #[test]
    fn rejects_traversal_into_other_clone_past_missing_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let clone = tmp.path().join("clone");
        std::fs::create_dir_all(&clone).unwrap();
        std::fs::write(clone.join(ROOT_MARKER), "").unwrap();
        let path = clone
            .join("missing1")
            .join("missing2")
            .join("..")
            .join("cache");
        assert!(matches!(
            ensure_outside_repo(&path),
            Err(PolicyError::InsideRepo { .. })
        ));
    }

    #[cfg(windows)]
    #[test]
    fn recanonicalizes_existing_dir_revealed_after_parent_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let clone = tmp.path().join("MixedCaseClone");
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&clone).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(clone.join(ROOT_MARKER), "").unwrap();
        let path = work
            .join("missing1")
            .join("missing2")
            .join("..")
            .join("..")
            .join("..")
            .join("mixedcaseclone")
            .join("missing3")
            .join("missing4")
            .join("..")
            .join("cache");
        assert!(
            resolve(&path)
                .unwrap()
                .starts_with(std::fs::canonicalize(&clone).unwrap())
        );
        assert!(matches!(
            ensure_outside_repo(&path),
            Err(PolicyError::InsideRepo { .. })
        ));
    }

    #[test]
    fn excess_parent_dirs_clamp_at_root() {
        let tmp = tempfile::tempdir().unwrap();
        let mut path = tmp.path().join("missing1").join("missing2");
        for _ in 0..32 {
            path.push("..");
        }
        path.push("out");
        assert!(resolve(&path).is_ok());
    }
}
