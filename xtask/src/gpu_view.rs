//! `cargo xtask gpu-view <dir>`: one self-contained `view.html` beside a GPU
//! command recording (`VICTORY_GPU_RECORD`, `victory_gpu::gpu_record`).
//!
//! The page needs nothing from the network (inline CSS, inline SVG, a few
//! lines of inline script), so it opens from disk anywhere. Per frame it
//! shows the totals, a timeline of each queue's submissions, signals and
//! waits, and each command list as a tree of passes (a pass begins where
//! the list sets render targets); over all frames, the pipelines with the
//! most draws. The page lands in the recording's folder, which must lie
//! outside the repository, as every tool's output does.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;

pub fn run(root: &Path, dir: &Path) -> Result<()> {
    crate::harness::ensure_outside_repo(root, dir)?;
    let frames = read_frames(dir)?;
    if frames.is_empty() {
        bail!("no frame-<n>.ndjson in {}", dir.display());
    }
    let out = dir.join("view.html");
    std::fs::write(&out, render(&frames)).with_context(|| format!("writing {}", out.display()))?;
    println!(
        "gpu-view: wrote {} ({} frames)",
        out.display(),
        frames.len()
    );
    Ok(())
}

/// One recorded frame: its number and its records in order.
pub struct Frame {
    pub number: u64,
    pub records: Vec<Value>,
}

/// Every `frame-<n>.ndjson` in `dir`, by frame number. Lines that do not
/// parse (a recording cut short) are left out.
fn read_frames(dir: &Path) -> Result<Vec<Frame>> {
    let mut found: Vec<(u64, PathBuf)> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter_map(|p| {
            let name = p.file_name()?.to_str()?;
            let n = name
                .strip_prefix("frame-")?
                .strip_suffix(".ndjson")?
                .parse()
                .ok()?;
            Some((n, p))
        })
        .collect();
    found.sort();
    found
        .into_iter()
        .map(|(number, path)| {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            Ok(Frame {
                number,
                records: parse_lines(&text),
            })
        })
        .collect()
}

pub fn parse_lines(text: &str) -> Vec<Value> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}

fn n(v: &Value, key: &str) -> u64 {
    v[key].as_u64().unwrap_or(0)
}

fn is_draw(op: &str) -> bool {
    matches!(op, "draw" | "draw_indexed" | "draw_mesh" | "indirect")
}

/// What a frame did, counted.
#[derive(Default, Debug, PartialEq)]
pub struct Totals {
    pub draws: u64,
    pub dispatches: u64,
    pub barriers: u64,
    pub copies: u64,
    pub clears: u64,
    pub submits: u64,
    pub lists: u64,
}

pub fn totals(records: &[Value]) -> Totals {
    let mut t = Totals::default();
    let mut lists = std::collections::HashSet::new();
    for r in records {
        match s(r, "type") {
            "cmd" => {
                lists.insert(s(r, "list").to_owned());
                match s(r, "op") {
                    op if is_draw(op) => t.draws += 1,
                    "dispatch" => t.dispatches += 1,
                    "barrier" => t.barriers += 1,
                    "copy" => t.copies += 1,
                    "clear" => t.clears += 1,
                    _ => {}
                }
            }
            "submit" => t.submits += 1,
            _ => {}
        }
    }
    t.lists = lists.len() as u64;
    t
}

/// A run of a command list between render-target changes.
#[derive(Default)]
struct Pass {
    label: String,
    draws: u64,
    dispatches: u64,
    barriers: u64,
    copies: u64,
    clears: u64,
    /// Consecutive commands of one kind, as `(op, count, detail)`.
    runs: Vec<(String, u64, String)>,
}

