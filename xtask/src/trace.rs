//! `cargo xtask trace`: reads the HLE call traces `VICTORY_HLE_TRACE`
//! writes (ndjson: a line per call, a line per return, joined by `seq`).
//!
//! - `summary <file>`: the most called functions, and per thread the calls
//!   it is still inside (no return line) and its last call: where each
//!   thread stopped, which is what a hang needs.
//! - `diff <a> <b>`: two runs side by side. Thread ids differ between runs,
//!   so threads are paired by their first calls, then by start order; each
//!   pair's calls are compared in step by `dll!fn`, and the first place
//!   they part is shown with five calls of context. Then the calls per
//!   function in each run.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Subcommand;

#[derive(Subcommand)]
pub enum TraceCmd {
    /// Top functions by calls, and where each thread stopped.
    Summary {
        file: PathBuf,
        /// Show at most this many functions.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Where two traces part, per thread, and calls per function in each.
    Diff {
        a: PathBuf,
        b: PathBuf,
        /// Show at most this many functions in the count table.
        #[arg(long, default_value_t = 30)]
        limit: usize,
    },
}

pub fn run(cmd: TraceCmd) -> Result<()> {
    match cmd {
        TraceCmd::Summary { file, limit } => {
            print!("{}", summary(&read(&file)?, limit));
        }
        TraceCmd::Diff { a, b, limit } => {
            print!("{}", diff(&read(&a)?, &read(&b)?, limit));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Call {
    seq: u64,
    t: u64,
    thread: u64,
    dll: String,
    func: String,
    args: Vec<u64>,
    caller: String,
    ret: Option<u64>,
}

impl Call {
    fn name(&self) -> String {
        format!("{}!{}", self.dll, self.func)
    }

    /// What two runs compare: the DLL's case does not matter.
    fn key(&self) -> (String, &str) {
        (self.dll.to_ascii_lowercase(), &self.func)
    }

    fn line(&self) -> String {
        let args: Vec<String> = self.args.iter().map(|a| format!("{a:#x}")).collect();
        let ret = match self.ret {
            Some(r) => format!("-> {r:#x}"),
            None => "(no return)".into(),
        };
        format!(
            "#{} t={}us {}({}) {ret} from {}",
            self.seq,
            self.t,
            self.name(),
            args.join(", "),
            self.caller
        )
    }
}

#[derive(Debug, Default)]
struct Trace {
    /// In `seq` order.
    calls: Vec<Call>,
    /// Events the writer fell a lap behind on.
    lost: u64,
    /// Lines that were not trace events.
    bad: usize,
}

fn read(path: &Path) -> Result<Trace> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(parse(&text))
}

fn parse(text: &str) -> Trace {
    let mut trace = Trace::default();
    let mut returns = HashMap::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            // The last line of a trace cut off mid-write.
            trace.bad += 1;
            continue;
        };
        let u = |k: &str| v.get(k).and_then(|x| x.as_u64());
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_owned();
        if let Some(lost) = u("lost") {
            trace.lost += lost;
        } else if v.get("fn").is_some() {
            trace.calls.push(Call {
                seq: u("seq").unwrap_or(0),
                t: u("t").unwrap_or(0),
                thread: u("thread").unwrap_or(0),
                dll: s("dll"),
                func: s("fn"),
                args: v
                    .get("args")
                    .and_then(|a| a.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_u64()).collect())
                    .unwrap_or_default(),
                caller: s("caller"),
                ret: None,
            });
        } else if let (Some(seq), Some(ret)) = (u("seq"), u("ret")) {
            returns.insert(seq, ret);
        } else {
            trace.bad += 1;
        }
    }
    trace.calls.sort_by_key(|c| c.seq);
    for call in &mut trace.calls {
        call.ret = returns.get(&call.seq).copied();
    }
    trace
}

