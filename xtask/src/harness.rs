//! Clean-room diagnostic harness for analyzing VIctory runtime logs,
//! comparing A/B runs, preparing isolated test profiles, and inspecting
//! individual shader translation failures.
//!
//! All generated artifacts (reports, metadata, launcher scripts) MUST be
//! written strictly outside the repository (`ensure_outside_repo`).

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Subcommand, ValueEnum};
use serde_json::{Value, json};

#[derive(Subcommand)]
pub enum HarnessCmd {
    /// Parse a runtime log into frame-time waterfall, settle, VRAM, and shader diagnostics.
    Analyze {
        /// Path to a runtime log file (outside the repository).
        log: PathBuf,
        /// Optional external output directory for report.json and report.md.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Compare two harness report.json files side-by-side (A vs B).
    Compare {
        /// Baseline report.json path.
        a: PathBuf,
        /// Candidate report.json path.
        b: PathBuf,
    },
    /// Generate a timestamped external test-profile folder with run.ps1, launch.cmd, and run_meta.json.
    Prepare {
        /// Built-in diagnostic / tuning profile.
        #[arg(long, value_enum)]
        profile: Profile,
        /// External output root directory (must be outside the repository).
        #[arg(long)]
        out: PathBuf,
        /// Path to the title executable inside the user's testing copy.
        #[arg(long)]
        game_exe: PathBuf,
        /// Path to external victory.toml.
        #[arg(long)]
        config: PathBuf,
        /// Path to victory.exe (defaults to target/diag/victory.exe in this workspace).
        #[arg(long)]
        binary: Option<PathBuf>,
        /// Enable PNG frame captures into <run_dir>/frames via VICTORY_CAPTURE.
        #[arg(long)]
        capture: bool,
        /// Enable ray-tracing BVH builder (VICTORY_BVH_BUILDER=1).
        #[arg(long)]
        bvh: bool,
        /// Enable safe BVH quarantine mode (VICTORY_BVH_QUARANTINE=1).
        #[arg(long)]
        bvh_quarantine: bool,
        /// Comma-separated shader hashes for VICTORY_SKIP_PIPELINES.
        #[arg(long)]
        skip_pipelines: Option<String>,
        /// Extra KEY=VALUE environment overrides (repeatable).
        #[arg(long = "extra")]
        extra: Vec<String>,
    },
    /// Run `victory translate --validate --pipelines --list` filtered to one shader hash.
    Shader {
        /// Shader container hash prefix (16-hex or prefix shown in runtime logs).
        hash: String,
        /// Path to external victory.toml.
        #[arg(long)]
        config: PathBuf,
        /// Path to victory.exe (defaults to target/diag/victory.exe in this workspace).
        #[arg(long)]
        binary: Option<PathBuf>,
        /// Optional explicit shader paths (defaults to configured game_dir).
        paths: Vec<PathBuf>,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Profile {
    #[value(name = "baseline-1080p")]
    Baseline1080p,
    #[value(name = "tuned-8gb")]
    Tuned8gb,
    /// `tuned-8gb` plus windows cut to what is read: a 1 MiB grid for asks
    /// with a length and tails for root views (`shadow.rs` `window`,
    /// `tails`). Run 65 had ~1.7 GB of its ~3.5 GB of windows outside any
    /// asked-for range, all of it in PC memory the card reads over PCIe.
    /// Also clamps indirect records a window at a time (`draw.rs`
    /// `VICTORY_INDIRECT_WINDOW`), which costs no memory: card time 165 ms
    /// to 89 ms in victory-all-forks' measurements.
    #[value(name = "trim-8gb")]
    Trim8gb,
    #[value(name = "bvh-test")]
    BvhTest,
    #[value(name = "null-magenta")]
    NullMagenta,
    #[value(name = "dred-debug")]
    DredDebug,
    #[value(name = "shader-bisect")]
    ShaderBisect,
}

impl Profile {
    fn slug(self) -> &'static str {
        match self {
            Self::Baseline1080p => "baseline-1080p",
            Self::Tuned8gb => "tuned-8gb",
            Self::Trim8gb => "trim-8gb",
            Self::BvhTest => "bvh-test",
            Self::NullMagenta => "null-magenta",
            Self::DredDebug => "dred-debug",
            Self::ShaderBisect => "shader-bisect",
        }
    }
}

pub fn run(root: &Path, cmd: HarnessCmd) -> Result<()> {
    match cmd {
        HarnessCmd::Analyze { log, out } => analyze_cmd(root, &log, out.as_deref()),
        HarnessCmd::Compare { a, b } => compare_cmd(&a, &b),
        HarnessCmd::Prepare {
            profile,
            out,
            game_exe,
            config,
            binary,
            capture,
            bvh,
            bvh_quarantine,
            skip_pipelines,
            extra,
        } => prepare_cmd(
            root,
            profile,
            &out,
            &game_exe,
            &config,
            binary.as_deref(),
            capture,
            bvh,
            bvh_quarantine,
            skip_pipelines.as_deref(),
            &extra,
        ),
        HarnessCmd::Shader {
            hash,
            config,
            binary,
            paths,
        } => shader_cmd(root, &hash, &config, binary.as_deref(), &paths),
    }
}

/// Rejects any output path inside the repository or any other Victory clone.
pub fn ensure_outside_repo(repo_root: &Path, candidate: &Path) -> Result<()> {
    let abs = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        std::env::current_dir()?.join(candidate)
    };
    let clean_abs = lexically_clean(&abs);
    let clean_root = lexically_clean(
        &std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf()),
    );
    if clean_abs.starts_with(&clean_root) {
        bail!(
            "refusing to write harness output inside the repository: {}",
            candidate.display()
        );
    }
    // Also check canonicalized deepest existing ancestor against canonical repo root and markers.
    let mut cur = clean_abs.as_path();
    while !cur.exists() {
        let Some(parent) = cur.parent() else {
            break;
        };
        cur = parent;
    }
    if cur.exists() {
        let canon_cur = std::fs::canonicalize(cur).unwrap_or_else(|_| cur.to_path_buf());
        if canon_cur.starts_with(&clean_root) {
            bail!(
                "refusing to write harness output inside the repository: {}",
                candidate.display()
            );
        }
        let mut check = Some(canon_cur.as_path());
        while let Some(dir) = check {
            if dir.join(".victory-root").exists() {
                bail!(
                    "refusing to write harness output inside a VIctory clone ({}): {}",
                    dir.display(),
                    candidate.display()
                );
            }
            check = dir.parent();
        }
    }
    Ok(())
}