impl Pass {
    fn add(&mut self, r: &Value) {
        let op = s(r, "op");
        match op {
            op if is_draw(op) => self.draws += 1,
            "dispatch" => self.dispatches += 1,
            "barrier" => self.barriers += 1,
            "copy" => self.copies += 1,
            "clear" => self.clears += 1,
            _ => {}
        }
        let detail = detail(r);
        let count = if op == "root" {
            n(r, "count").max(1)
        } else {
            1
        };
        match self.runs.last_mut() {
            Some((last, c, d)) if last == op && (op != "pipeline" || *d == detail) => {
                *c += count;
                if op != "pipeline" {
                    *d = String::new();
                }
            }
            _ => self.runs.push((op.to_owned(), count, detail)),
        }
    }
}

/// What a command record says besides its kind, for one shown alone.
fn detail(r: &Value) -> String {
    match s(r, "op") {
        "draw" => format!(
            "{} vertices x {} instances",
            n(r, "vertices"),
            n(r, "instances")
        ),
        "draw_indexed" => format!(
            "{} indices x {} instances",
            n(r, "indices"),
            n(r, "instances")
        ),
        "draw_mesh" => format!("{} groups", n(r, "groups")),
        "dispatch" => format!("{} x {} x {}", n(r, "x"), n(r, "y"), n(r, "z")),
        "indirect" => format!("up to {}", n(r, "max")),
        "barrier" => format!("{} barriers", n(r, "count")),
        "copy" | "clear" => s(r, "what").to_owned(),
        "pipeline" => pipeline_name(r),
        "targets" => format!(
            "{} targets, first {}, depth {}",
            n(r, "count"),
            s(r, "first"),
            s(r, "depth")
        ),
        "begin" => format!("kind {}", n(r, "kind")),
        _ => String::new(),
    }
}