/// Each thread's calls, threads in the order of their first call.
fn threads(trace: &Trace) -> Vec<(u64, Vec<&Call>)> {
    let mut order: Vec<u64> = Vec::new();
    let mut by: HashMap<u64, Vec<&Call>> = HashMap::new();
    for call in &trace.calls {
        by.entry(call.thread)
            .or_insert_with(|| {
                order.push(call.thread);
                Vec::new()
            })
            .push(call);
    }
    order
        .into_iter()
        .map(|t| (t, by.remove(&t).unwrap_or_default()))
        .collect()
}

/// Calls per `dll!fn`, most first.
fn counts(trace: &Trace) -> Vec<(String, u64)> {
    let mut counts: HashMap<String, u64> = HashMap::new();
    for call in &trace.calls {
        *counts.entry(call.name()).or_default() += 1;
    }
    let mut counts: Vec<_> = counts.into_iter().collect();
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    counts
}

fn summary(trace: &Trace, limit: usize) -> String {
    let mut out = String::new();
    let threads = threads(trace);
    let span = match (trace.calls.first(), trace.calls.last()) {
        (Some(a), Some(b)) => b.t.saturating_sub(a.t),
        _ => 0,
    };
    out += &format!(
        "{} calls on {} threads over {:.3} s",
        trace.calls.len(),
        threads.len(),
        span as f64 / 1e6
    );
    if trace.lost > 0 {
        out += &format!(", {} events lost (the writer fell behind)", trace.lost);
    }
    if trace.bad > 0 {
        out += &format!(", {} unreadable lines", trace.bad);
    }
    out += "\n\ntop functions:\n";
    let counts = counts(trace);
    for (name, n) in counts.iter().take(limit) {
        let share = *n as f64 * 100.0 / trace.calls.len().max(1) as f64;
        out += &format!("  {n:>10}  {share:>5.1}%  {name}\n");
    }
    if counts.len() > limit {
        out += &format!("  ... {} more (--limit)\n", counts.len() - limit);
    }
    out += "\nwhere each thread stopped:\n";
    for (thread, calls) in &threads {
        let first = calls.first().map_or(0, |c| c.t);
        let last = calls.last().map_or(0, |c| c.t);
        out += &format!(
            "  thread {thread}: {} calls, t={first}us..{last}us\n",
            calls.len()
        );
        // Calls without a return, outermost first: the one a blocked thread
        // sits in is the last.
        let open: Vec<&&Call> = calls.iter().filter(|c| c.ret.is_none()).collect();
        for call in open.iter().rev().take(5).rev() {
            out += &format!("    still in  {}\n", call.line());
        }
        if open.len() > 5 {
            out += &format!(
                "    ... {} earlier calls without a return\n",
                open.len() - 5
            );
        }
        if let Some(call) = calls.last().filter(|c| c.ret.is_some()) {
            out += &format!("    last call {}\n", call.line());
        }
    }
    out
}

/// Pairs the threads of `a` and `b`: same first calls first, then by the
/// order they started in. Unpaired threads get `None`.
fn pair<'t>(
    a: &'t [(u64, Vec<&'t Call>)],
    b: &'t [(u64, Vec<&'t Call>)],
) -> Vec<(Option<usize>, Option<usize>)> {
    let start = |calls: &[&Call]| -> Vec<(String, String, String)> {
        calls
            .iter()
            .take(3)
            .map(|c| (c.dll.to_ascii_lowercase(), c.func.clone(), c.caller.clone()))
            .collect()
    };
    let mut taken = vec![false; b.len()];
    let mut pairs: Vec<(Option<usize>, Option<usize>)> = Vec::new();
    let mut unmatched = Vec::new();
    for (i, (_, calls)) in a.iter().enumerate() {
        let key = start(calls);
        match (0..b.len()).find(|&j| !taken[j] && start(&b[j].1) == key) {
            Some(j) => {
                taken[j] = true;
                pairs.push((Some(i), Some(j)));
            }
            None => unmatched.push(i),
        }
    }
    let mut rest = (0..b.len())
        .filter(|&j| !taken[j])
        .collect::<Vec<_>>()
        .into_iter();
    for i in unmatched {
        pairs.push((Some(i), rest.next()));
    }
    pairs.extend(rest.map(|j| (None, Some(j))));
    pairs.sort_by_key(|p| (p.0.unwrap_or(usize::MAX), p.1));
    pairs
}

