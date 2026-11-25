//! `cargo xtask todo`: the HLE to-do list, from stub ledgers.
//!
//! `victory run --report <dir>` keeps `<dir>/stub-hits.tsv`: one line per
//! unimplemented import the title called, `hits<TAB>dll!symbol<TAB>first
//! caller`. This merges any number of ledgers (several runs, several
//! titles), sums the hits per import and lists the most hit first.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The ledger's file name inside a report folder.
const LEDGER: &str = "stub-hits.tsv";

#[derive(Debug, PartialEq, Eq)]
struct Entry {
    /// `dll!symbol` as the first ledger wrote it.
    import: String,
    hits: u64,
    /// How many ledgers name it.
    runs: usize,
    /// From the ledger where it was hit most.
    first_caller: String,
    most: u64,
}

pub fn run(paths: &[PathBuf], limit: Option<usize>) -> Result<()> {
    anyhow::ensure!(
        !paths.is_empty(),
        "name the report folders (`victory run --report <dir>`) or ledger files"
    );
    let mut entries = Vec::new();
    for path in paths {
        let file = if path.is_dir() {
            path.join(LEDGER)
        } else {
            path.clone()
        };
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("reading the stub ledger {}", file.display()))?;
        merge(&mut entries, &text, &file)?;
    }
    rank(&mut entries);
    print!("{}", table(&entries, limit.unwrap_or(usize::MAX)));
    Ok(())
}

/// Adds one ledger's lines to `entries`.
fn merge(entries: &mut Vec<Entry>, text: &str, file: &Path) -> Result<()> {
    for (n, line) in text.lines().enumerate() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let bad = || {
            format!(
                "{}:{}: not `hits<TAB>dll!symbol<TAB>caller`",
                file.display(),
                n + 1
            )
        };
        let mut fields = line.split('\t');
        let (Some(hits), Some(import)) = (fields.next(), fields.next()) else {
            anyhow::bail!(bad());
        };
        let hits: u64 = hits.trim().parse().with_context(bad)?;
        anyhow::ensure!(import.contains('!'), bad());
        let caller = fields.next().unwrap_or("?").trim().to_owned();
        match entries
            .iter_mut()
            .find(|e| e.import.eq_ignore_ascii_case(import))
        {
            Some(e) => {
                e.hits += hits;
                e.runs += 1;
                if hits > e.most {
                    (e.most, e.first_caller) = (hits, caller);
                }
            }
            None => entries.push(Entry {
                import: import.to_owned(),
                hits,
                runs: 1,
                first_caller: caller,
                most: hits,
            }),
        }
    }
    Ok(())
}

/// Most hit first, then by name.
fn rank(entries: &mut [Entry]) {
    entries.sort_by(|a, b| {
        b.hits.cmp(&a.hits).then_with(|| {
            a.import
                .to_ascii_lowercase()
                .cmp(&b.import.to_ascii_lowercase())
        })
    });
}

fn table(entries: &[Entry], limit: usize) -> String {
    if entries.is_empty() {
        return "no unimplemented import was called\n".into();
    }
    let width = entries.iter().map(|e| e.import.len()).max().unwrap_or(0);
    let mut out = format!(
        "{:>4}  {:>10}  {:>4}  {:<width$}  first caller\n",
        "rank", "hits", "runs", "import"
    );
    for (i, e) in entries.iter().take(limit).enumerate() {
        out += &format!(
            "{:>4}  {:>10}  {:>4}  {:<width$}  {}\n",
            i + 1,
            e.hits,
            e.runs,
            e.import,
            e.first_caller
        );
    }
    if entries.len() > limit {
        out += &format!("({} more)\n", entries.len() - limit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUN_A: &str = "# victory stub ledger v1\n# hits\timport\tfirst caller\n\
        40\tapi.dll!Hot\ttitle.exe+0x20\n\
        3\tapi.dll!Warm\ttitle.exe+0x30\n";
    const RUN_B: &str = "# victory stub ledger v1\r\n\
        100\tAPI.dll!Warm\tother.dll+0x99\r\n\
        1\tz.dll!Cold\t?\r\n";

    fn merged() -> Vec<Entry> {
        let mut entries = Vec::new();
        merge(&mut entries, RUN_A, Path::new("a.tsv")).unwrap();
        merge(&mut entries, RUN_B, Path::new("b.tsv")).unwrap();
        rank(&mut entries);
        entries
    }

    #[test]
    fn ledgers_merge_per_import_and_rank_by_hits() {
        let e = merged();
        let got: Vec<_> = e
            .iter()
            .map(|e| (e.import.as_str(), e.hits, e.runs, e.first_caller.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("api.dll!Warm", 103, 2, "other.dll+0x99"),
                ("api.dll!Hot", 40, 1, "title.exe+0x20"),
                ("z.dll!Cold", 1, 1, "?"),
            ]
        );
    }

    #[test]
    fn the_table_is_ranked_and_cut_at_the_limit() {
        let text = table(&merged(), 2);
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 4, "{text}");
        assert!(lines[0].starts_with("rank"));
        assert!(lines[1].trim_start().starts_with("1 ") && lines[1].contains("api.dll!Warm"));
        assert!(lines[2].contains("api.dll!Hot") && lines[2].ends_with("title.exe+0x20"));
        assert_eq!(lines[3], "(1 more)");
        assert_eq!(table(&[], 5), "no unimplemented import was called\n");
    }

    #[test]
    fn a_malformed_line_names_its_place() {
        let mut entries = Vec::new();
        let err = merge(&mut entries, "# h\nlots\tapi.dll!X\n", Path::new("r.tsv")).unwrap_err();
        assert!(format!("{err:#}").contains("r.tsv:2"), "{err:#}");
        let err = merge(&mut entries, "5\tnobang\n", Path::new("r.tsv")).unwrap_err();
        assert!(format!("{err:#}").contains("r.tsv:1"), "{err:#}");
    }

    #[test]
    fn a_report_folder_is_read_through_its_ledger() {
        let dir = std::env::temp_dir().join(format!("xtask-todo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(LEDGER), RUN_A).unwrap();
        let ok = run(std::slice::from_ref(&dir), None);
        let missing = run(&[dir.join("nope")], None);
        std::fs::remove_file(dir.join(LEDGER)).unwrap();
        std::fs::remove_dir(&dir).unwrap();
        ok.unwrap();
        assert!(missing.is_err());
        assert!(run(&[], None).is_err());
    }
}
