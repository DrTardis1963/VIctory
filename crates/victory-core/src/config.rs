//! Where the user's files live.
//!
//! Resolution order for each value: environment variable, then the config
//! file (`VICTORY_CONFIG`, else `./victory.toml`), then the default.
//! A variable that is set but empty counts as unset. Relative paths in the
//! config file are relative to the file, not to the working directory. A
//! leading `~` in a path (from the file or a variable) is the home directory.
//! See `victory.example.toml` at the repository root.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::policy::ensure_outside_repo;

#[derive(Debug, Default, Clone)]
pub struct Config {
    /// The user's dump of Grand Theft Auto VI.
    pub game_dir: Option<PathBuf>,
    /// Root for all derived data. Always outside the repository.
    pub cache_dir: PathBuf,
    /// What titles write and must keep (persistent local storage). Never
    /// inside the cache, which may be deleted at any time.
    pub data_dir: PathBuf,
    /// Other titles used to exercise platform-level code.
    pub test_titles: Vec<PathBuf>,
    /// The config file that was read, if any.
    pub source: Option<PathBuf>,
    /// Where each value above came from.
    pub origins: Origins,
}

/// Where a configured value came from, so `victory config` and errors can
/// name the setting to change instead of leaving the user to guess which of
/// the variable, the file and the default won.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Not set anywhere: the built-in default (or nothing, for `game_dir`).
    #[default]
    Default,
    /// The `[paths]` table of the config file.
    File,
    /// The named environment variable.
    Env(&'static str),
}

impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Origin::Default => f.write_str("default"),
            Origin::File => f.write_str("config file"),
            Origin::Env(name) => f.write_str(name),
        }
    }
}

/// The [`Origin`] of each [`Config`] value.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Origins {
    pub game_dir: Origin,
    pub cache_dir: Origin,
    pub data_dir: Origin,
    pub test_titles: Origin,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    #[serde(default)]
    paths: FilePaths,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilePaths {
    game_dir: Option<PathBuf>,
    cache_dir: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    #[serde(default)]
    test_titles: Vec<PathBuf>,
}

impl FileConfig {
    /// The `[paths]` table with every relative path made relative to `dir`,
    /// the directory holding the config file. With `VICTORY_CONFIG` pointing
    /// elsewhere, `cache_dir = "cache"` then means the folder next to that
    /// file, not one under whatever directory the tool was started from
    /// (often the repository, which the asset policy refuses). A path that
    /// starts with `~` is under `home` instead, when there is one.
    fn anchored_at(self, dir: &Path, home: Option<&Path>) -> FilePaths {
        let anchor = |p: PathBuf| {
            let p = expand_home(p, home);
            if p.is_relative() { dir.join(p) } else { p }
        };
        let paths = self.paths;
        FilePaths {
            game_dir: paths.game_dir.map(anchor),
            cache_dir: paths.cache_dir.map(anchor),
            data_dir: paths.data_dir.map(anchor),
            test_titles: paths.test_titles.into_iter().map(anchor).collect(),
        }
    }
}

impl Config {
    /// Loads configuration from the environment and the config file.
    pub fn load() -> Result<Self> {
        let env = |k: &str| std::env::var_os(k);
        let home = home_dir(env);
        let file = match config_file(explicit_config(env, home.as_deref()))? {
            Some(path) => {
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                Some((path, text))
            }
            None => None,
        };
        let (source, parsed) = match file {
            Some((path, text)) => {
                let parsed: FileConfig =
                    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
                let dir = path.parent().unwrap_or(Path::new("")).to_path_buf();
                (Some(path), parsed.anchored_at(&dir, home.as_deref()))
            }
            None => (None, FilePaths::default()),
        };
        Self::resolve(parsed, source, env)
    }

