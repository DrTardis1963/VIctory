//! Golden DXIL: a fixed set of synthetic shaders, written with the test
//! assembler, lowered and disassembled with the Windows SDK's
//! `dxc.exe -dumpbin`, compared with the text checked in under
//! `crates/victory-shader/tests/snapshots/`.
//!
//! A change to the lowering that alters its output shows as a diff here, so
//! a reviewer sees what the DXIL now says rather than only that a test
//! still passes. When the change is meant, rewrite the files:
//! `cargo xtask snapshots --bless` (or `VICTORY_BLESS=1` on the test run).
//!
//! Skipped, with a note, without `dxc.exe` or `dxil.dll`.

use std::path::{Path, PathBuf};

use super::synth::{bytes, compute, pixel, translate, validate, validator};
use crate::gfx10::asm::asm;
use crate::native::NativeProgram;

/// Loads the descriptor of the raw UAV at table entry 0 into `s[8:11]`
/// (the table address arrives in `s4`), as every compute case does.
const PROLOGUE: &str = "
    s_mov_b32 s5, 0
    s_load_dwordx4 s[8:11], s[4:5], null
    s_waitcnt lgkmcnt(0)
";

/// (snapshot name, program, compute lanes and LDS bytes; `None` for pixel).
const CASES: &[(&str, &str, Option<(u32, u32)>)] = &[
    (
        "valu_arithmetic",
        "
        v_mov_b32 v1, s0
        v_cvt_f32_u32 v2, s1
        v_add_f32 v3, v1, v2
        v_mul_f32 v3, 0.5, v3
        v_fma_f32 v4, v3, s2, -v1 clamp
        v_med3_f32 v4, v4, 0, 1.0
        v_mul_lo_u32 v5, v1, s3
        v_xor_b32 v5, lit(0x80000000), v5
        v_cmp_gt_f32 vcc_lo, v4, v3
        v_cndmask_b32 v0, v4, v5, vcc_lo
        buffer_store_dword v0, v0, s[8:11], 0
        s_endpgm
        ",
        Some((1, 0)),
    ),
    (
        "branchy",
        "
        v_mov_b32 v1, s0
        v_mov_b32 v2, 0
        v_cmp_gt_u32 vcc_lo, v0, v1
        s_and_saveexec_b32 s20, vcc_lo
        s_cbranch_execz merge
        v_add_nc_u32 v2, v0, v1
        merge:
        s_or_b32 exec_lo, exec_lo, s20
        s_mov_b32 s21, 4
        again:
        v_lshlrev_b32 v2, 1, v2
        s_sub_u32 s21, s21, 1
        s_cmp_lg_u32 s21, 0
        s_cbranch_scc1 again
        v_lshlrev_b32 v3, 2, v0
        buffer_store_dword v2, v3, s[8:11], 0 offen
        s_endpgm
        ",
        Some((32, 0)),
    ),
    (
        "lds",
        "
        v_lshlrev_b32 v1, 2, v0
        v_mov_b32 v2, s0
        ds_write_b32 v1, v2
        s_waitcnt lgkmcnt(0)
        s_barrier
        v_xor_b32 v3, 4, v1
        ds_read_b32 v4, v3 offset:0
        ds_add_u32 v1, v4 offset:128
        s_waitcnt lgkmcnt(0)
        buffer_store_dword v4, v1, s[8:11], 0 offen
        s_endpgm
        ",
        Some((32, 512)),
    ),
    (
        "buffer_load_store",
        "
        v_lshlrev_b32 v1, 2, v0
        buffer_load_dword v2, v1, s[8:11], 0 offen
        buffer_load_dwordx2 v[4:5], v1, s[8:11], 0 offen offset:64
        s_waitcnt vmcnt(0)
        v_add_nc_u32 v2, v2, v4
        v_add_nc_u32 v2, v2, v5
        buffer_store_dword v2, v1, s[8:11], 0 offen offset:256
        buffer_atomic_add v2, v1, s[8:11], 0 offen
        s_endpgm
        ",
        Some((32, 0)),
    ),
    (
        "pixel_export",
        "
        v_mov_b32 v0, 1.0
        v_mov_b32 v1, 0.5
        v_mul_f32 v1, v1, v1
        v_mov_b32 v2, 0
        v_mov_b32 v3, 1.0
        exp mrt0, v0, v1, v2, v3 done vm
        s_endpgm
        ",
        None,
    ),
];

fn native(text: &str, compute_args: Option<(u32, u32)>) -> NativeProgram {
    match compute_args {
        Some((threads, lds)) => compute(bytes(&asm(&format!("{PROLOGUE}\n{text}"))), threads, lds),
        None => pixel(bytes(&asm(text))),
    }
}