/// A pipeline as its shaders name it, else by its address.
fn pipeline_name(r: &Value) -> String {
    let shaders: Vec<&str> = r["shaders"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if shaders.is_empty() {
        format!("pipeline {}", s(r, "pipeline"))
    } else {
        shaders.join(" + ")
    }
}

/// Each list's passes, lists in the order they first appear.
fn passes(records: &[Value]) -> Vec<(String, Vec<Pass>)> {
    let mut order: Vec<String> = Vec::new();
    let mut by_list: HashMap<String, Vec<Pass>> = HashMap::new();
    for r in records.iter().filter(|r| s(r, "type") == "cmd") {
        let list = s(r, "list").to_owned();
        let passes = by_list.entry(list.clone()).or_insert_with(|| {
            order.push(list.clone());
            Vec::new()
        });
        let op = s(r, "op");
        if op == "targets" || op == "begin" || passes.is_empty() {
            passes.push(Pass {
                label: match op {
                    "targets" => detail(r),
                    "begin" => format!("list begins ({})", detail(r)),
                    _ => "before any render targets".to_owned(),
                },
                ..Pass::default()
            });
        }
        if let Some(pass) = passes.last_mut()
            && op != "targets"
        {
            pass.add(r);
        }
    }
    order
        .into_iter()
        .map(|l| {
            let p = by_list.remove(&l).unwrap_or_default();
            (l, p)
        })
        .collect()
}

/// Pipelines by draws over all frames: `(name, draws, dispatches)`, most
/// first. A draw counts for the pipeline its list set last.
pub fn top_pipelines(frames: &[Frame]) -> Vec<(String, u64, u64)> {
    let mut counts: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for f in frames {
        let mut current: HashMap<&str, String> = HashMap::new();
        for r in f.records.iter().filter(|r| s(r, "type") == "cmd") {
            let list = s(r, "list");
            match s(r, "op") {
                "pipeline" => {
                    current.insert(list, pipeline_name(r));
                }
                "begin" => {
                    current.remove(list);
                }
                op if is_draw(op) || op == "dispatch" => {
                    let name = current
                        .get(list)
                        .cloned()
                        .unwrap_or_else(|| "(set before the recording)".to_owned());
                    let e = counts.entry(name).or_default();
                    if op == "dispatch" {
                        e.1 += 1;
                    } else {
                        e.0 += 1;
                    }
                }
                _ => {}
            }
        }
    }
    let mut v: Vec<(String, u64, u64)> = counts.into_iter().map(|(k, (d, c))| (k, d, c)).collect();
    v.sort_by(|a, b| (b.1 + b.2).cmp(&(a.1 + a.2)).then_with(|| a.0.cmp(&b.0)));
    v
}

fn esc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

/// The queues' calls in one frame as an SVG timeline: a row per queue,
/// submissions as bars, signals and waits as ticks.
fn timeline(records: &[Value]) -> String {
    const WIDTH: f64 = 1000.0;
    const ROW: f64 = 26.0;
    const LEFT: f64 = 150.0;
    let times: Vec<u64> = records.iter().filter_map(|r| r["t_us"].as_u64()).collect();
    let (Some(&t0), Some(&t1)) = (times.iter().min(), times.iter().max()) else {
        return String::new();
    };
    let span = (t1 - t0).max(1) as f64;
    let mut queues: Vec<(String, String)> = Vec::new();
    for r in records {
        if matches!(s(r, "type"), "submit" | "signal" | "wait") {
            let q = s(r, "queue").to_owned();
            if !queues.iter().any(|(name, _)| *name == q) {
                let kind = r["kind"].as_u64().map_or("?".to_owned(), |k| k.to_string());
                queues.push((q, kind));
            }
        }
    }
    if queues.is_empty() {
        return "<p class=\"muted\">No queue calls in this frame.</p>\n".to_owned();
    }
    let height = ROW * queues.len() as f64 + 20.0;
    let mut svg = format!(
        "<svg class=\"timeline\" viewBox=\"0 0 {} {height}\" role=\"img\" aria-label=\"queue timeline\">\n",
        LEFT + WIDTH + 10.0
    );
    for (i, (q, kind)) in queues.iter().enumerate() {
        let y = ROW * i as f64;
        let _ = writeln!(
            svg,
            "<text x=\"4\" y=\"{}\" class=\"qlabel\">queue {} (kind {})</text><line x1=\"{LEFT}\" x2=\"{}\" y1=\"{}\" y2=\"{}\" class=\"lane\"/>",
            y + 17.0,
            esc(q),
            esc(kind),
            LEFT + WIDTH,
            y + 13.0,
            y + 13.0
        );
    }
    for r in records {
        let kind = s(r, "type");
        let Some(row) = queues.iter().position(|(q, _)| q == s(r, "queue")) else {
            continue;
        };
        let Some(t) = r["t_us"].as_u64() else {
            continue;
        };
        let x = LEFT + (t - t0) as f64 / span * WIDTH;
        let y = ROW * row as f64;
        let what = match kind {
            "submit" => format!(
                "submit of {} lists at {} us",
                r["lists"].as_array().map_or(0, Vec::len),
                t - t0
            ),
            _ => format!(
                "{kind} fence {} value {} at {} us",
                s(r, "fence"),
                n(r, "value"),
                t - t0
            ),
        };
        let shape = match kind {
            "submit" => format!(
                "<rect x=\"{:.1}\" y=\"{}\" width=\"4\" height=\"18\" class=\"submit\">",
                x - 2.0,
                y + 4.0
            ),
            "signal" => format!(
                "<circle cx=\"{x:.1}\" cy=\"{}\" r=\"4\" class=\"signal\">",
                y + 13.0
            ),
            _ => format!(
                "<rect x=\"{:.1}\" y=\"{}\" width=\"8\" height=\"8\" class=\"wait\">",
                x - 4.0,
                y + 9.0
            ),
        };
        let close = if kind == "signal" {
            "</circle>"
        } else {
            "</rect>"
        };
        let _ = writeln!(svg, "{shape}<title>{}</title>{close}", esc(&what));
    }
    let _ = writeln!(
        svg,
        "<text x=\"{LEFT}\" y=\"{}\" class=\"axis\">0 us</text><text x=\"{}\" y=\"{}\" class=\"axis\" text-anchor=\"end\">{} us</text>",
        height - 4.0,
        LEFT + WIDTH,
        height - 4.0,
        t1 - t0
    );
    svg.push_str("</svg>\n");
    svg
}

const STYLE: &str = "
:root { --bg:#fff; --fg:#1d1d1f; --muted:#6e6e73; --line:#d2d2d7; --card:#f5f5f7;
  --submit:#0a64d6; --signal:#1a8f3c; --wait:#c4431c; }
@media (prefers-color-scheme: dark) { :root { --bg:#161618; --fg:#ececef; --muted:#9a9aa0;
  --line:#3a3a3f; --card:#202024; --submit:#5aa2ff; --signal:#4cc770; --wait:#ff7a52; } }
body { background:var(--bg); color:var(--fg); font:14px/1.45 system-ui,sans-serif; margin:0 16px 40px; }
h1 { font-size:20px; } h2 { font-size:17px; margin-top:28px; } h3 { font-size:15px; }
table { border-collapse:collapse; margin:8px 0; } th, td { border-bottom:1px solid var(--line);
  padding:3px 10px; text-align:right; } th:first-child, td:first-child { text-align:left; }
td.name { font-family:ui-monospace,monospace; font-size:12px; text-align:left; }
details { margin:2px 0 2px 14px; } summary { cursor:pointer; } .muted { color:var(--muted); }
.frame { background:var(--card); border-radius:8px; padding:4px 14px 12px; margin:16px 0; }
.timeline { width:100%; max-width:1160px; height:auto; }
.lane { stroke:var(--line); } .qlabel, .axis { fill:var(--muted); font-size:11px; }
.submit { fill:var(--submit); } .signal { fill:var(--signal); } .wait { fill:var(--wait); }
code { font-size:12px; } button { margin-right:6px; }
";

const SCRIPT: &str = "
function openAll(on) { document.querySelectorAll('details').forEach(function (d) { d.open = on; }); }
";

/// The whole page.
pub fn render(frames: &[Frame]) -> String {
    let mut h = String::new();
    h.push_str("<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n");
    h.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    h.push_str("<title>GPU command recording</title>\n<style>");
    h.push_str(STYLE);
    h.push_str("</style>\n<script>");
    h.push_str(SCRIPT);
    h.push_str("</script>\n</head>\n<body>\n<h1>GPU command recording</h1>\n");
    h.push_str("<p><button onclick=\"openAll(true)\">Expand all</button><button onclick=\"openAll(false)\">Collapse all</button></p>\n");

    h.push_str("<h2>Totals per frame</h2>\n<table id=\"totals\">\n<tr><th>Frame</th><th>Draws</th><th>Dispatches</th><th>Barriers</th><th>Copies</th><th>Clears</th><th>Submits</th><th>Lists</th></tr>\n");
    for f in frames {
        let t = totals(&f.records);
        let _ = writeln!(
            h,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            f.number, t.draws, t.dispatches, t.barriers, t.copies, t.clears, t.submits, t.lists
        );
    }
    h.push_str("</table>\n");

    h.push_str("<h2>Pipelines by draws (all frames)</h2>\n<table id=\"pipelines\">\n<tr><th>Pipeline (shaders)</th><th>Draws</th><th>Dispatches</th></tr>\n");
    for (name, draws, dispatches) in top_pipelines(frames).iter().take(40) {
        let _ = writeln!(
            h,
            "<tr><td class=\"name\">{}</td><td>{draws}</td><td>{dispatches}</td></tr>",
            esc(name)
        );
    }
    h.push_str("</table>\n");

    for f in frames {
        let t = totals(&f.records);
        let _ = writeln!(
            h,
            "<section class=\"frame\">\n<h2>Frame {}</h2>\n<p class=\"muted\">{} draws, {} dispatches, {} barriers, {} copies in {} lists and {} submits</p>",
            f.number, t.draws, t.dispatches, t.barriers, t.copies, t.lists, t.submits
        );
        h.push_str("<h3>Queues</h3>\n");
        h.push_str(&timeline(&f.records));
        h.push_str("<h3>Command lists</h3>\n");
        for (list, passes) in passes(&f.records) {
            let draws: u64 = passes.iter().map(|p| p.draws).sum();
            let dispatches: u64 = passes.iter().map(|p| p.dispatches).sum();
            let _ = writeln!(
                h,
                "<details><summary>list <code>{}</code>: {} passes, {draws} draws, {dispatches} dispatches</summary>",
                esc(&list),
                passes.len()
            );
            for p in &passes {
                let _ = writeln!(
                    h,
                    "<details><summary>{}: {} draws, {} dispatches, {} barriers, {} copies, {} clears</summary>",
                    esc(&p.label),
                    p.draws,
                    p.dispatches,
                    p.barriers,
                    p.copies,
                    p.clears
                );
                h.push_str("<table>\n");
                for (op, count, d) in &p.runs {
                    let _ = writeln!(
                        h,
                        "<tr><td>{}</td><td>{count}</td><td class=\"name\">{}</td></tr>",
                        esc(op),
                        esc(d)
                    );
                }
                h.push_str("</table>\n</details>\n");
            }
            h.push_str("</details>\n");
        }
        h.push_str("</section>\n");
    }
    h.push_str("</body>\n</html>\n");
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two lists on two queues: a depth pass and a colour pass of draws,
    /// then a compute list waited for by the graphics queue.
    const FRAME: &str = r#"{"type":"frame","frame":7,"t_us":1000,"draws_before":4200}
{"type":"cmd","t_us":1001,"list":"0xa0","op":"begin","kind":0}
{"type":"cmd","t_us":1002,"list":"0xa0","op":"targets","count":0,"first":"0x0","depth":"0x900"}
{"type":"cmd","t_us":1003,"list":"0xa0","op":"pipeline","pipeline":"0x51","shaders":["vertex 1111222233334444"]}
{"type":"cmd","t_us":1004,"list":"0xa0","op":"root","count":5}
{"type":"cmd","t_us":1005,"list":"0xa0","op":"draw_indexed","indices":36,"instances":1}
{"type":"cmd","t_us":1006,"list":"0xa0","op":"draw_indexed","indices":72,"instances":2}
{"type":"cmd","t_us":1007,"list":"0xa0","op":"barrier","count":2}
{"type":"cmd","t_us":1008,"list":"0xa0","op":"targets","count":2,"first":"0x400","depth":"0x900"}
{"type":"cmd","t_us":1009,"list":"0xa0","op":"clear","what":"rtv"}
{"type":"cmd","t_us":1010,"list":"0xa0","op":"pipeline","pipeline":"0x52","shaders":["vertex 1111222233334444","pixel 5555666677778888"]}
{"type":"cmd","t_us":1011,"list":"0xa0","op":"draw","vertices":3,"instances":1}
{"type":"cmd","t_us":1012,"list":"0xa0","op":"draw","vertices":3,"instances":1}
{"type":"cmd","t_us":1013,"list":"0xa0","op":"draw","vertices":6,"instances":1}
{"type":"cmd","t_us":1014,"list":"0xa0","op":"resolve"}
{"type":"cmd","t_us":1015,"list":"0xa0","op":"close"}
{"type":"cmd","t_us":1016,"list":"0xb0","op":"begin","kind":2}
{"type":"cmd","t_us":1017,"list":"0xb0","op":"pipeline","pipeline":"0x53","shaders":["compute 9999aaaabbbbcccc"]}
{"type":"cmd","t_us":1018,"list":"0xb0","op":"dispatch","x":8,"y":8,"z":1}
{"type":"cmd","t_us":1019,"list":"0xb0","op":"copy","what":"buffer"}
{"type":"cmd","t_us":1020,"list":"0xb0","op":"close"}
{"type":"submit","t_us":1030,"queue":"0xc0","kind":2,"lists":["0xb0"]}
{"type":"signal","t_us":1031,"queue":"0xc0","kind":2,"fence":"0xf0","value":9}
{"type":"wait","t_us":1032,"queue":"0xd0","kind":0,"fence":"0xf0","value":9}
{"type":"submit","t_us":1040,"queue":"0xd0","kind":0,"lists":["0xa0"]}
this line is cut shor
{"type":"present","frame":7,"t_us":1050}
"#;

    fn frames() -> Vec<Frame> {
        vec![Frame {
            number: 7,
            records: parse_lines(FRAME),
        }]
    }

    #[test]
    fn a_frame_is_counted_by_kind_of_command() {
        let f = frames();
        assert_eq!(
            totals(&f[0].records),
            Totals {
                draws: 5,
                dispatches: 1,
                barriers: 1,
                copies: 1,
                clears: 1,
                submits: 2,
                lists: 2,
            }
        );
    }

    #[test]
    fn passes_split_at_render_target_changes() {
        let f = frames();
        let lists = passes(&f[0].records);
        assert_eq!(lists.len(), 2);
        let (name, graphics) = &lists[0];
        assert_eq!(name, "0xa0");
        // The begin, the depth pass, the colour pass.
        assert_eq!(graphics.len(), 3);
        assert_eq!(graphics[1].draws, 2);
        assert_eq!(graphics[2].draws, 3);
        assert_eq!(graphics[2].clears, 1);
        // Consecutive draws are one run; the five root calls one row.
        assert!(
            graphics[2]
                .runs
                .iter()
                .any(|(op, n, _)| op == "draw" && *n == 3)
        );
        assert!(
            graphics[1]
                .runs
                .iter()
                .any(|(op, n, _)| op == "root" && *n == 5)
        );
    }

    #[test]
    fn draws_count_for_the_pipeline_set_last() {
        let top = top_pipelines(&frames());
        assert_eq!(
            top[0],
            (
                "vertex 1111222233334444 + pixel 5555666677778888".to_owned(),
                3,
                0
            )
        );
        assert_eq!(top[1], ("vertex 1111222233334444".to_owned(), 2, 0));
        assert_eq!(top[2], ("compute 9999aaaabbbbcccc".to_owned(), 0, 1));
    }

    #[test]
    fn the_page_is_whole_html_with_the_counts_and_no_outside_links() {
        let html = render(&frames());
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.trim_end().ends_with("</html>"));
        for tag in [
            "html", "head", "body", "table", "tr", "details", "summary", "svg", "section",
            "script", "style",
        ] {
            let opens = html.matches(&format!("<{tag}>")).count()
                + html.matches(&format!("<{tag} ")).count();
            let closes = html.matches(&format!("</{tag}>")).count();
            assert_eq!(
                opens, closes,
                "<{tag}> opened {opens} times, closed {closes}"
            );
        }
        assert!(!html.contains("http:") && !html.contains("https:") && !html.contains("src="));
        // Frame 7: 5 draws, 1 dispatch, 1 barrier, 1 copy, 1 clear, 2 submits, 2 lists.
        assert!(html.contains(
            "<tr><td>7</td><td>5</td><td>1</td><td>1</td><td>1</td><td>1</td><td>2</td><td>2</td></tr>"
        ));
        assert!(html.contains("2 targets, first 0x400, depth 0x900: 3 draws"));
        assert!(html.contains("queue 0xc0 (kind 2)"));
        assert!(html.contains("submit of 1 lists at 30 us"));
        assert!(html.contains("<td>3</td><td>0</td>"));
    }

    #[test]
    fn the_page_is_written_beside_the_recording_and_never_in_the_repository() {
        let root = crate::workspace_root();
        assert!(run(&root, &root.join("gpu-record")).is_err());
        let dir =
            std::env::temp_dir().join(format!("victory-gpu-view-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("frame-7.ndjson"), FRAME).unwrap();
        run(&root, &dir).unwrap();
        let html = std::fs::read_to_string(dir.join("view.html")).unwrap();
        assert!(html.contains("<h2>Frame 7</h2>"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
