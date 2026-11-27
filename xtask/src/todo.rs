//! `cargo xtask todo`: what to implement next, from what titles called.
//!
//! `victory run --execute` keeps a ledger of the unimplemented imports the
//! title called (`unimplemented.toml` beside `crashes.log`, under the data
//! directory). This merges any number of ledgers (several runs, several
//! titles), ranks the imports by calls, and names the HLE module file that
//! serves each import's DLL, i.e. where the implementation goes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::json;

#[derive(Deserialize)]
struct Ledger {
    #[serde(default)]
    import: Vec<Entry>,
}

#[derive(Deserialize, Clone)]
struct Entry {
    dll: String,
    symbol: String,
    calls: u64,
    #[serde(default)]
    first_caller: String,
}

/// One import across every ledger read.
struct Ranked {
    dll: String,
    symbol: String,
    calls: u64,
    /// How many ledgers list it.
    runs: usize,
    /// From the ledger where it was called most.
    first_caller: String,
    /// HLE module files that serve its DLL; empty means none does yet.
    modules: Vec<String>,
}

pub fn run(root: &Path, files: Vec<PathBuf>, limit: usize, as_json: bool) -> Result<()> {
    let files = if files.is_empty() {
        default_ledgers()?
    } else {
        files
    };
    if files.is_empty() {
        println!(
            "no ledgers found: run a title with `victory run --execute --unimplemented zero`, \
             or name ledger files"
        );
        return Ok(());
    }
    let mut ledgers = Vec::new();
    for file in &files {
        let text =
            std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
        let ledger: Ledger =
            toml::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
        ledgers.push(ledger.import);
    }
    let served = served_dlls(&root.join("crates/victory-hle/src/modules"))?;
    let mut ranked = rank(&ledgers, &served);
    let total = ranked.len();
    ranked.truncate(limit);

    if as_json {
        let items = ranked.iter().map(|r| {
            json::object([
                ("dll", json::string(&r.dll)),
                ("symbol", json::string(&r.symbol)),
                ("calls", r.calls.to_string()),
                ("runs", r.runs.to_string()),
                ("first_caller", json::string(&r.first_caller)),
                (
                    "modules",
                    json::array(r.modules.iter().map(|m| json::string(m))),
                ),
                (
                    "hle_stub",
                    stub_command(r).map_or("null".into(), |c| json::string(&c)),
                ),
            ])
        });
        println!(
            "{}",
            json::object([
                ("ledgers", files.len().to_string()),
                ("total", total.to_string()),
                ("imports", json::array(items)),
            ])
        );
        return Ok(());
    }

    println!(
        "{total} unimplemented import(s) called, from {} ledger(s):",
        files.len()
    );
    for file in &files {
        println!("  {}", file.display());
    }
    println!();
    println!(
        "{:>4}  {:>10}  {:>4}  {:<48}  {:<28}  where",
        "rank", "calls", "runs", "import", "first caller"
    );
    for (i, r) in ranked.iter().enumerate() {
        let place = if r.modules.is_empty() {
            "no module serves this DLL yet".to_owned()
        } else {
            r.modules.join(", ")
        };
        println!(
            "{:>4}  {:>10}  {:>4}  {:<48}  {:<28}  {place}",
            i + 1,
            r.calls,
            r.runs,
            format!("{}!{}", r.dll, r.symbol),
            r.first_caller
        );
        if let Some(command) = stub_command(r) {
            println!("{:>24}{command}", "");
        }
    }
    if total > ranked.len() {
        println!("  ... {} more (--limit)", total - ranked.len());
    }
    Ok(())
}

/// The `cargo xtask hle-stub` command that scaffolds `r`, where a module
/// serves its DLL and its name can be a function's.
fn stub_command(r: &Ranked) -> Option<String> {
    let ident = r.symbol.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && r.symbol.chars().next().is_some_and(|c| !c.is_ascii_digit());
    (!r.modules.is_empty() && ident)
        .then(|| format!("cargo xtask hle-stub {} {}", r.dll, r.symbol))
}

/// `unimplemented.toml` in each title's folder under the configured data
/// directory.
fn default_ledgers() -> Result<Vec<PathBuf>> {
    let cfg = victory_core::Config::load()?;
    let mut found: Vec<PathBuf> = std::fs::read_dir(&cfg.data_dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().join("unimplemented.toml"))
        .filter(|p| p.is_file())
        .collect();
    found.sort();
    Ok(found)
}

fn rank(ledgers: &[Vec<Entry>], served: &[(String, String)]) -> Vec<Ranked> {
    // Keyed case-blind on the DLL, as the loader matches DLL names.
    let mut merged: BTreeMap<(String, String), (Ranked, u64)> = BTreeMap::new();
    for ledger in ledgers {
        for e in ledger {
            let key = (e.dll.to_ascii_lowercase(), e.symbol.clone());
            let (r, most) = merged.entry(key).or_insert_with(|| {
                let ranked = Ranked {
                    dll: e.dll.clone(),
                    symbol: e.symbol.clone(),
                    calls: 0,
                    runs: 0,
                    first_caller: String::new(),
                    modules: modules_serving(&e.dll, served),
                };
                (ranked, 0)
            });
            r.calls += e.calls;
            r.runs += 1;
            if e.calls > *most {
                *most = e.calls;
                r.first_caller = e.first_caller.clone();
            }
        }
    }
    let mut ranked: Vec<Ranked> = merged.into_values().map(|(r, _)| r).collect();
    ranked.sort_by(|a, b| {
        (b.calls, b.runs, &a.dll, &a.symbol).cmp(&(a.calls, a.runs, &b.dll, &b.symbol))
    });
    ranked
}