    fn resolve(
        file: FilePaths,
        source: Option<PathBuf>,
        env: impl Fn(&str) -> Option<std::ffi::OsString>,
    ) -> Result<Self> {
        // A quoted `VICTORY_GAME_DIR="~/games"` reaches us unexpanded.
        let home = home_dir(&env);
        let expand = |p: PathBuf| expand_home(p, home.as_deref());
        // The variable, else the file, with where the value came from.
        let pick =
            |k: &'static str, from_file: Option<PathBuf>| match env(k).filter(|v| !v.is_empty()) {
                Some(v) => (Some(expand(PathBuf::from(v))), Origin::Env(k)),
                None if from_file.is_some() => (from_file, Origin::File),
                None => (None, Origin::Default),
            };
        let (test_titles, test_titles_origin) =
            match env("VICTORY_TEST_TITLES").filter(|v| !v.is_empty()) {
                Some(v) => (
                    std::env::split_paths(&v)
                        .filter(|p| !p.as_os_str().is_empty())
                        .map(expand)
                        .collect(),
                    Origin::Env("VICTORY_TEST_TITLES"),
                ),
                None if file.test_titles.is_empty() => (file.test_titles, Origin::Default),
                None => (file.test_titles, Origin::File),
            };
        let (game_dir, game_origin) = pick("VICTORY_GAME_DIR", file.game_dir);
        let (cache_dir, cache_origin) = pick("VICTORY_CACHE_DIR", file.cache_dir);
        let cache_dir = cache_dir.map_or_else(|| default_dir(Kind::Cache, &env), Ok)?;
        let (data_dir, data_origin) = pick("VICTORY_DATA_DIR", file.data_dir);
        let data_dir = data_dir.map_or_else(|| default_dir(Kind::Data, &env), Ok)?;
        let origins = Origins {
            game_dir: game_origin,
            cache_dir: cache_origin,
            data_dir: data_origin,
            test_titles: test_titles_origin,
        };
        ensure_data_outside_cache(&cache_dir, &data_dir, &origins)?;
        Ok(Config {
            game_dir,
            cache_dir,
            data_dir,
            test_titles,
            source,
            origins,
        })
    }

    /// The game directory, or a helpful error if it is not configured.
    pub fn require_game_dir(&self) -> Result<&Path> {
        self.game_dir.as_deref().context(
            "no game directory configured: set VICTORY_GAME_DIR or [paths].game_dir in \
             victory.toml (see victory.example.toml)",
        )
    }

    /// Returns `<cache>/<sub>`, creating it, after checking the asset policy.
    pub fn cache_subdir(&self, sub: &str) -> Result<PathBuf> {
        subdir(&self.cache_dir, sub)
    }

    /// Returns `<data>/<sub>`, creating it, after checking the asset policy.
    pub fn data_subdir(&self, sub: &str) -> Result<PathBuf> {
        subdir(&self.data_dir, sub)
    }
}

/// `VICTORY_CONFIG`, empty counting as unset, with a leading `~` as the
/// home directory like every other path variable: a quoted
/// `VICTORY_CONFIG="~/victory.toml"` reaches us unexpanded and would
/// otherwise be reported as not existing.
fn explicit_config(
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    env("VICTORY_CONFIG")
        .filter(|v| !v.is_empty())
        .map(|v| expand_home(PathBuf::from(v), home))
}

/// The config file to read: `explicit` (`VICTORY_CONFIG`), which must be a
/// file, else `./victory.toml` when that is a file.
fn config_file(explicit: Option<PathBuf>) -> Result<Option<PathBuf>> {
    let Some(path) = explicit else {
        let default = PathBuf::from("victory.toml");
        return Ok(default.is_file().then_some(default));
    };
    if path.is_file() {
        return Ok(Some(path));
    }
    if path.is_dir() {
        // An easy slip: the folder holding victory.toml instead of the file.
        let inside = path.join("victory.toml");
        let hint = if inside.is_file() {
            format!("; did you mean {}?", inside.display())
        } else {
            String::new()
        };
        anyhow::bail!(
            "VICTORY_CONFIG points at {}, which is a folder, not a config file{hint}",
            path.display()
        );
    }
    anyhow::bail!(
        "VICTORY_CONFIG points at {}, which does not exist",
        path.display()
    );
}

/// The data folder holds what titles must keep and the cache may be deleted
/// at any time, so data inside (or equal to) the cache would be lost with it.
/// Easy to set up by accident, e.g. `VICTORY_CACHE_DIR=~/victory` with
/// `VICTORY_DATA_DIR=~/victory/data`. Compared as written (no symlinks
/// followed): the folders need not exist yet. The error says where each
/// folder came from, so it is clear which setting caused it.
fn ensure_data_outside_cache(cache_dir: &Path, data_dir: &Path, origins: &Origins) -> Result<()> {
    if data_dir.starts_with(cache_dir) {
        anyhow::bail!(
            "the data folder {} (from {}) is inside the cache folder {} (from {}), which may \
             be deleted at any time; set VICTORY_DATA_DIR or [paths].data_dir to a folder \
             outside it",
            data_dir.display(),
            origins.data_dir,
            cache_dir.display(),
            origins.cache_dir
        );
    }
    Ok(())
}