/// Calls shown around a divergence.
const CONTEXT: usize = 5;

fn diff(a: &Trace, b: &Trace, limit: usize) -> String {
    let mut out = format!(
        "a: {} calls on {} threads; b: {} calls on {} threads\n",
        a.calls.len(),
        threads(a).len(),
        b.calls.len(),
        threads(b).len()
    );
    let (ta, tb) = (threads(a), threads(b));
    out += "\nper thread (paired by their first calls, then start order):\n";
    for (i, j) in pair(&ta, &tb) {
        let (i, j) = match (i, j) {
            (Some(i), Some(j)) => (i, j),
            (Some(i), None) => {
                out += &format!(
                    "  thread a:{} ({} calls): only in a\n",
                    ta[i].0,
                    ta[i].1.len()
                );
                continue;
            }
            (None, Some(j)) => {
                out += &format!(
                    "  thread b:{} ({} calls): only in b\n",
                    tb[j].0,
                    tb[j].1.len()
                );
                continue;
            }
            (None, None) => continue,
        };
        let ((id_a, ca), (id_b, cb)) = (&ta[i], &tb[j]);
        let same = ca
            .iter()
            .zip(cb.iter())
            .take_while(|(x, y)| x.key() == y.key())
            .count();
        let head = format!("  thread a:{id_a} ~ b:{id_b}");
        if same == ca.len() && same == cb.len() {
            out += &format!("{head}: the same {same} calls\n");
            continue;
        }
        let parted = match (ca.get(same), cb.get(same)) {
            (Some(x), Some(y)) => format!("a calls {}, b calls {}", x.name(), y.name()),
            (Some(_), None) => format!("b ends there, a goes on for {}", ca.len() - same),
            (None, _) => format!("a ends there, b goes on for {}", cb.len() - same),
        };
        out += &format!("{head}: the same for {same} calls, then {parted}\n");
        for call in &ca[same.saturating_sub(CONTEXT)..same] {
            out += &format!("        both  {}\n", call.line());
        }
        for call in ca.iter().skip(same).take(CONTEXT) {
            out += &format!("      a only  {}\n", call.line());
        }
        for call in cb.iter().skip(same).take(CONTEXT) {
            out += &format!("      b only  {}\n", call.line());
        }
    }
    let mut table: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for (name, n) in counts(a) {
        table.entry(name).or_default().0 = n;
    }
    for (name, n) in counts(b) {
        table.entry(name).or_default().1 = n;
    }
    let mut rows: Vec<_> = table.into_iter().collect();
    rows.sort_by(|(na, (a1, b1)), (nb, (a2, b2))| {
        let delta = |x: &u64, y: &u64| x.abs_diff(*y);
        delta(a2, b2)
            .cmp(&delta(a1, b1))
            .then((a2 + b2).cmp(&(a1 + b1)))
            .then(na.cmp(nb))
    });
    out += &format!(
        "\ncalls per function, biggest difference first:\n  {:>10}  {:>10}  {:>10}  function\n",
        "a", "b", "b - a"
    );
    for (name, (x, y)) in rows.iter().take(limit) {
        out += &format!(
            "  {x:>10}  {y:>10}  {:>+10}  {name}\n",
            *y as i64 - *x as i64
        );
    }
    if rows.len() > limit {
        out += &format!("  ... {} more (--limit)\n", rows.len() - limit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(seq: u64, thread: u64, func: &str, caller: &str) -> String {
        format!(
            r#"{{"seq":{seq},"t":{},"thread":{thread},"dll":"k.dll","fn":"{func}","args":[1,2,3,4],"caller":"{caller}"}}"#,
            seq * 10
        )
    }

    fn ret(seq: u64, thread: u64, value: u64) -> String {
        format!(
            r#"{{"seq":{seq},"t":{},"thread":{thread},"ret":{value}}}"#,
            seq * 10 + 1
        )
    }

    fn trace(lines: &[String]) -> Trace {
        parse(&lines.join("\n"))
    }

    #[test]
    fn calls_are_joined_to_their_returns() {
        let t = trace(&[
            call(0, 5, "Open", "t.exe+0x10"),
            ret(0, 5, 3),
            call(1, 5, "Wait", "t.exe+0x20"),
            r#"{"lost":4}"#.into(),
            r#"{"seq":2,"t":"#.into(),
        ]);
        assert_eq!(t.calls.len(), 2);
        assert_eq!(t.calls[0].ret, Some(3));
        assert_eq!(t.calls[1].ret, None);
        assert_eq!(t.calls[1].args, [1, 2, 3, 4]);
        assert_eq!((t.lost, t.bad), (4, 1));
    }

    #[test]
    fn the_summary_says_where_each_thread_stopped() {
        let t = trace(&[
            call(0, 5, "Open", "t.exe+0x10"),
            ret(0, 5, 3),
            call(1, 7, "Open", "t.exe+0x10"),
            ret(1, 7, 4),
            call(2, 5, "Wait", "t.exe+0x20"),
            call(3, 7, "Sleep", "t.exe+0x30"),
            ret(3, 7, 0),
        ]);
        let text = summary(&t, 20);
        assert!(text.starts_with("4 calls on 2 threads"), "{text}");
        assert!(text.contains("         2   50.0%  k.dll!Open"), "{text}");
        assert!(text.contains("  thread 5: 2 calls"), "{text}");
        assert!(
            text.contains(
                "    still in  #2 t=20us k.dll!Wait(0x1, 0x2, 0x3, 0x4) (no return) from t.exe+0x20"
            ),
            "{text}"
        );
        assert!(
            text.contains("    last call #3 t=30us k.dll!Sleep"),
            "{text}"
        );
    }

    #[test]
    fn a_diff_pairs_threads_and_finds_where_they_part() {
        // Thread ids differ between runs; the threads start the same way.
        let a = trace(&[
            call(0, 1, "Init", "t.exe+0x1"),
            call(1, 2, "Worker", "t.exe+0x2"),
            call(2, 1, "Load", "t.exe+0x3"),
            call(3, 1, "Wait", "t.exe+0x4"),
            call(4, 2, "Sleep", "t.exe+0x5"),
        ]);
        let b = trace(&[
            call(0, 9, "Worker", "t.exe+0x2"),
            call(1, 8, "Init", "t.exe+0x1"),
            call(2, 9, "Sleep", "t.exe+0x5"),
            call(3, 8, "Load", "t.exe+0x3"),
            call(4, 8, "Signal", "t.exe+0x6"),
            call(5, 8, "Wait", "t.exe+0x4"),
        ]);
        let text = diff(&a, &b, 30);
        assert!(text.contains("thread a:1 ~ b:8: the same for 2 calls, then a calls k.dll!Wait, b calls k.dll!Signal"), "{text}");
        assert!(
            text.contains("thread a:2 ~ b:9: the same 2 calls"),
            "{text}"
        );
        assert!(
            text.contains("        both  #2 t=20us k.dll!Load"),
            "{text}"
        );
        assert!(
            text.contains("      b only  #4 t=40us k.dll!Signal"),
            "{text}"
        );
        assert!(
            text.contains("           0           1          +1  k.dll!Signal"),
            "{text}"
        );
    }

    #[test]
    fn a_thread_only_one_run_has_is_named() {
        let a = trace(&[call(0, 1, "Init", "t.exe+0x1")]);
        let b = trace(&[
            call(0, 1, "Init", "t.exe+0x1"),
            call(1, 2, "Extra", "t.exe+0x9"),
        ]);
        let text = diff(&a, &b, 30);
        assert!(text.contains("thread b:2 (1 calls): only in b"), "{text}");
        assert!(
            text.contains("thread a:1 ~ b:1: the same 1 calls"),
            "{text}"
        );
    }
}