/// `(dll pattern, module file)` for every `dlls: &[...]` list in the HLE
/// modules. Patterns are lower case; a trailing `*` matches a prefix
/// (`HleModule::serves`).
pub(crate) fn served_dlls(modules: &Path) -> Result<Vec<(String, String)>> {
    let mut served = Vec::new();
    for entry in walkdir::WalkDir::new(modules).sort_by_file_name() {
        let entry = entry?;
        if entry.path().extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let text = std::fs::read_to_string(entry.path())?;
        let file = entry
            .path()
            .strip_prefix(modules)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .replace('\\', "/");
        for pattern in dll_patterns(&text) {
            served.push((pattern, format!("modules/{file}")));
        }
    }
    Ok(served)
}

/// The string literals inside each `dlls: &[ ... ]` in `source`.
fn dll_patterns(source: &str) -> Vec<String> {
    let mut patterns = Vec::new();
    let mut rest = source;
    while let Some(start) = rest.find("dlls: &[") {
        rest = &rest[start + "dlls: &[".len()..];
        let end = rest.find(']').unwrap_or(rest.len());
        let list = &rest[..end];
        patterns.extend(
            list.split('"')
                .skip(1)
                .step_by(2)
                .map(|s| s.to_ascii_lowercase()),
        );
        rest = &rest[end..];
    }
    patterns
}

pub(crate) fn modules_serving(dll: &str, served: &[(String, String)]) -> Vec<String> {
    let dll = dll.to_ascii_lowercase();
    let mut files: Vec<String> = served
        .iter()
        .filter(|(pattern, _)| match pattern.strip_suffix('*') {
            Some(prefix) => dll.starts_with(prefix),
            None => dll == *pattern,
        })
        .map(|(_, file)| file.clone())
        .collect();
    files.dedup();
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(dll: &str, symbol: &str, calls: u64, caller: &str) -> Entry {
        Entry {
            dll: dll.into(),
            symbol: symbol.into(),
            calls,
            first_caller: caller.into(),
        }
    }

    #[test]
    fn dll_lists_are_read_from_module_source() {
        let source = r#"
            HleModule { name: "a", dlls: &["One.dll"], exports: vec![] }
            HleModule {
                name: "b",
                dlls: &[
                    "two.dll",
                    "api-ms-win-core-file-*",
                ],
            }
        "#;
        assert_eq!(
            dll_patterns(source),
            ["one.dll", "two.dll", "api-ms-win-core-file-*"]
        );
    }

    #[test]
    fn ledgers_merge_case_blind_and_rank_by_calls() {
        let served = vec![
            ("one.dll".to_string(), "modules/one.rs".to_string()),
            (
                "api-ms-win-core-*".to_string(),
                "modules/files.rs".to_string(),
            ),
        ];
        let ledgers = vec![
            vec![
                entry("ONE.dll", "Hot", 5, "t.exe+0x10"),
                entry("api-ms-win-core-file-l1-1-0.dll", "Rare", 1, "t.exe+0x30"),
            ],
            vec![
                entry("one.dll", "Hot", 7, "t.exe+0x20"),
                entry("new.dll", "Mid", 3, "t.exe+0x40"),
            ],
        ];
        let ranked = rank(&ledgers, &served);
        let order: Vec<_> = ranked.iter().map(|r| r.symbol.as_str()).collect();
        assert_eq!(order, ["Hot", "Mid", "Rare"]);
        assert_eq!(ranked[0].calls, 12);
        assert_eq!(ranked[0].runs, 2);
        // The caller from the run that called it most.
        assert_eq!(ranked[0].first_caller, "t.exe+0x20");
        assert_eq!(ranked[0].modules, ["modules/one.rs"]);
        assert!(ranked[1].modules.is_empty());
        assert_eq!(ranked[2].modules, ["modules/files.rs"]);
    }

    #[test]
    fn the_loaders_ledger_format_parses() {
        let text = "# comment\n\n[[import]]\ndll = \"a.dll\"\nsymbol = \"F\"\ncalls = 2\nfirst_caller = \"t.exe+0x1\"\n";
        let ledger: Ledger = toml::from_str(text).unwrap();
        assert_eq!(ledger.import[0].calls, 2);
        let empty: Ledger = toml::from_str("# nothing called\n").unwrap();
        assert!(empty.import.is_empty());
    }

    #[test]
    fn an_import_a_module_serves_gets_its_hle_stub_command() {
        let ranked = |symbol: &str, modules: &[&str]| Ranked {
            dll: "a.dll".into(),
            symbol: symbol.into(),
            calls: 1,
            runs: 1,
            first_caller: String::new(),
            modules: modules.iter().map(|m| m.to_string()).collect(),
        };
        let command = stub_command(&ranked("XOpen", &["modules/a.rs"]));
        assert_eq!(command.as_deref(), Some("cargo xtask hle-stub a.dll XOpen"));
        assert_eq!(stub_command(&ranked("XOpen", &[])), None);
        assert_eq!(stub_command(&ranked("#3", &["modules/a.rs"])), None);
    }

    #[test]
    fn the_real_modules_serve_kernel32() {
        let root = crate::workspace_root();
        let served = served_dlls(&root.join("crates/victory-hle/src/modules")).unwrap();
        assert!(!modules_serving("KERNEL32.dll", &served).is_empty());
    }
}