fn lexically_clean(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

#[derive(Default)]
struct FrameSample {
    fps: f64,
    frame_ms: f64,
    prep_ms: f64,
    copies_ms: f64,
    copies_mb: f64,
    textures_ms: f64,
    textures_mb: f64,
    gpu_ms: f64,
    copyback_ms: f64,
    copyback_mb: f64,
    draws: u64,
    dropped_pipeline: u64,
    dropped_indirect: u64,
    heap_buf_ms: f64,
    null_buffer_views: u64,
}

#[derive(Default)]
struct ParsedLog {
    frames: Vec<FrameSample>,
    rec_cpu_ms: Vec<f64>,
    rec_guards_ms: Vec<f64>,
    rec_calls: Vec<f64>,
    settle_counts: Vec<f64>,
    settle_wait_ms: Vec<f64>,
    submitting_wait_ms: Vec<f64>,
    gpu_busy_pct: Vec<f64>,
    relay_land_wait_ms: Vec<f64>,
    relay_fetch_wait_ms: Vec<f64>,
    relay_landing_ms: Vec<f64>,
    vram_used_mb: Vec<f64>,
    vram_budget_mb: Vec<f64>,
    vram_rt_mb: Vec<f64>,
    vram_tex_mb: Vec<f64>,
    vram_buf_mb: Vec<f64>,
    vram_rest_mb: Vec<f64>,
    pc_shared_mb: Vec<f64>,
    pc_windows_mb: Vec<f64>,
    pc_buf_copies_mb: Vec<f64>,
    private_ram_mb: Vec<f64>,
    tex_queue_waiting: Vec<f64>,
    tex_passed_unmapped_max: u64,
    tex_made_null_max: u64,
    settle_reasons: BTreeMap<String, (f64, usize)>,
    dropped_reasons: BTreeMap<String, f64>,
    warnings: BTreeMap<String, usize>,
    errors: BTreeMap<String, usize>,
    refused_shaders: Vec<String>,
}

fn analyze_cmd(root: &Path, log: &Path, out: Option<&Path>) -> Result<()> {
    if let Some(out_dir) = out {
        ensure_outside_repo(root, out_dir)?;
    }
    let file = File::open(log).with_context(|| format!("opening log {}", log.display()))?;
    let parsed = parse_log(BufReader::new(file))?;
    let report = build_report(log, &parsed);
    let md = render_markdown(&report);
    println!("{md}");
    if let Some(out_dir) = out {
        std::fs::create_dir_all(out_dir)
            .with_context(|| format!("creating {}", out_dir.display()))?;
        let json_path = out_dir.join("report.json");
        let md_path = out_dir.join("report.md");
        std::fs::write(&json_path, serde_json::to_string_pretty(&report)?)?;
        std::fs::write(&md_path, &md)?;
        eprintln!(
            "harness: wrote {} and {}",
            json_path.display(),
            md_path.display()
        );
    }
    Ok(())
}

fn parse_log<R: BufRead>(reader: R) -> Result<ParsedLog> {
    let mut parsed = ParsedLog::default();
    for raw in reader.lines() {
        let raw = raw?;
        let line = strip_ansi(&raw);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(s) = parse_frame_line(trimmed) {
            parsed.frames.push(s);
            continue;
        }
        if trimmed.contains("recording per frame") {
            if let Some(v) = num_before(trimmed, " list calls ") {
                parsed.rec_calls.push(v);
            }
            if let Some(v) = num_between(trimmed, " list calls ", " ms (") {
                parsed.rec_cpu_ms.push(v);
            }
            if let Some(v) = num_between(trimmed, "guards ", " ms;") {
                parsed.rec_guards_ms.push(v);
            }
            continue;
        }
        if trimmed.contains("submissions sent ahead per frame:") {
            if let Some(v) = num_between(trimmed, " in ", " settles ") {
                parsed.settle_counts.push(v);
            }
            if let Some(v) = num_between(trimmed, "each time), ", " ms waiting for the card") {
                parsed.settle_wait_ms.push(v);
            }
            if let Some(clause) = slice_between(trimmed, "settled for: ", ";") {
                for item in clause.split(',') {
                    let item = item.trim();
                    if let Some((label, val)) = parse_trailing_number(item)
                        && val > 0.0
                    {
                        let entry = parsed
                            .settle_reasons
                            .entry(label.to_owned())
                            .or_insert((0.0, 0));
                        entry.0 += val;
                        entry.1 += 1;
                    }
                }
            }
            continue;
        }
        if trimmed.contains("the card's clock per frame:") {
            if let Some(v) = num_between(trimmed, "(the card about ", "% busy") {
                parsed.gpu_busy_pct.push(v);
            }
            if let Some(v) = num_after(trimmed, "the submitting thread stood waiting for the card ")
                .and_then(parse_first_f64)
            {
                parsed.submitting_wait_ms.push(v);
            }
            continue;
        }
        if trimmed.contains("the relay per frame") {
            if let Some(v) = num_between(trimmed, "that thread waited ", " ms for the card") {
                parsed.relay_land_wait_ms.push(v);
            }
            if let Some(v) = num_between(trimmed, " and ", " ms for fetches") {
                parsed.relay_fetch_wait_ms.push(v);
            }
            if let Some(v) = num_between(trimmed, ", and was ", " ms landing in all") {
                parsed.relay_landing_ms.push(v);
            }
            continue;
        }
        if trimmed.contains("the card's memory by kind:") {
            if let Some(v) = num_between(trimmed, "the card's memory by kind: ", " MB in use") {
                parsed.vram_used_mb.push(v);
            }
            if let Some(v) = num_between(trimmed, "of a budget of ", " MB:") {
                parsed.vram_budget_mb.push(v);
            }
            if let Some(v) =
                num_between(trimmed, "render targets, depth and unordered access textures ", " MB ")
            {
                parsed.vram_rt_mb.push(v);
            }
            if let Some(v) = num_between(trimmed, "textures the title fills ", " MB,") {
                parsed.vram_tex_mb.push(v);
            }
            if let Some(v) = num_between(trimmed, "title buffers ", " MB ") {
                parsed.vram_buf_mb.push(v);
            }
            if let Some(v) = num_between(trimmed, "the swap chain, the driver) ", " MB;") {
                parsed.vram_rest_mb.push(v);
            }
            if let Some(v) = num_between(trimmed, "the PC's memory the card reads: ", " MB in use") {
                parsed.pc_shared_mb.push(v);
            }
            if let Some(v) = num_between(trimmed, "of it windows ", " MB and") {
                parsed.pc_windows_mb.push(v);
            }
            if let Some(v) = num_between(trimmed, "title buffers' copies ", " MB") {
                parsed.pc_buf_copies_mb.push(v);
            }
            continue;
        }
        if trimmed.contains("memory: ") && trimmed.contains(" MB private (") {
            if let Some(v) = num_between(trimmed, "memory: ", " MB private") {
                parsed.private_ram_mb.push(v);
            }
            continue;
        }
        if trimmed.contains("textures picked by index, the queue:") {
            if let Some(v) = num_between(trimmed, "due ", " waiting for their memory") {
                // handles `), <W> waiting`
                let _ = v;
            }
            if let Some(after_due) = trimmed.split("frames ago), ").nth(1)
                && let Some(w) = parse_first_f64(after_due)
            {
                parsed.tex_queue_waiting.push(w);
            }
            if let Some(v) = num_before(trimmed, " passed over unmapped") {
                parsed.tex_passed_unmapped_max = parsed.tex_passed_unmapped_max.max(v as u64);
            }
            if let Some(v) = num_before(trimmed, " made slots found unmapped and made null") {
                parsed.tex_made_null_max = parsed.tex_made_null_max.max(v as u64);
            }
            continue;
        }
        if let Some(rest) = trimmed.split("without a host pipeline: ").nth(1)
            && let Some(prefix) = slice_between(trimmed, "dropped ", "/frame without")
            && let Ok(rate) = prefix.trim().parse::<f64>()
        {
            let entry = parsed.dropped_reasons.entry(rest.trim().to_owned()).or_insert(0.0);
            if rate > *entry {
                *entry = rate;
            }
            continue;
        }
        if (trimmed.contains("shader does not translate") || trimmed.contains("host pipeline refused"))
            && parsed.refused_shaders.len() < 100
        {
            parsed.refused_shaders.push(trimmed.to_owned());
        }
        if trimmed.contains(" WARN ") {
            let msg = trimmed.split(" WARN ").nth(1).unwrap_or(trimmed).trim().to_owned();
            *parsed.warnings.entry(msg).or_insert(0) += 1;
        } else if trimmed.contains(" ERROR ") {
            let msg = trimmed.split(" ERROR ").nth(1).unwrap_or(trimmed).trim().to_owned();
            *parsed.errors.entry(msg).or_insert(0) += 1;
        }
    }
    Ok(parsed)
}

fn parse_frame_line(line: &str) -> Option<FrameSample> {
    if !line.contains(" fps, ") || !line.contains(" ms/frame ") || !line.contains(" draws/frame") {
        return None;
    }
    let fps = num_before(line, " fps, ")?;
    let frame_ms = num_between(line, " fps, ", " ms/frame ")?;
    let prep_ms = num_between(line, " ms/frame (", " preparing:").unwrap_or(0.0);
    let copies_ms = num_between(line, "preparing: copies ", " ms ").unwrap_or(0.0);
    let copies_mb = slice_between(line, "preparing: copies ", " MB,")
        .and_then(|s| s.split(" ms ").nth(1))
        .and_then(parse_first_f64)
        .unwrap_or(0.0);
    let textures_ms = num_between(line, ", textures ", " ms ").unwrap_or(0.0);
    let textures_mb = slice_between(line, ", textures ", " MB;")
        .and_then(|s| s.split(" ms ").nth(1))
        .and_then(parse_first_f64)
        .unwrap_or(0.0);
    let gpu_ms = num_between(line, "; ", " on the GPU;").unwrap_or(0.0);
    let copyback_ms = num_between(line, " on the GPU; ", " copying back ").unwrap_or(0.0);
    let copyback_mb = num_between(line, " copying back ", " MB),").unwrap_or(0.0);
    let draws = num_between(line, "), ", " draws/frame").unwrap_or(0.0) as u64;
    let dropped_pipeline = num_between(line, "dropped ", " without a host pipeline").unwrap_or(0.0) as u64;
    let dropped_indirect = num_between(line, ") and ", " indirect;").unwrap_or(0.0) as u64;
    let heap_buf_ms = slice_between(line, "heap buffers ", " ms;")
        .and_then(|s| s.split(" passes ").nth(1))
        .and_then(parse_first_f64)
        .unwrap_or(0.0);
    let null_buffer_views = num_between(line, "buffer views left null as the host would refuse them: ", ";")
        .unwrap_or(0.0) as u64;

    Some(FrameSample {
        fps,
        frame_ms,
        prep_ms,
        copies_ms,
        copies_mb,
        textures_ms,
        textures_mb,
        gpu_ms,
        copyback_ms,
        copyback_mb,
        draws,
        dropped_pipeline,
        dropped_indirect,
        heap_buf_ms,
        null_buffer_views,
    })
}

fn stats_f64(vals: &[f64]) -> Value {
    if vals.is_empty() {
        return json!({ "count": 0, "min": 0.0, "p50": 0.0, "p95": 0.0, "max": 0.0, "avg": 0.0 });
    }
    let mut s = vals.to_vec();
    s.sort_by(f64::total_cmp);
    let n = s.len();
    let pct = |p: usize| s[((n - 1) * p) / 100];
    let avg = s.iter().sum::<f64>() / n as f64;
    json!({
        "count": n,
        "min": round1(s[0]),
        "p50": round1(pct(50)),
        "p95": round1(pct(95)),
        "max": round1(s[n - 1]),
        "avg": round1(avg),
    })
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn build_report(log: &Path, p: &ParsedLog) -> Value {
    let scene: Vec<&FrameSample> = p.frames.iter().filter(|f| f.draws > 60).collect();
    let all_fps: Vec<f64> = p.frames.iter().map(|f| f.fps).collect();
    let all_ms: Vec<f64> = p.frames.iter().map(|f| f.frame_ms).collect();
    let scene_fps: Vec<f64> = scene.iter().map(|f| f.fps).collect();
    let scene_ms: Vec<f64> = scene.iter().map(|f| f.frame_ms).collect();
    let scene_prep: Vec<f64> = scene.iter().map(|f| f.prep_ms).collect();
    let scene_copies: Vec<f64> = scene.iter().map(|f| f.copies_ms).collect();
    let scene_copies_mb: Vec<f64> = scene.iter().map(|f| f.copies_mb).collect();
    let scene_tex: Vec<f64> = scene.iter().map(|f| f.textures_ms).collect();
    let scene_tex_mb: Vec<f64> = scene.iter().map(|f| f.textures_mb).collect();
    let scene_gpu: Vec<f64> = scene.iter().map(|f| f.gpu_ms).collect();
    let scene_cb: Vec<f64> = scene.iter().map(|f| f.copyback_ms).collect();
    let scene_cb_mb: Vec<f64> = scene.iter().map(|f| f.copyback_mb).collect();
    let scene_draws: Vec<f64> = scene.iter().map(|f| f.draws as f64).collect();
    let scene_dropped: Vec<f64> = scene.iter().map(|f| f.dropped_pipeline as f64).collect();
    let scene_heap_buf: Vec<f64> = scene.iter().map(|f| f.heap_buf_ms).collect();
    let max_null_bv = p.frames.iter().map(|f| f.null_buffer_views).max().unwrap_or(0);
    let max_dropped_indirect = p.frames.iter().map(|f| f.dropped_indirect).max().unwrap_or(0);

    let peak_vram = p.vram_used_mb.iter().copied().fold(0.0_f64, f64::max);
    let min_budget = p.vram_budget_mb.iter().copied().fold(f64::INFINITY, f64::min);
    let budget_mb = if min_budget.is_finite() { min_budget } else { 0.0 };
    let peak_pc_shared = p.pc_shared_mb.iter().copied().fold(0.0_f64, f64::max);
    let peak_priv_ram = p.private_ram_mb.iter().copied().fold(0.0_f64, f64::max);

    let mut diagnoses = Vec::new();
    if budget_mb > 0.0 && peak_vram > budget_mb {
        diagnoses.push(format!(
            "VRAM_OVER_BUDGET: peak {peak_vram:.0} MB > budget {budget_mb:.0} MB (spilling up to {peak_pc_shared:.0} MB into PC shared memory)"
        ));
    }
    let p50_settle = stats_f64(&p.settle_wait_ms)["p50"].as_f64().unwrap_or(0.0);
    if p50_settle > 20.0 {
        diagnoses.push(format!(
            "HIGH_SETTLE_WAIT: p50 {p50_settle:.1} ms/frame waiting for GPU settles (check VICTORY_QUEUE_COPIES / VICTORY_RELAY_COPY_QUEUE)"
        ));
    }
    let p50_rec = stats_f64(&p.rec_cpu_ms)["p50"].as_f64().unwrap_or(0.0);
    if p50_rec > 50.0 {
        diagnoses.push(format!(
            "HIGH_CPU_RECORDING: p50 {p50_rec:.1} ms/frame in command recording"
        ));
    }
    if p.dropped_reasons.keys().any(|k| k.contains("ray-tracing structure builder")) {
        diagnoses.push(
            "BVH_BUILDER_OFF: ray-tracing structure builder is disabled (shadows/indirect RT lighting missing)"
                .to_owned(),
        );
    }
    if max_null_bv > 0 {
        diagnoses.push(format!(
            "NULL_BUFFER_VIEWS: up to {max_null_bv} buffer views left null (expected when BVH/buffers drop)"
        ));
    }
    if p.tex_made_null_max > 1000 {
        diagnoses.push(format!(
            "TEXTURE_SLOT_THRASH: {} slots made null / {} passed over unmapped",
            p.tex_made_null_max, p.tex_passed_unmapped_max
        ));
    }

    let settle_reasons: Value = p
        .settle_reasons
        .iter()
        .map(|(k, (sum, cnt))| (k.clone(), json!(round1(*sum / (*cnt).max(1) as f64))))
        .collect::<serde_json::Map<String, Value>>()
        .into();
    let dropped_reasons: Value = p
        .dropped_reasons
        .iter()
        .map(|(k, v)| (k.clone(), json!(round1(*v))))
        .collect::<serde_json::Map<String, Value>>()
        .into();

    json!({
        "log_path": log.display().to_string(),
        "frame_counts": {
            "total": p.frames.len(),
            "menu_or_intro": p.frames.len() - scene.len(),
            "scene_3d": scene.len(),
        },
        "all_frames": {
            "fps": stats_f64(&all_fps),
            "frame_ms": stats_f64(&all_ms),
        },
        "scene_3d": {
            "fps": stats_f64(&scene_fps),
            "frame_ms": stats_f64(&scene_ms),
            "preparing_ms": stats_f64(&scene_prep),
            "copies_ms": stats_f64(&scene_copies),
            "copies_mb": stats_f64(&scene_copies_mb),
            "textures_ms": stats_f64(&scene_tex),
            "textures_mb": stats_f64(&scene_tex_mb),
            "gpu_ms": stats_f64(&scene_gpu),
            "copyback_ms": stats_f64(&scene_cb),
            "copyback_mb": stats_f64(&scene_cb_mb),
            "draws_per_frame": stats_f64(&scene_draws),
            "dropped_pipeline_per_frame": stats_f64(&scene_dropped),
            "heap_buffers_ms": stats_f64(&scene_heap_buf),
            "max_dropped_indirect": max_dropped_indirect,
            "max_null_buffer_views": max_null_bv,
        },
        "sync_and_cpu": {
            "rec_cpu_ms": stats_f64(&p.rec_cpu_ms),
            "rec_guards_ms": stats_f64(&p.rec_guards_ms),
            "rec_list_calls": stats_f64(&p.rec_calls),
            "settles_per_frame": stats_f64(&p.settle_counts),
            "settle_wait_ms": stats_f64(&p.settle_wait_ms),
            "submitting_wait_ms": stats_f64(&p.submitting_wait_ms),
            "gpu_busy_pct": stats_f64(&p.gpu_busy_pct),
            "relay_land_wait_ms": stats_f64(&p.relay_land_wait_ms),
            "relay_fetch_wait_ms": stats_f64(&p.relay_fetch_wait_ms),
            "relay_landing_ms": stats_f64(&p.relay_landing_ms),
            "settle_reasons_avg": settle_reasons,
        },
        "memory": {
            "vram_used_mb": stats_f64(&p.vram_used_mb),
            "vram_budget_mb": round1(budget_mb),
            "vram_peak_mb": round1(peak_vram),
            "vram_rt_mb": stats_f64(&p.vram_rt_mb),
            "vram_tex_mb": stats_f64(&p.vram_tex_mb),
            "vram_buf_mb": stats_f64(&p.vram_buf_mb),
            "vram_rest_mb": stats_f64(&p.vram_rest_mb),
            "pc_shared_mb": stats_f64(&p.pc_shared_mb),
            "pc_shared_peak_mb": round1(peak_pc_shared),
            "pc_windows_mb": stats_f64(&p.pc_windows_mb),
            "pc_buf_copies_mb": stats_f64(&p.pc_buf_copies_mb),
            "private_ram_peak_mb": round1(peak_priv_ram),
            "tex_queue_waiting": stats_f64(&p.tex_queue_waiting),
            "tex_passed_unmapped_max": p.tex_passed_unmapped_max,
            "tex_made_null_max": p.tex_made_null_max,
        },
        "dropped_pipeline_reasons_max_per_frame": dropped_reasons,
        "warnings_count": p.warnings.len(),
        "errors_count": p.errors.len(),
        "refused_shader_samples": p.refused_shaders.len(),
        "diagnoses": diagnoses,
    })
}

fn render_markdown(r: &Value) -> String {
    let mut s = String::new();
    s.push_str("# VIctory Harness Log Analysis\n\n");
    s.push_str(&format!(
        "- **Log**: `{}`\n- **Samples**: {} total ({} intro/menu, {} 3D scene with >60 draws)\n\n",
        r["log_path"].as_str().unwrap_or(""),
        r["frame_counts"]["total"],
        r["frame_counts"]["menu_or_intro"],
        r["frame_counts"]["scene_3d"]
    ));

    if let Some(diags) = r["diagnoses"].as_array()
        && !diags.is_empty()
    {
        s.push_str("## Automated Diagnoses\n");
        for d in diags {
            if let Some(txt) = d.as_str() {
                s.push_str(&format!("- **{txt}**\n"));
            }
        }
        s.push('\n');
    }

    s.push_str("## 3D Scene Frame Waterfall (draws > 60)\n\n");
    s.push_str("| Metric | Min | p50 | p95 | Max |\n|---|---:|---:|---:|---:|\n");
    for (label, key) in [
        ("FPS", "fps"),
        ("Frame time (ms)", "frame_ms"),
        ("On GPU (ms)", "gpu_ms"),
        ("Preparing total (ms)", "preparing_ms"),
        ("  Buffer copies (ms)", "copies_ms"),
        ("  Textures (ms)", "textures_ms"),
        ("Copying back (ms)", "copyback_ms"),
        ("Heap buffers pass (ms)", "heap_buffers_ms"),
        ("Draws / frame", "draws_per_frame"),
        ("Dropped (no pipeline)", "dropped_pipeline_per_frame"),
    ] {
        let v = &r["scene_3d"][key];
        s.push_str(&format!(
            "| {label} | {} | {} | {} | {} |\n",
            v["min"], v["p50"], v["p95"], v["max"]
        ));
    }

    s.push_str("\n## CPU, Sync & Relay Contention\n\n");
    s.push_str("| Metric | p50 | p95 | Max |\n|---|---:|---:|---:|\n");
    for (label, key) in [
        ("CPU command recording (ms)", "rec_cpu_ms"),
        ("Guards overhead (ms)", "rec_guards_ms"),
        ("Settles / frame", "settles_per_frame"),
        ("Settle wait (ms)", "settle_wait_ms"),
        ("Submitting thread wait (ms)", "submitting_wait_ms"),
        ("Relay wait for card (ms)", "relay_land_wait_ms"),
        ("Relay wait for fetches (ms)", "relay_fetch_wait_ms"),
        ("GPU busy (%)", "gpu_busy_pct"),
    ] {
        let v = &r["sync_and_cpu"][key];
        s.push_str(&format!("| {label} | {} | {} | {} |\n", v["p50"], v["p95"], v["max"]));
    }

    s.push_str("\n## 8 GB VRAM & Memory Forensics\n\n");
    let m = &r["memory"];
    s.push_str(&format!(
        "- **Dedicated VRAM**: peak **{} MB** / budget **{} MB** (RTs p95: {} MB, Textures p95: {} MB, Buffers p95: {} MB, Driver/Rest p95: {} MB)\n",
        m["vram_peak_mb"],
        m["vram_budget_mb"],
        m["vram_rt_mb"]["p95"],
        m["vram_tex_mb"]["p95"],
        m["vram_buf_mb"]["p95"],
        m["vram_rest_mb"]["p95"]
    ));
    s.push_str(&format!(
        "- **PC Shared Memory Spill**: peak **{} MB** (windows p95: {} MB, buffer copies p95: {} MB)\n",
        m["pc_shared_peak_mb"], m["pc_windows_mb"]["p95"], m["pc_buf_copies_mb"]["p95"]
    ));
    s.push_str(&format!(
        "- **Texture Queue & Views**: waiting p95 {}, passed over unmapped max {}, made null max {}, null buffer views max {}, private RAM peak {} MB\n\n",
        m["tex_queue_waiting"]["p95"],
        m["tex_passed_unmapped_max"],
        m["tex_made_null_max"],
        r["scene_3d"]["max_null_buffer_views"],
        m["private_ram_peak_mb"]
    ));

    if let Some(drops) = r["dropped_pipeline_reasons_max_per_frame"].as_object()
        && !drops.is_empty()
    {
        s.push_str("## Dropped Pipelines (Max / Frame)\n");
        for (k, v) in drops {
            s.push_str(&format!("- `{v}` / frame: `{k}`\n"));
        }
    }
    s
}

fn compare_cmd(a_path: &Path, b_path: &Path) -> Result<()> {
    let a: Value = serde_json::from_str(&std::fs::read_to_string(a_path)?)
        .with_context(|| format!("reading {}", a_path.display()))?;
    let b: Value = serde_json::from_str(&std::fs::read_to_string(b_path)?)
        .with_context(|| format!("reading {}", b_path.display()))?;
    println!("{}", render_compare(&a, &b));
    Ok(())
}

pub fn render_compare(a: &Value, b: &Value) -> String {
    let mut out = String::from("# VIctory Harness A/B Comparison\n\n");
    out.push_str("| Metric | Run A | Run B | Delta (B - A) |\n|---|---:|---:|---:|\n");
    let rows = [
        ("3D p50 FPS", &a["scene_3d"]["fps"]["p50"], &b["scene_3d"]["fps"]["p50"]),
        ("3D p50 frame (ms)", &a["scene_3d"]["frame_ms"]["p50"], &b["scene_3d"]["frame_ms"]["p50"]),
        ("3D p95 frame (ms)", &a["scene_3d"]["frame_ms"]["p95"], &b["scene_3d"]["frame_ms"]["p95"]),
        ("3D GPU p50 (ms)", &a["scene_3d"]["gpu_ms"]["p50"], &b["scene_3d"]["gpu_ms"]["p50"]),
        ("CPU recording p50 (ms)", &a["sync_and_cpu"]["rec_cpu_ms"]["p50"], &b["sync_and_cpu"]["rec_cpu_ms"]["p50"]),
        ("Settle wait p50 (ms)", &a["sync_and_cpu"]["settle_wait_ms"]["p50"], &b["sync_and_cpu"]["settle_wait_ms"]["p50"]),
        ("Peak VRAM (MB)", &a["memory"]["vram_peak_mb"], &b["memory"]["vram_peak_mb"]),
        ("Peak PC Shared Spill (MB)", &a["memory"]["pc_shared_peak_mb"], &b["memory"]["pc_shared_peak_mb"]),
        ("Dropped pipelines p50", &a["scene_3d"]["dropped_pipeline_per_frame"]["p50"], &b["scene_3d"]["dropped_pipeline_per_frame"]["p50"]),
        ("Null buffer views max", &a["scene_3d"]["max_null_buffer_views"], &b["scene_3d"]["max_null_buffer_views"]),
    ];
    for (name, va, vb) in rows {
        let fa = va.as_f64().unwrap_or(0.0);
        let fb = vb.as_f64().unwrap_or(0.0);
        let delta = fb - fa;
        out.push_str(&format!("| {name} | {fa:.1} | {fb:.1} | {delta:+.1} |\n"));
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn prepare_cmd(
    root: &Path,
    profile: Profile,
    out: &Path,
    game_exe: &Path,
    config: &Path,
    binary: Option<&Path>,
    capture: bool,
    bvh: bool,
    bvh_quarantine: bool,
    skip_pipelines: Option<&str>,
    extra: &[String],
) -> Result<()> {
    ensure_outside_repo(root, out)?;
    let bin = binary
        .map(Path::to_path_buf)
        .unwrap_or_else(|| root.join("target").join("diag").join("victory.exe"));
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let run_dir = out.join(format!("{}_{}", profile.slug(), ts));
    std::fs::create_dir_all(&run_dir)?;

    let mut env_vars = vec![
        ("VICTORY_CONFIG".to_owned(), config.display().to_string()),
        ("VICTORY_DISPLAY_MODE".to_owned(), "1920x1080".to_owned()),
        ("VICTORY_RELAY".to_owned(), "1".to_owned()),
        ("VICTORY_WIDE_BACK".to_owned(), "1".to_owned()),
        ("VICTORY_WIDE_REFRESH".to_owned(), "1".to_owned()),
        ("VICTORY_NO_WATCHDOG".to_owned(), "1".to_owned()),
        ("VICTORY_LOG".to_owned(), "info".to_owned()),
    ];
    match profile {
        Profile::Baseline1080p => {}
        Profile::Tuned8gb | Profile::Trim8gb => {
            env_vars.push(("VICTORY_QUEUE_COPIES".to_owned(), "1".to_owned()));
            env_vars.push(("VICTORY_RELAY_COPY_QUEUE".to_owned(), "1".to_owned()));
            env_vars.push(("VICTORY_CARD_BUFFERS_MIN_MB".to_owned(), "1".to_owned()));
            env_vars.push(("VICTORY_CARD_REHASH_RUNS".to_owned(), "1".to_owned()));
            env_vars.push(("VICTORY_JOIN_COPIES".to_owned(), "1".to_owned()));
            env_vars.push(("VICTORY_TEXTURE_MIP_DROP_MIN_SIZE".to_owned(), "1024".to_owned()));
            env_vars.push(("VICTORY_TEXTURE_ROOM_PERCENT".to_owned(), "94".to_owned()));
            if matches!(profile, Profile::Trim8gb) {
                env_vars.push(("VICTORY_WINDOW_KB".to_owned(), "1024".to_owned()));
                env_vars.push(("VICTORY_WINDOW_TAILS".to_owned(), "1".to_owned()));
                env_vars.push(("VICTORY_INDIRECT_WINDOW".to_owned(), "1".to_owned()));
            }
        }
        Profile::BvhTest => {
            env_vars.push(("VICTORY_QUEUE_COPIES".to_owned(), "1".to_owned()));
            env_vars.push(("VICTORY_RELAY_COPY_QUEUE".to_owned(), "1".to_owned()));
            env_vars.push(("VICTORY_CARD_BUFFERS_MIN_MB".to_owned(), "1".to_owned()));
            env_vars.push(("VICTORY_TEXTURE_MIP_DROP_MIN_SIZE".to_owned(), "1024".to_owned()));
            env_vars.push(("VICTORY_TEXTURE_ROOM_PERCENT".to_owned(), "94".to_owned()));
            env_vars.push(("VICTORY_BVH_BUILDER".to_owned(), "1".to_owned()));
            env_vars.push(("VICTORY_BVH_QUARANTINE".to_owned(), "1".to_owned()));
        }
        Profile::NullMagenta => {
            env_vars.push(("VICTORY_NULL_MAGENTA".to_owned(), "1".to_owned()));
        }
        Profile::DredDebug => {
            env_vars.push(("VICTORY_D3D12_DRED".to_owned(), "1".to_owned()));
            env_vars.push(("VICTORY_D3D12_DEBUG".to_owned(), "1".to_owned()));
        }
        Profile::ShaderBisect => {}
    }
    if capture {
        let frames_dir = run_dir.join("frames");
        std::fs::create_dir_all(&frames_dir)?;
        env_vars.push(("VICTORY_CAPTURE".to_owned(), frames_dir.display().to_string()));
    }
    if bvh {
        env_vars.push(("VICTORY_BVH_BUILDER".to_owned(), "1".to_owned()));
    }
    if bvh_quarantine {
        env_vars.push(("VICTORY_BVH_QUARANTINE".to_owned(), "1".to_owned()));
    }
    if let Some(skips) = skip_pipelines {
        env_vars.push(("VICTORY_SKIP_PIPELINES".to_owned(), skips.to_owned()));
    }
    for kv in extra {
        let Some((k, v)) = kv.split_once('=') else {
            bail!("--extra must be KEY=VALUE, got `{kv}`");
        };
        env_vars.push((k.trim().to_owned(), v.trim().to_owned()));
    }

    let git_head = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(root)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|| "unknown".to_owned());

    let meta = json!({
        "profile": profile.slug(),
        "git_head": git_head,
        "binary": bin.display().to_string(),
        "game_exe": game_exe.display().to_string(),
        "env": env_vars.iter().cloned().collect::<BTreeMap<String, String>>(),
    });
    std::fs::write(run_dir.join("run_meta.json"), serde_json::to_string_pretty(&meta)?)?;

    let log_path = run_dir.join("run.log");
    let mut ps1 = String::from("$ErrorActionPreference = \"Stop\"\n");
    for (k, v) in &env_vars {
        let escaped = v.replace('\'', "''");
        ps1.push_str(&format!("$env:{k} = '{escaped}'\n"));
    }
    ps1.push_str(&format!(
        "Write-Host \"Launching {} (logging to {})...\"\n",
        profile.slug(),
        log_path.display()
    ));
    // PCIe traffic beside the run (`rxpci` near 10-13 GB/s on PCIe 3 x16
    // means the card is waiting on memory spilled to the PC), when the
    // NVIDIA tool is present; stopped when the title exits.
    let pcie_path = run_dir.join("pcie.log");
    ps1.push_str(&format!(
        "$dmon = $null\n\
         if (Get-Command nvidia-smi -ErrorAction SilentlyContinue) {{\n\
         \x20   $dmon = Start-Process nvidia-smi -ArgumentList 'dmon','-s','t','-d','1' -RedirectStandardOutput \"{}\" -WindowStyle Hidden -PassThru\n\
         }}\n",
        pcie_path.display()
    ));
    ps1.push_str("try {\n");
    ps1.push_str(&format!(
        "    & \"{}\" run \"{}\" --execute 2>&1 | Tee-Object -FilePath \"{}\"\n",
        bin.display(),
        game_exe.display(),
        log_path.display()
    ));
    ps1.push_str("} finally {\n    if ($dmon) { Stop-Process -Id $dmon.Id -ErrorAction SilentlyContinue }\n}\n");
    let ps1_path = run_dir.join("run.ps1");
    std::fs::write(&ps1_path, ps1)?;

    let cmd_path = run_dir.join("launch.cmd");
    let cmd_body = format!(
        "@echo off\r\nstart \"\" pwsh -NoExit -File \"{}\"\r\n",
        ps1_path.display()
    );
    std::fs::write(&cmd_path, cmd_body)?;

    println!("Prepared harness run: {}", run_dir.display());
    println!("  launcher : {}", cmd_path.display());
    println!("  log file : {}", log_path.display());
    Ok(())
}

fn shader_cmd(
    root: &Path,
    hash: &str,
    config: &Path,
    binary: Option<&Path>,
    paths: &[PathBuf],
) -> Result<()> {
    let bin = binary
        .map(Path::to_path_buf)
        .unwrap_or_else(|| root.join("target").join("diag").join("victory.exe"));
    if !bin.is_file() {
        bail!("victory binary not found at {}", bin.display());
    }
    let mut cmd = Command::new(&bin);
    cmd.arg("translate")
        .arg("--validate")
        .arg("--pipelines")
        .arg("--list");
    for p in paths {
        cmd.arg(p);
    }
    cmd.env("VICTORY_CONFIG", config);
    cmd.env("VICTORY_TRANSLATE_ONLY", hash);
    let status = cmd.status().with_context(|| format!("running {}", bin.display()))?;
    if !status.success() {
        bail!("victory translate exited with {status}");
    }
    Ok(())
}

fn slice_between<'a>(haystack: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let i = haystack.find(start)? + start.len();
    let rest = &haystack[i..];
    let j = rest.find(end)?;
    Some(&rest[..j])
}

fn num_after<'a>(haystack: &'a str, marker: &str) -> Option<&'a str> {
    let i = haystack.find(marker)? + marker.len();
    Some(&haystack[i..])
}