/// `dxc.exe`: `VICTORY_PC_DXC`, else beside the newest SDK's `dxil.dll`.
fn dxc() -> Option<PathBuf> {
    let dxc = match std::env::var_os("VICTORY_PC_DXC") {
        Some(p) => PathBuf::from(p),
        None => super::validator::Validator::default_path()?.with_file_name("dxc.exe"),
    };
    dxc.exists().then_some(dxc)
}

/// `dxc -dumpbin` of `container`, through a file in the temp folder (a
/// unique name: the cases run on parallel test threads).
fn dumpbin(dxc: &Path, name: &str, container: &[u8]) -> String {
    let file = std::env::temp_dir().join(format!(
        "victory-snapshot-{}-{name}.dxil",
        std::process::id()
    ));
    std::fs::write(&file, container).expect("writing the container to the temp folder");
    let out = std::process::Command::new(dxc)
        .arg("-dumpbin")
        .arg(&file)
        .output()
        .expect("running dxc");
    let _ = std::fs::remove_file(&file);
    assert!(
        out.status.success(),
        "dxc -dumpbin failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The dump without what changes from machine to machine or SDK to SDK:
/// hashes, file paths, compiler banners; and with `\n` line ends.
fn normalise(dump: &str) -> String {
    let mut out = String::new();
    for line in dump.lines() {
        let line = line.trim_end();
        let lower = line.to_ascii_lowercase();
        let unstable = lower.contains("hash")
            || lower.contains("dxc")
            || lower.contains(":\\")
            || lower.contains("version")
            || lower.starts_with("; note:");
        if unstable {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn snapshot_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots")
        .join(format!("{name}.txt"))
}

/// A unified diff of two texts' lines, at most `limit` lines of it.
fn diff(old: &str, new: &str, limit: usize) -> String {
    let (a, b): (Vec<&str>, Vec<&str>) = (old.lines().collect(), new.lines().collect());
    // Longest common subsequence table, from the ends.
    let mut lcs = vec![vec![0u32; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    // (kind, line): ' ' both, '-' old only, '+' new only.
    let mut ops = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            ops.push((' ', a[i]));
            (i, j) = (i + 1, j + 1);
        } else if i < a.len() && (j == b.len() || lcs[i + 1][j] >= lcs[i][j + 1]) {
            ops.push(('-', a[i]));
            i += 1;
        } else {
            ops.push(('+', b[j]));
            j += 1;
        }
    }
    // Changed lines with two lines of context around them.
    let near = |k: usize| {
        ops[k.saturating_sub(2)..(k + 3).min(ops.len())]
            .iter()
            .any(|(kind, _)| *kind != ' ')
    };
    let mut out = String::from("--- snapshot\n+++ lowered now\n");
    let mut shown = 0;
    let mut gap = false;
    for (k, (kind, line)) in ops.iter().enumerate() {
        if !near(k) {
            gap = true;
            continue;
        }
        if gap {
            out.push_str("@@\n");
            gap = false;
        }
        if shown == limit {
            out.push_str("... (diff cut short)\n");
            break;
        }
        out.push(*kind);
        out.push_str(line);
        out.push('\n');
        shown += 1;
    }
    out
}

#[test]
fn lowered_synthetic_shaders_match_their_dxil_snapshots() {
    let (Some(dxc), Some(mut v)) = (dxc(), validator()) else {
        eprintln!("no dxc.exe or dxil.dll; DXIL snapshots skipped");
        return;
    };
    let bless = std::env::var_os("VICTORY_BLESS").is_some_and(|b| b == "1");
    let mut failures = Vec::new();
    for &(name, text, compute_args) in CASES {
        let shader = translate(&native(text, compute_args))
            .unwrap_or_else(|e| panic!("{name}: lowering failed: {e}"));
        validate(&mut v, &shader.dxil).unwrap_or_else(|e| panic!("{name}: invalid DXIL: {e}"));
        let now = normalise(&dumpbin(&dxc, name, &shader.dxil));
        let path = snapshot_path(name);
        if bless {
            std::fs::create_dir_all(path.parent().expect("a folder")).expect("creating snapshots/");
            std::fs::write(&path, &now).expect("writing the snapshot");
            eprintln!("blessed {}", path.display());
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(old) if old.replace("\r\n", "\n") == now => {}
            Ok(old) => failures.push(format!("{name}:\n{}", diff(&old, &now, 40))),
            Err(_) => failures.push(format!("{name}: no snapshot at {}", path.display())),
        }
    }
    assert!(
        failures.is_empty(),
        "DXIL differs from the snapshots (rewrite them with `cargo xtask snapshots --bless` if meant):\n\n{}",
        failures.join("\n")
    );
}

#[test]
fn the_diff_shows_changed_lines_with_context() {
    let d = diff("a\nb\nc\nd\ne\nf\ng\n", "a\nb\nc\nD\ne\nf\ng\n", 40);
    assert_eq!(
        d,
        "--- snapshot\n+++ lowered now\n@@\n b\n c\n-d\n+D\n e\n f\n"
    );
}