fn subdir(root: &Path, sub: &str) -> Result<PathBuf> {
    let dir = root.join(sub);
    ensure_outside_repo(&dir)?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// The user's home directory: `HOME`, else `USERPROFILE` (Windows), when
/// set to an absolute path.
fn home_dir(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    ["HOME", "USERPROFILE"]
        .into_iter()
        .filter_map(|k| env(k).map(PathBuf::from))
        .find(|p| p.is_absolute())
}

/// `~` and `~/rest` (`~\rest` on Windows) under `home`. Neither TOML nor a
/// quoted shell value expands `~`, so without this `game_dir = "~/games"`
/// would name a folder called `~`. `~user` is left alone, and so is
/// everything when there is no home directory.
fn expand_home(path: PathBuf, home: Option<&Path>) -> PathBuf {
    let Some(home) = home else { return path };
    let mut parts = path.components();
    match parts.next() {
        Some(Component::Normal(first)) if first == "~" => {
            let rest = parts.as_path();
            if rest.as_os_str().is_empty() {
                home.to_path_buf()
            } else {
                home.join(rest)
            }
        }
        _ => path,
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Cache,
    Data,
}

/// `%LOCALAPPDATA%/victory/{cache,data}`. Without `LOCALAPPDATA` (a native
/// non-Windows host), the XDG base directories: the cache under
/// `$XDG_CACHE_HOME` (`~/.cache`), the data under `$XDG_DATA_HOME`
/// (`~/.local/share`), so clearing the user's cache does not take what titles
/// must keep with it.
///
/// A variable that is set but empty or relative is skipped, as the XDG base
/// directory spec asks: otherwise `XDG_CACHE_HOME=` (or a relative value)
/// would put the directory under the working directory, which is often the
/// repository itself and so refused by the asset policy.
fn default_dir(kind: Kind, env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Result<PathBuf> {
    let absolute = |k: &str| env(k).map(PathBuf::from).filter(|p| p.is_absolute());
    if let Some(local) = absolute("LOCALAPPDATA") {
        let name = match kind {
            Kind::Cache => "cache",
            Kind::Data => "data",
        };
        return Ok(local.join("victory").join(name));
    }
    let (xdg, fallback) = match kind {
        Kind::Cache => ("XDG_CACHE_HOME", Path::new(".cache")),
        Kind::Data => ("XDG_DATA_HOME", Path::new(".local/share")),
    };
    let base = absolute(xdg)
        .or_else(|| absolute("HOME").map(|h| h.join(fallback)))
        .context("cannot determine a user directory; set VICTORY_CACHE_DIR and VICTORY_DATA_DIR")?;
    Ok(base.join("victory"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn environment_overrides_file() {
        let file = FilePaths {
            game_dir: Some("from-file".into()),
            cache_dir: Some("cache-from-file".into()),
            data_dir: Some("data-from-file".into()),
            ..Default::default()
        };
        let env = |k: &str| match k {
            "VICTORY_GAME_DIR" => Some(OsString::from("from-env")),
            "HOME" => Some(abs("home")),
            _ => None,
        };
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(cfg.game_dir.unwrap(), PathBuf::from("from-env"));
        assert_eq!(cfg.cache_dir, PathBuf::from("cache-from-file"));
    }

    #[test]
    fn empty_env_values_are_ignored() {
        let file = FilePaths {
            game_dir: Some("from-file".into()),
            ..Default::default()
        };
        let env = |k: &str| match k {
            "VICTORY_GAME_DIR" => Some(OsString::new()),
            "HOME" => Some(abs("home")),
            _ => None,
        };
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(cfg.game_dir.unwrap(), PathBuf::from("from-file"));
    }

    fn defaults(vars: &[(&str, OsString)]) -> Config {
        let env = |k: &str| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone());
        Config::resolve(FilePaths::default(), None, env).unwrap()
    }

    #[test]
    fn local_app_data_holds_cache_and_data() {
        let cfg = defaults(&[("LOCALAPPDATA", abs("l")), ("XDG_CACHE_HOME", abs("x"))]);
        let base = PathBuf::from(abs("l")).join("victory");
        assert_eq!(cfg.cache_dir, base.join("cache"));
        assert_eq!(cfg.data_dir, base.join("data"));
    }

    #[test]
    fn data_is_not_under_the_xdg_cache() {
        let cfg = defaults(&[("XDG_CACHE_HOME", abs("c")), ("XDG_DATA_HOME", abs("d"))]);
        assert_eq!(cfg.cache_dir, PathBuf::from(abs("c")).join("victory"));
        assert_eq!(cfg.data_dir, PathBuf::from(abs("d")).join("victory"));

        let cfg = defaults(&[("HOME", abs("h"))]);
        let home = PathBuf::from(abs("h"));
        assert_eq!(cfg.cache_dir, home.join(".cache").join("victory"));
        assert_eq!(cfg.data_dir, home.join(".local/share").join("victory"));
        assert!(!cfg.data_dir.starts_with(&cfg.cache_dir));
    }

    #[test]
    fn empty_or_relative_values_are_ignored() {
        let cfg = defaults(&[
            ("LOCALAPPDATA", OsString::new()),
            ("XDG_CACHE_HOME", OsString::new()),
            ("XDG_DATA_HOME", OsString::from("relative")),
            ("HOME", abs("h")),
        ]);
        let home = PathBuf::from(abs("h"));
        assert_eq!(cfg.cache_dir, home.join(".cache").join("victory"));
        assert_eq!(cfg.data_dir, home.join(".local/share").join("victory"));

        // A relative LOCALAPPDATA or XDG_CACHE_HOME falls through as well.
        let cfg = defaults(&[
            ("LOCALAPPDATA", OsString::from("relative/lad")),
            ("XDG_CACHE_HOME", OsString::from("relative/cache")),
            ("HOME", abs("h")),
        ]);
        assert_eq!(cfg.cache_dir, home.join(".cache").join("victory"));
    }

    #[test]
    fn default_dir_prefers_localappdata_then_xdg_then_home() {
        let all = |k: &str| match k {
            "LOCALAPPDATA" => Some(abs("lad")),
            "XDG_CACHE_HOME" => Some(abs("xdg")),
            "HOME" => Some(abs("home")),
            _ => None,
        };
        assert_eq!(
            default_dir(Kind::Cache, all).unwrap(),
            PathBuf::from(abs("lad")).join("victory").join("cache")
        );
        let no_lad = |k: &str| if k == "LOCALAPPDATA" { None } else { all(k) };
        assert_eq!(
            default_dir(Kind::Cache, no_lad).unwrap(),
            PathBuf::from(abs("xdg")).join("victory")
        );
        let home_only = |k: &str| (k == "HOME").then(|| abs("home"));
        assert_eq!(
            default_dir(Kind::Cache, home_only).unwrap(),
            PathBuf::from(abs("home")).join(".cache").join("victory")
        );
    }

    #[test]
    fn no_user_directory_is_an_error() {
        let env = |_: &str| None;
        assert!(Config::resolve(FilePaths::default(), None, env).is_err());
        let nothing_usable = |k: &str| (k == "HOME").then(OsString::new);
        assert!(default_dir(Kind::Cache, nothing_usable).is_err());
        assert!(default_dir(Kind::Data, nothing_usable).is_err());
    }

    #[test]
    fn empty_test_titles_variable_falls_back_to_the_file() {
        let file = FilePaths {
            test_titles: vec!["from-file".into()],
            ..Default::default()
        };
        let env = |k: &str| match k {
            "VICTORY_TEST_TITLES" => Some(OsString::new()),
            "HOME" => Some(abs("home")),
            _ => None,
        };
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(cfg.test_titles, vec![PathBuf::from("from-file")]);
    }

    #[test]
    fn relative_file_paths_are_relative_to_the_file() {
        let text = format!(
            "[paths]\ngame_dir = 'game'\ncache_dir = {:?}\ntest_titles = ['t/one']\n",
            PathBuf::from(abs("elsewhere")).display().to_string()
        );
        let parsed: FileConfig = toml::from_str(&text).unwrap();
        let dir = PathBuf::from(abs("configs"));
        let paths = parsed.anchored_at(&dir, None);
        assert_eq!(paths.game_dir.unwrap(), dir.join("game"));
        // Absolute paths are kept as written.
        assert_eq!(paths.cache_dir.unwrap(), PathBuf::from(abs("elsewhere")));
        assert_eq!(paths.data_dir, None);
        assert_eq!(paths.test_titles, vec![dir.join("t/one")]);
    }

    #[test]
    fn the_default_config_file_keeps_paths_as_written() {
        // `./victory.toml` has an empty parent, so nothing changes.
        let parsed: FileConfig = toml::from_str("[paths]\ngame_dir = 'game'\n").unwrap();
        let paths = parsed.anchored_at(Path::new(""), None);
        assert_eq!(paths.game_dir.unwrap(), PathBuf::from("game"));
    }

    #[test]
    fn a_leading_tilde_is_the_home_directory() {
        let home = PathBuf::from(abs("home"));
        let h = Some(home.as_path());
        assert_eq!(expand_home("~".into(), h), home);
        assert_eq!(expand_home("~/games/gta".into(), h), home.join("games/gta"));
        // Only a whole leading `~` component counts.
        assert_eq!(expand_home("~user/x".into(), h), PathBuf::from("~user/x"));
        assert_eq!(expand_home("a/~/b".into(), h), PathBuf::from("a/~/b"));
        // Without a home directory nothing changes.
        assert_eq!(expand_home("~/x".into(), None), PathBuf::from("~/x"));
    }

    #[test]
    fn tilde_in_the_file_is_home_not_next_to_the_file() {
        let parsed: FileConfig =
            toml::from_str("[paths]\ngame_dir = '~/games/gta'\ntest_titles = ['~/t']\n").unwrap();
        let home = PathBuf::from(abs("home"));
        let paths = parsed.anchored_at(&PathBuf::from(abs("configs")), Some(&home));
        assert_eq!(paths.game_dir.unwrap(), home.join("games/gta"));
        assert_eq!(paths.test_titles, vec![home.join("t")]);
    }

    #[test]
    fn tilde_in_variables_is_home() {
        let sep = if cfg!(windows) { ";" } else { ":" };
        let titles = format!("~/one{sep}~/two");
        let env = |k: &str| match k {
            "VICTORY_GAME_DIR" => Some(OsString::from("~/games/gta")),
            "VICTORY_CACHE_DIR" => Some(OsString::from("~/cache")),
            "VICTORY_TEST_TITLES" => Some(OsString::from(&titles)),
            "HOME" => Some(abs("home")),
            _ => None,
        };
        let cfg = Config::resolve(FilePaths::default(), None, env).unwrap();
        let home = PathBuf::from(abs("home"));
        assert_eq!(cfg.game_dir.unwrap(), home.join("games/gta"));
        assert_eq!(cfg.cache_dir, home.join("cache"));
        assert_eq!(cfg.test_titles, vec![home.join("one"), home.join("two")]);
    }

    #[test]
    fn home_dir_falls_back_to_userprofile_and_skips_relative() {
        let env = |k: &str| match k {
            "HOME" => Some(OsString::from("relative")),
            "USERPROFILE" => Some(abs("profile")),
            _ => None,
        };
        assert_eq!(home_dir(env), Some(PathBuf::from(abs("profile"))));
        assert_eq!(home_dir(|_: &str| None), None);
    }

    #[test]
    fn victory_config_expands_tilde_and_ignores_empty() {
        let home = PathBuf::from(abs("home"));
        let with = |v: &'static str| move |k: &str| (k == "VICTORY_CONFIG").then(|| v.into());
        assert_eq!(
            explicit_config(with("~/victory.toml"), Some(&home)),
            Some(home.join("victory.toml"))
        );
        assert_eq!(explicit_config(with(""), Some(&home)), None);
        assert_eq!(explicit_config(|_: &str| None, Some(&home)), None);
        // Without a home directory the value is taken as written.
        assert_eq!(
            explicit_config(with("~/victory.toml"), None),
            Some(PathBuf::from("~/victory.toml"))
        );
    }

    #[test]
    fn victory_config_must_name_a_file() {
        let dir = std::env::temp_dir().join(format!("victory-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("victory.toml");
        let _ = std::fs::remove_file(&file);

        let err = config_file(Some(dir.clone())).unwrap_err().to_string();
        assert!(err.contains("is a folder"), "{err}");
        assert!(!err.contains("did you mean"), "{err}");

        std::fs::write(&file, "").unwrap();
        let err = config_file(Some(dir.clone())).unwrap_err().to_string();
        assert!(err.contains("did you mean"), "{err}");
        assert!(err.contains(&file.display().to_string()), "{err}");
        assert_eq!(config_file(Some(file.clone())).unwrap(), Some(file.clone()));

        let missing = dir.join("missing.toml");
        let err = config_file(Some(missing)).unwrap_err().to_string();
        assert!(err.contains("does not exist"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn data_inside_the_cache_is_refused() {
        let env = |k: &str| match k {
            "VICTORY_CACHE_DIR" => Some(abs("v")),
            "VICTORY_DATA_DIR" => Some(PathBuf::from(abs("v")).join("data").into()),
            _ => None,
        };
        let err = Config::resolve(FilePaths::default(), None, env)
            .unwrap_err()
            .to_string();
        assert!(err.contains("inside the cache folder"), "{err}");

        // The same folder for both counts as inside.
        let same = |k: &str| match k {
            "VICTORY_CACHE_DIR" | "VICTORY_DATA_DIR" => Some(abs("v")),
            _ => None,
        };
        assert!(Config::resolve(FilePaths::default(), None, same).is_err());

        // A sibling whose name merely starts with the cache's is fine, and so
        // is the cache inside the data folder.
        let sibling = |k: &str| match k {
            "VICTORY_CACHE_DIR" => Some(abs("v")),
            "VICTORY_DATA_DIR" => Some(abs("v-data")),
            _ => None,
        };
        assert!(Config::resolve(FilePaths::default(), None, sibling).is_ok());
        let cache_in_data = |k: &str| match k {
            "VICTORY_CACHE_DIR" => Some(PathBuf::from(abs("v")).join("cache").into()),
            "VICTORY_DATA_DIR" => Some(abs("v")),
            _ => None,
        };
        assert!(Config::resolve(FilePaths::default(), None, cache_in_data).is_ok());
    }

    #[test]
    fn origins_name_the_setting_that_won() {
        let file = FilePaths {
            game_dir: Some(abs("file-game").into()),
            cache_dir: Some(abs("file-cache").into()),
            test_titles: vec![abs("file-title").into()],
            ..Default::default()
        };
        let env = |k: &str| match k {
            "VICTORY_GAME_DIR" => Some(abs("env-game")),
            "HOME" => Some(abs("home")),
            _ => None,
        };
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(
            cfg.origins,
            Origins {
                game_dir: Origin::Env("VICTORY_GAME_DIR"),
                cache_dir: Origin::File,
                data_dir: Origin::Default,
                test_titles: Origin::File,
            }
        );

        // Empty variables are unset, so they are not the origin either.
        let env = |k: &str| match k {
            "VICTORY_CACHE_DIR" | "VICTORY_TEST_TITLES" => Some(OsString::new()),
            "VICTORY_DATA_DIR" => Some(abs("env-data")),
            "HOME" => Some(abs("home")),
            _ => None,
        };
        let cfg = Config::resolve(FilePaths::default(), None, env).unwrap();
        assert_eq!(
            cfg.origins,
            Origins {
                game_dir: Origin::Default,
                cache_dir: Origin::Default,
                data_dir: Origin::Env("VICTORY_DATA_DIR"),
                test_titles: Origin::Default,
            }
        );
        assert_eq!(
            Origin::Env("VICTORY_DATA_DIR").to_string(),
            "VICTORY_DATA_DIR"
        );
        assert_eq!(Origin::File.to_string(), "config file");
        assert_eq!(Origin::Default.to_string(), "default");
    }

    #[test]
    fn data_inside_the_cache_names_both_origins() {
        // The cache from the file is the home folder, which holds the
        // default data folder: the error must say which setting is which.
        let file = FilePaths {
            cache_dir: Some(abs("home").into()),
            ..Default::default()
        };
        let env = |k: &str| (k == "HOME").then(|| abs("home"));
        let err = Config::resolve(file, None, env).unwrap_err().to_string();
        assert!(err.contains("(from default)"), "{err}");
        assert!(err.contains("(from config file)"), "{err}");
    }

    #[test]
    fn example_config_parses() {
        let text = include_str!("../../../victory.example.toml");
        let parsed: FileConfig = toml::from_str(text).unwrap();
        assert!(parsed.paths.game_dir.is_some());
    }

    #[test]
    fn cache_inside_repo_is_refused() {
        let inside = Path::new(env!("CARGO_MANIFEST_DIR")).join("cache");
        let cfg = Config {
            cache_dir: inside,
            ..Default::default()
        };
        assert!(cfg.cache_subdir("x").is_err());
    }

    /// A path that is absolute on the host (`/x` is not absolute on Windows).
    fn abs(p: &str) -> OsString {
        if cfg!(windows) {
            format!("C:\\{p}").into()
        } else {
            format!("/{p}").into()
        }
    }
}