fn num_before(haystack: &str, marker: &str) -> Option<f64> {
    let i = haystack.find(marker)?;
    let prefix = haystack[..i].trim_end();
    let token = prefix
        .rsplit(|c: char| !(c.is_ascii_digit() || c == '.'))
        .next()?;
    token.parse::<f64>().ok()
}

fn num_between(haystack: &str, start: &str, end: &str) -> Option<f64> {
    slice_between(haystack, start, end).and_then(parse_first_f64)
}

fn parse_first_f64(s: &str) -> Option<f64> {
    let s = s.trim_start();
    let end = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    if end == 0 {
        return None;
    }
    s[..end].parse::<f64>().ok()
}

fn parse_trailing_number(s: &str) -> Option<(&str, f64)> {
    let s = s.trim();
    let idx = s.rfind(' ')?;
    let val = s[idx + 1..].trim().parse::<f64>().ok()?;
    Some((s[..idx].trim(), val))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi_sequences() {
        let raw = "\x1b[2m2026-11-25\x1b[0m \x1b[32mINFO\x1b[0m ok";
        assert_eq!(strip_ansi(raw), "2026-11-25 INFO ok");
    }

    #[test]
    fn parses_waterfall_vram_and_settles() {
        let sample = "\
\x1b[32mINFO\x1b[0m victory_gpu::host: 1.7 fps, 596.3 ms/frame (110.4 preparing: copies 70.8 ms 125.3 MB, textures 3.2 ms 7.6 MB; 469.9 on the GPU; 64.6 copying back 64.5 MB), 5102 draws/frame, dropped 324 without a host pipeline (0 being made) and 1 indirect; heap buffers 1.0 passes 8.7 ms; buffer views left null as the host would refuse them: 2533;
\x1b[32mINFO\x1b[0m victory_gpu::copystat: recording per frame (16 frames): 45320 list calls 378.7 ms (all recording threads); 0.0 ms waiting for pipelines; guards 35.7 ms;
\x1b[32mINFO\x1b[0m victory_gpu::copystat: the card's memory by kind: 7360 MB in use of a budget of 7052 MB: render targets, depth and unordered access textures 3488 MB (twins 228 MB), textures the title fills 1970 MB, title buffers 936 MB (and 0 MB before-twins), windows 0 MB, the rest (lists, views, scratch, the swap chain, the driver) 965 MB; the PC's memory the card reads: 5632 MB in use, of it windows 3672 MB and title buffers' copies 985 MB
\x1b[32mINFO\x1b[0m victory_gpu::host: submissions sent ahead per frame: 29.0 in 2.0 settles (0.0 flights each time), 482.0 ms waiting for the card in them; settled for: a synchronous submission or present 0.0, title memory changed under shader writes 1.0, title memory read 1.0;
\x1b[32mINFO\x1b[0m victory_gpu::host: dropped 363.0/frame without a host pipeline: a stage of the ray-tracing structure builder, which is off
";
        let parsed = parse_log(BufReader::new(sample.as_bytes())).unwrap();
        let report = build_report(Path::new("synthetic.log"), &parsed);
        assert_eq!(report["scene_3d"]["fps"]["p50"], 1.7);
        assert_eq!(report["scene_3d"]["frame_ms"]["p50"], 596.3);
        assert_eq!(report["scene_3d"]["gpu_ms"]["p50"], 469.9);
        assert_eq!(report["memory"]["vram_peak_mb"], 7360.0);
        assert_eq!(report["memory"]["vram_budget_mb"], 7052.0);
        assert_eq!(report["memory"]["pc_shared_peak_mb"], 5632.0);
        assert_eq!(report["sync_and_cpu"]["settle_wait_ms"]["p50"], 482.0);
        assert_eq!(report["scene_3d"]["max_null_buffer_views"], 2533);
        let diags = report["diagnoses"].as_array().unwrap();
        assert!(diags.iter().any(|d| d.as_str().unwrap().contains("VRAM_OVER_BUDGET")));
        assert!(diags.iter().any(|d| d.as_str().unwrap().contains("BVH_BUILDER_OFF")));
    }

    #[test]
    fn compares_two_reports() {
        let a = json!({
            "scene_3d": {
                "fps": {"p50": 2.0},
                "frame_ms": {"p50": 500.0, "p95": 700.0},
                "gpu_ms": {"p50": 400.0},
                "dropped_pipeline_per_frame": {"p50": 350.0},
                "max_null_buffer_views": 2000
            },
            "sync_and_cpu": {"rec_cpu_ms": {"p50": 300.0}, "settle_wait_ms": {"p50": 400.0}},
            "memory": {"vram_peak_mb": 7360.0, "pc_shared_peak_mb": 5600.0}
        });
        let b = json!({
            "scene_3d": {
                "fps": {"p50": 10.0},
                "frame_ms": {"p50": 100.0, "p95": 140.0},
                "gpu_ms": {"p50": 80.0},
                "dropped_pipeline_per_frame": {"p50": 0.0},
                "max_null_buffer_views": 0
            },
            "sync_and_cpu": {"rec_cpu_ms": {"p50": 20.0}, "settle_wait_ms": {"p50": 5.0}},
            "memory": {"vram_peak_mb": 5200.0, "pc_shared_peak_mb": 400.0}
        });
        let md = render_compare(&a, &b);
        assert!(md.contains("| 3D p50 FPS | 2.0 | 10.0 | +8.0 |"));
        assert!(md.contains("| Peak VRAM (MB) | 7360.0 | 5200.0 | -2160.0 |"));
    }

    #[test]
    fn refuses_output_inside_repo() {
        let root = crate::workspace_root();
        assert!(ensure_outside_repo(&root, &root.join("harness_out")).is_err());
        assert!(ensure_outside_repo(&root, &std::env::temp_dir().join("victory_unrelated_tmp")).is_ok());
    }
}
