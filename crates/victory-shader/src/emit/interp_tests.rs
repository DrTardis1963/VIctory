//! Differential tests: synthetic GFX10 compute programs run on the reference
//! interpreter ([`super::interp`]) and, translated to DXIL, on the host GPU
//! and WARP, with the output buffers compared word by word.
//!
//! The DXIL validator only says a translation is well formed; these tests
//! say it computes what the console would. Every program gets the same
//! frame: root constants in s0-s3, a raw input buffer (random words from a
//! fixed seed) and a raw output buffer through the descriptor table at s4,
//! the thread index in v0 and its byte offset in v1.
//!
//! - `translated_programs_match_the_reference_interpreter` runs the corpus.
//! - `random_programs_match_the_reference_interpreter` runs
//!   `VICTORY_DIFF_CASES` (default 0) generated straight-line programs
//!   from `VICTORY_DIFF_SEED` (default 1); `cargo xtask diff-test` sets both.
//! - `refused_programs_stay_refused_or_match` holds the programs the
//!   translation refuses today, so that they are checked once it takes them.
//! - Cases the translation gets wrong stay as `#[ignore]` tests below the
//!   corpus, each with what it shows (`--include-ignored` runs them).
//!
//! `VICTORY_DIFF_ONLY=<case name>` runs one corpus case. Skipped without an
//! SM 6.6 device.

use std::collections::HashMap;
use std::sync::OnceLock;

use super::host::{Dispatch, Host, Root, Stride, View};
use super::interp::{self, Group};
use super::{HostShader, lower, validator};
use crate::gfx10::{self, Encoding};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

// ---------------------------------------------------------------------------
// Encoding.

/// Opcode numbers by encoding and name, found by decoding every opcode of
/// each encoding (so the test needs no second opcode table).
fn opcodes() -> &'static HashMap<(Encoding, &'static str), u16> {
    static TABLE: OnceLock<HashMap<(Encoding, &'static str), u16>> = OnceLock::new();
    TABLE.get_or_init(|| {
        const NOP: u32 = 0xBF80_0000;
        const END: u32 = 0xBF81_0000;
        let forms: [(Encoding, u16, fn(u32) -> [u32; 2]); 12] = [
            (Encoding::Sop2, 0x80, |op| [0x8000_0000 | op << 23, NOP]),
            (Encoding::Sopk, 0x20, |op| [0xB000_0000 | op << 23, NOP]),
            (Encoding::Sop1, 0x100, |op| [0xBE80_0000 | op << 8, NOP]),
            (Encoding::Sopc, 0x80, |op| [0xBF00_0000 | op << 16, NOP]),
            (Encoding::Sopp, 0x80, |op| [0xBF80_0000 | op << 16, NOP]),
            (Encoding::Smem, 0x100, |op| {
                [0xF400_0000 | op << 18, 0xFA00_0000]
            }),
            (Encoding::Vop1, 0x100, |op| [0x7E00_0000 | op << 9, NOP]),
            (Encoding::Vop2, 0x3E, |op| [op << 25, NOP]),
            (Encoding::Vopc, 0x100, |op| [0x7C00_0000 | op << 17, NOP]),
            (Encoding::Vop3, 0x400, |op| [0xD400_0000 | op << 16, 0]),
            (Encoding::Ds, 0x100, |op| [0xD800_0000 | op << 18, 0]),
            (Encoding::Mubuf, 0x80, |op| {
                [0xE000_0000 | op << 18, 0x8000_0000]
            }),
        ];
        let mut table = HashMap::new();
        for (enc, count, make) in forms {
            for op in 0..count {
                let [w0, w1] = make(u32::from(op));
                let code: Vec<u8> = [w0, w1, NOP, NOP, END]
                    .iter()
                    .flat_map(|w| w.to_le_bytes())
                    .collect();
                let Ok(list) = gfx10::decode(&code) else {
                    continue;
                };
                if let Some(first) = list.first()
                    && first.encoding == enc
                    && let Some(name) = first.name()
                {
                    table.entry((enc, name)).or_insert(op);
                }
            }
        }
        table
    })
}

fn opcode(enc: Encoding, name: &str) -> u32 {
    match opcodes().get(&(enc, name)) {
        Some(&op) => u32::from(op),
        None => panic!("no {enc:?} opcode {name}"),
    }
}

/// A source operand: its 9-bit code and, for code 255, the literal.
#[derive(Clone, Copy, Debug)]
struct Src(u16, Option<u32>);

const fn s(n: u8) -> Src {
    Src(n as u16, None)
}

const fn v(n: u8) -> Src {
    Src(256 + n as u16, None)
}

const VCC: Src = Src(106, None);
const EXEC_LO: Src = Src(126, None);

/// An integer: inline when -16..=64, else a literal.
fn c(x: i32) -> Src {
    match x {
        0..=64 => Src(128 + x as u16, None),
        -16..=-1 => Src((192 - x) as u16, None),
        _ => lit(x as u32),
    }
}

fn lit(x: u32) -> Src {
    Src(255, Some(x))
}

/// A float: inline when one of the nine inline values, else a literal.
fn fk(x: f32) -> Src {
    const INLINE: [f32; 8] = [0.5, -0.5, 1.0, -1.0, 2.0, -2.0, 4.0, -4.0];
    match INLINE.iter().position(|&y| y.to_bits() == x.to_bits()) {
        Some(i) => Src(240 + i as u16, None),
        None if x.to_bits() == 0 => Src(128, None),
        None => lit(x.to_bits()),
    }
}

/// Byte offsets of the input's rows (64 dwords each): random words, normal
/// floats, small integers (0-63), and floats in [-300, 300] (half of them
/// on quarter steps: turns, roundings).
const INT: u16 = 0;
const FLT: u16 = 1024;
const SMALL: u16 = 2048;
const ANG: u16 = 3072;
const ROW: u16 = 256;
/// The input buffer's size in dwords (16 rows).
const DWORDS: usize = 1024;
/// The output buffer's (32 rows).
const OUT_DWORDS: usize = 2048;
/// A VGPR the helpers use as scratch.
const TMP: u8 = 60;
/// The thread's byte offset in output rows 16-31 (v1 + 4096).
const HIGH: u8 = 61;

#[derive(Default)]
struct Asm {
    w: Vec<u32>,
}

impl Asm {
    fn literal(&mut self, srcs: &[Src]) {
        let lits: Vec<u32> = srcs.iter().filter_map(|s| s.1).collect();
        assert!(
            lits.windows(2).all(|w| w[0] == w[1]),
            "one literal per instruction: {lits:x?}"
        );
        if let Some(&l) = lits.first() {
            self.w.push(l);
        }
    }

    fn sop2(&mut self, name: &str, d: u8, a: Src, b: Src) {
        let op = opcode(Encoding::Sop2, name);
        self.w.push(
            0x8000_0000
                | op << 23
                | u32::from(d) << 16
                | u32::from(b.0 & 0xFF) << 8
                | u32::from(a.0 & 0xFF),
        );
        self.literal(&[a, b]);
    }

    fn sop1(&mut self, name: &str, d: u8, a: Src) {
        let op = opcode(Encoding::Sop1, name);
        self.w
            .push(0xBE80_0000 | u32::from(d) << 16 | op << 8 | u32::from(a.0 & 0xFF));
        self.literal(&[a]);
    }

    fn sopc(&mut self, name: &str, a: Src, b: Src) {
        let op = opcode(Encoding::Sopc, name);
        self.w
            .push(0xBF00_0000 | op << 16 | u32::from(b.0 & 0xFF) << 8 | u32::from(a.0 & 0xFF));
        self.literal(&[a, b]);
    }

    fn sopk(&mut self, name: &str, d: u8, imm: u16) {
        let op = opcode(Encoding::Sopk, name);
        self.w
            .push(0xB000_0000 | op << 23 | u32::from(d) << 16 | u32::from(imm));
    }

    fn sopp(&mut self, name: &str, imm: u16) {
        let op = opcode(Encoding::Sopp, name);
        self.w.push(0xBF80_0000 | op << 16 | u32::from(imm));
    }

    /// `s_load_dword*`/`s_buffer_load_dword*` `sdata, s[base..], offset`.
    fn smem(&mut self, name: &str, sdata: u8, sbase: u8, offset: u32) {
        let op = opcode(Encoding::Smem, name);
        self.w
            .push(0xF400_0000 | op << 18 | u32::from(sdata) << 6 | u32::from(sbase >> 1));
        self.w.push(125 << 25 | offset & 0x1F_FFFF);
    }

    fn vop1(&mut self, name: &str, d: u8, a: Src) {
        let op = opcode(Encoding::Vop1, name);
        self.w
            .push(0x7E00_0000 | u32::from(d) << 17 | op << 9 | u32::from(a.0));
        self.literal(&[a]);
    }

    fn vop2(&mut self, name: &str, d: u8, a: Src, b: u8) {
        let op = opcode(Encoding::Vop2, name);
        self.w
            .push(op << 25 | u32::from(d) << 17 | u32::from(b) << 9 | u32::from(a.0));
        self.literal(&[a]);
    }

    /// `v_madmk`/`v_madak`/`v_fmamk`/`v_fmaak` with the constant `k`.
    fn vop2k(&mut self, name: &str, d: u8, a: Src, b: u8, k: f32) {
        assert!(a.1.is_none(), "one literal per instruction");
        let op = opcode(Encoding::Vop2, name);
        self.w
            .push(op << 25 | u32::from(d) << 17 | u32::from(b) << 9 | u32::from(a.0));
        self.w.push(k.to_bits());
    }

    fn vopc(&mut self, name: &str, a: Src, b: u8) {
        let op = opcode(Encoding::Vopc, name);
        self.w
            .push(0x7C00_0000 | op << 17 | u32::from(b) << 9 | u32::from(a.0));
        self.literal(&[a]);
    }

    fn vop3(&mut self, name: &str, d: u8, srcs: &[Src]) {
        self.vop3m(name, d, srcs, 0, 0, false, 0);
    }

    /// VOP3 with `abs`, `neg` (bit i: source i), `clamp` and `omod`.
    #[allow(clippy::too_many_arguments)]
    fn vop3m(&mut self, name: &str, d: u8, srcs: &[Src], abs: u8, neg: u8, clamp: bool, omod: u8) {
        let op = opcode(Encoding::Vop3, name);
        let f = |i: usize| u32::from(srcs.get(i).map_or(0, |s| s.0));
        self.w.push(
            0xD400_0000 | op << 16 | u32::from(clamp) << 15 | u32::from(abs) << 8 | u32::from(d),
        );
        self.w
            .push(u32::from(neg) << 29 | u32::from(omod) << 27 | f(2) << 18 | f(1) << 9 | f(0));
        self.literal(srcs);
    }

    /// VOP3B: a VGPR result and a lane-mask carry in `sdst`.
    fn vop3b(&mut self, name: &str, d: u8, sdst: u8, srcs: &[Src]) {
        let op = opcode(Encoding::Vop3, name);
        let f = |i: usize| u32::from(srcs.get(i).map_or(0, |s| s.0));
        self.w
            .push(0xD400_0000 | op << 16 | u32::from(sdst) << 8 | u32::from(d));
        self.w.push(f(2) << 18 | f(1) << 9 | f(0));
        self.literal(srcs);
    }

    #[allow(clippy::too_many_arguments)]
    fn ds(&mut self, name: &str, vdst: u8, addr: u8, d0: u8, d1: u8, off0: u8, off1: u8) {
        let op = opcode(Encoding::Ds, name);
        self.w
            .push(0xD800_0000 | op << 18 | u32::from(off1) << 8 | u32::from(off0));
        self.w.push(
            u32::from(vdst) << 24 | u32::from(d1) << 16 | u32::from(d0) << 8 | u32::from(addr),
        );
    }

    /// MUBUF `name vdata, vaddr, s[srsrc:srsrc+3], soffset offen offset:offset`.
    fn mubuf(&mut self, name: &str, vdata: u8, vaddr: u8, srsrc: u8, soffset: Src, offset: u16) {
        assert!(offset < 4096 && soffset.1.is_none());
        let op = opcode(Encoding::Mubuf, name);
        self.w
            .push(0xE000_0000 | op << 18 | 1 << 12 | u32::from(offset));
        self.w.push(
            u32::from(soffset.0 & 0xFF) << 24
                | u32::from(srsrc >> 2) << 16
                | u32::from(vdata) << 8
                | u32::from(vaddr),
        );
    }

    /// `vd` = this thread's dword at byte `off` of the input row group.
    fn ld(&mut self, vd: u8, off: u16) {
        self.mubuf("buffer_load_dword", vd, 1, 8, c(0), off);
    }

    /// This thread's dword of output row `row` = `vs`.
    fn st(&mut self, vs: u8, row: u16) {
        let (vaddr, row) = if row < 16 { (1, row) } else { (HIGH, row - 16) };
        self.mubuf("buffer_store_dword", vs, vaddr, 12, c(0), row * ROW);
    }

    /// Output row `row` = SGPR `sn` (every thread).
    fn st_s(&mut self, sn: u8, row: u16) {
        self.vop1("v_mov_b32", TMP, s(sn));
        self.st(TMP, row);
    }

    /// `acc |= (mask's bit for this lane) << bit`.
    fn bit(&mut self, acc: u8, mask: Src, bit: i32) {
        self.vop3("v_cndmask_b32", TMP, &[c(0), c(1), mask]);
        self.vop3("v_lshl_or_b32", acc, &[v(TMP), c(bit), v(acc)]);
    }

    /// A branch to be bound later: the index to give [`Asm::bind`].
    fn branch(&mut self, name: &str) -> usize {
        self.sopp(name, 0);
        self.w.len() - 1
    }

    /// Points the branch at `at` here.
    fn bind(&mut self, at: usize) {
        let delta = self.w.len() as i32 - (at as i32 + 1);
        self.w[at] = self.w[at] & 0xFFFF_0000 | u32::from(delta as i16 as u16);
    }

    /// A branch back to `target` (a word index from [`Asm::here`]).
    fn branch_to(&mut self, name: &str, target: usize) {
        let delta = target as i32 - (self.w.len() as i32 + 1);
        self.sopp(name, delta as i16 as u16);
    }

    fn here(&self) -> usize {
        self.w.len()
    }
}

// ---------------------------------------------------------------------------
// The two sides.

/// How output words are compared.
#[derive(Clone, Copy, Debug)]
enum Cmp {
    /// Bit for bit.
    Exact,
    /// Bit for bit, but any NaN equals any NaN, and a denormal equals the
    /// zero of its sign (the host's denormal mode is not the console's).
    Float,
    /// [`Cmp::Float`], or at most this many units in the last place apart,
    /// or at most this far apart (transcendentals: both sides are
    /// approximations; sines and logarithms near zero are good to an
    /// absolute error only).
    Near(u32, f32),
}

fn same(cmp: Cmp, want: u32, got: u32) -> bool {
    if want == got {
        return true;
    }
    if matches!(cmp, Cmp::Exact) {
        return false;
    }
    let (x, y) = (f32::from_bits(want), f32::from_bits(got));
    let tiny = |b: u32| b & 0x7F80_0000 == 0;
    if x.is_nan() && y.is_nan() || tiny(want) && tiny(got) && want >> 31 == got >> 31 {
        return true;
    }
    let Cmp::Near(ulps, eps) = cmp else {
        return false;
    };
    let ord = |b: u32| {
        if b >> 31 != 0 {
            -i64::from(b & 0x7FFF_FFFF)
        } else {
            i64::from(b)
        }
    };
    !x.is_nan()
        && !y.is_nan()
        && ((ord(want) - ord(got)).unsigned_abs() <= u64::from(ulps) || (x - y).abs() <= eps)
}

struct Case {
    name: &'static str,
    wave64: bool,
    threads: u32,
    lds: u32,
    cmp: Cmp,
    body: fn(&mut Asm),
}

const fn case(name: &'static str, cmp: Cmp, body: fn(&mut Asm)) -> Case {
    Case {
        name,
        wave64: false,
        threads: 64,
        lds: 0,
        cmp,
        body,
    }
}

/// The program: the frame's prologue, `body`, `s_endpgm`.
fn program(body: impl FnOnce(&mut Asm)) -> Vec<u32> {
    let mut a = Asm::default();
    a.sop1("s_mov_b32", 5, c(0));
    a.smem("s_load_dwordx4", 8, 4, 0); // the input
    a.smem("s_load_dwordx4", 12, 4, 32); // the output
    a.sopp("s_waitcnt", 0);
    a.vop2("v_lshlrev_b32", 1, c(2), 0);
    a.vop2("v_add_nc_u32", HIGH, lit(4096), 1);
    body(&mut a);
    a.sopp("s_endpgm", 0);
    a.w
}

fn disassembly(code: &[u32], wave64: bool) -> String {
    let bytes: Vec<u8> = code.iter().flat_map(|w| w.to_le_bytes()).collect();
    match gfx10::decode(&bytes) {
        Ok(list) => list
            .iter()
            .map(|i| format!("  {:04x}: {}\n", i.offset, i.text(wave64)))
            .collect(),
        Err(e) => format!("  (undecodable: {e})\n"),
    }
}

/// Splitmix64: a fixed, dependency-free stream of random words.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn u32(&mut self) -> u32 {
        self.next() as u32
    }

    fn below(&mut self, n: u32) -> u32 {
        (self.next() % u64::from(n)) as u32
    }

    /// A normal float with exponent in `-e..=e`, now and then a special one.
    fn float(&mut self, e: i32) -> f32 {
        const SPECIAL: [f32; 8] = [0.0, -0.0, 1.0, -1.0, 0.5, 2.0, 3.0, -0.25];
        if self.below(16) == 0 {
            return SPECIAL[self.below(8) as usize];
        }
        let exp = (127 + self.below(2 * e as u32 + 1) as i32 - e) as u32;
        f32::from_bits(self.u32() & 0x807F_FFFF | exp << 23)
    }
}

/// The input buffer and root constants for `seed`.
fn inputs(seed: u64) -> (Vec<u32>, [u32; 4]) {
    let mut r = Rng(seed);
    let mut input = Vec::with_capacity(DWORDS);
    for i in 0..DWORDS {
        input.push(match i / 256 {
            0 => r.u32(),
            1 => r.float(20).to_bits(),
            2 => {
                if r.below(8) == 0 {
                    0
                } else {
                    r.below(64)
                }
            }
            _ => {
                if r.below(2) == 0 {
                    (r.below(2401) as f32 / 4.0 - 300.0).to_bits()
                } else {
                    (r.u32() as f32 / u32::MAX as f32 * 600.0 - 300.0).to_bits()
                }
            }
        });
    }
    let k = [r.below(32), r.u32(), r.float(10).to_bits(), r.u32()];
    (input, k)
}

/// The synthetic program as the console's compiler would hand it over: root
/// constants in s0-s3, the table (input SRV, output UAV) at s4.
fn native(code: &[u32], wave64: bool, threads: u32, lds: u32) -> NativeProgram {
    let bytes: Vec<u8> = code.iter().flat_map(|w| w.to_le_bytes()).collect();
    let range = |kind, table_offset_dwords| UsedResource {
        kind,
        root: RootType::Table,
        table_offset_dwords,
        lower_bound: 0,
        stride: 4,
        dwords: 4,
        user_data: 4,
        slot: 1,
    };
    NativeProgram {
        stage: HwStage::Cs,
        wave64,
        // Every program stays below v64 (the helpers' registers included).
        vgprs: 64,
        layout: vec![
            RootParam {
                root: RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 4,
                base_register: 0,
                user_data: 0,
                slot: 0,
            },
            RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 4,
                slot: 1,
            },
        ],
        used: vec![range(ResourceType::Srv, 0), range(ResourceType::Uav, 8)],
        io: StageIo::Cs(ComputeIo {
            rsrc2: (5 << 1) | ((lds / 512) << 15),
            threads: [threads, 1, 1],
        }),
        code: bytes,
    }
}

/// Translates the program; a refusal or invalid DXIL is the error.
fn translate(native: &NativeProgram) -> Result<HostShader, String> {
    let e = |what: &str, err: crate::ShaderError| format!("{what}: {err}");
    let p = ir::build(&native.code, native.wave64, native.vgprs).map_err(|x| e("ir", x))?;
    let st = structure::structure(&p).map_err(|x| e("structure", x))?;
    let res = resource::track(&p, &native.layout, native.stage, Some(&st))
        .map_err(|x| e("resources", x))?;
    let io = io::raise(&p, native).map_err(|x| e("io", x))?;
    let lowered = lower::lower(&lower::Parts {
        native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
    })
    .map_err(|x| e("lower", x))?;
    let shader = HostShader {
        stage: HwStage::Cs,
        dxil: lowered.dxil,
        user_data: lowered.user_data,
        samplers: lowered.samplers,
        static_samplers: Vec::new(),
        root_cbvs: lowered.root_cbvs,
        root_srvs: lowered.root_srvs,
        root_uavs: lowered.root_uavs,
        root_indexed_views: res.root_indexed_views,
        half_views: lowered.half_views,
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        v.validate(&mut shader.dxil.clone())
            .map_err(|x| format!("DXIL validation: {x}"))?;
    }
    Ok(shader)
}

/// The output buffer after one group on `host`; `Ok(None)` when the host
/// refuses the pipeline (WARP takes no fixed wave size of 32).
fn on_gpu(
    host: &Host,
    warp: bool,
    shader: &HostShader,
    input: &[u32],
    k: [u32; 4],
) -> Result<Option<Vec<u32>>, String> {
    // WARP takes no fixed wave size of 32, and a GPU of 32-lane waves none
    // of 64: the host's limits, not the translation's.
    let (root, pso) = match host.compute_pipeline(shader) {
        Ok(p) => p,
        Err(e) => {
            let label = if warp { "warp" } else { "hardware" };
            eprintln!("    {label}: pipeline refused ({e}); not run there");
            return Ok(None);
        }
    };
    let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    for u in &shader.user_data {
        ud[u.index as usize] = match (u.root, u.slot) {
            (RootType::Constants, 0) => k[u.dword as usize],
            (RootType::Table, 1) => 0,
            // The address of a buffer in the table, as the interpreter's
            // descriptors give it (input at dword 0, output at dword 8).
            (RootType::Other(super::shader::USER_VIEW_ADDRESS), 1) => match u.dword {
                0 => 0x1000_0000,
                8 => 0x2000_0000,
                _ => return Err(format!("no buffer at table dword {}", u.dword)),
            },
            (RootType::Other(a), 1) if a == super::shader::USER_VIEW_ADDRESS + 1 => 0,
            other => {
                return Err(format!(
                    "unexpected user data {other:?} (dword {})",
                    u.dword
                ));
            }
        };
    }
    let views = [
        View::Buffer {
            data: input,
            stride: Stride::Raw,
        },
        View::RawUav(OUT_DWORDS as u32),
    ];
    let out = host.dispatch(
        &root,
        &pso,
        &Dispatch {
            roots: vec![(0, Root::Constants(ud))],
            views: &views,
            samplers: &[],
            groups: [1, 1, 1],
        },
    )?;
    Ok(out.into_iter().next())
}

struct Gpus(Vec<(&'static str, bool, Host)>);

impl Gpus {
    fn new() -> Option<Self> {
        let hosts: Vec<_> = [("hardware", false), ("warp", true)]
            .into_iter()
            .filter_map(|(label, warp)| match Host::new(warp) {
                Ok(h) => Some((label, warp, h)),
                Err(e) => {
                    eprintln!("no SM 6.6 {label} device ({e}); not run there");
                    None
                }
            })
            .collect();
        (!hosts.is_empty()).then_some(Gpus(hosts))
    }
}

enum Outcome {
    Equal,
    /// The interpreter does not cover the program.
    Unsupported(String),
    /// The translation refuses the program, or no host runs it.
    Refused(String),
    /// Invalid DXIL, a failed dispatch, or different output: the report.
    Failed(String),
}

/// Runs one program both ways on every host.
#[allow(clippy::too_many_arguments)]
fn differ(
    gpus: &Gpus,
    name: &str,
    code: &[u32],
    wave64: bool,
    threads: u32,
    lds: u32,
    cmp: Cmp,
    seed: u64,
) -> Outcome {
    let (input, k) = inputs(seed);
    let group = Group {
        code,
        wave64,
        threads,
        lds_bytes: lds,
        constants: k,
        input: &input,
        output_dwords: OUT_DWORDS,
    };
    let want = match interp::run(&group) {
        Ok(w) => w,
        Err(interp::Error::Unsupported(what)) => return Outcome::Unsupported(what),
        Err(e) => return Outcome::Failed(format!("{name}: reference: {e}")),
    };
    let report = |what: String| {
        Outcome::Failed(format!(
            "{name} (seed {seed}, constants {k:08x?}): {what}\n{}",
            disassembly(code, wave64)
        ))
    };
    let shader = match translate(&native(code, wave64, threads, lds)) {
        Ok(s) => s,
        Err(e) if e.starts_with("DXIL validation") => return report(e),
        Err(e) => return Outcome::Refused(format!("{name}: {e}")),
    };
    let mut ran = 0;
    for (label, warp, host) in &gpus.0 {
        let got = match on_gpu(host, *warp, &shader, &input, k) {
            Ok(Some(g)) => g,
            Ok(None) => continue,
            Err(e) => return report(format!("{label}: {e}")),
        };
        ran += 1;
        if got.len() != OUT_DWORDS {
            return report(format!("{label}: {} output words", got.len()));
        }
        let bad: Vec<usize> = (0..OUT_DWORDS)
            .filter(|&i| !same(cmp, want[i], got[i]))
            .collect();
        if !bad.is_empty() {
            let lines: String = bad
                .iter()
                .take(8)
                .map(|&i| {
                    format!(
                        "    word {i} (row {}, thread {}): want {:#010x} ({:e}), got {:#010x} ({:e})\n",
                        i / 64,
                        i % 64,
                        want[i],
                        f32::from_bits(want[i]),
                        got[i],
                        f32::from_bits(got[i]),
                    )
                })
                .collect();
            return report(format!(
                "{label}: {} of {OUT_DWORDS} words differ ({cmp:?}):\n{lines}",
                bad.len()
            ));
        }
    }
    if ran == 0 {
        return Outcome::Refused(format!("{name}: {NO_HOST}"));
    }
    Outcome::Equal
}

/// Runs a corpus case; the corpus is ours, so the interpreter must cover it
/// and the translation take it.
fn run_case(gpus: &Gpus, case: &Case) -> Result<(), String> {
    let code = program(case.body);
    match differ(
        gpus,
        case.name,
        &code,
        case.wave64,
        case.threads,
        case.lds,
        case.cmp,
        0x5EED,
    ) {
        Outcome::Equal => {
            eprintln!("{}: equal", case.name);
            Ok(())
        }
        Outcome::Unsupported(what) => Err(format!("{}: reference {what}", case.name)),
        // (A console wave of 64 on a GPU of 32-lane waves, say.)
        Outcome::Refused(what) if what.ends_with(NO_HOST) => {
            eprintln!("{what}: skipped");
            Ok(())
        }
        Outcome::Refused(what) => Err(format!("refused: {what}")),
        Outcome::Failed(report) => Err(report),
    }
}

const NO_HOST: &str = "no host creates its pipeline";

/// Runs `case` alone (the `#[ignore]` cases).
fn run_alone(case: &Case) {
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some(gpus) = Gpus::new() else {
        eprintln!("no SM 6.6 device; skipped");
        return;
    };
    if let Err(e) = run_case(&gpus, case) {
        panic!("{e}");
    }
}

// ---------------------------------------------------------------------------
// The corpus.

/// v2..v(2+n) = input words at `rows` (byte offsets).
fn load(a: &mut Asm, first: u8, rows: &[u16]) {
    for (i, &off) in rows.iter().enumerate() {
        a.ld(first + i as u8, off);
    }
}

/// `v[dst]` = NaN where this lane's small input at `row` is below `below`
/// (`v[dst]` unchanged elsewhere). Uses v59.
fn nan_where(a: &mut Asm, dst: u8, row: u16, below: i32) {
    a.ld(59, SMALL + row);
    a.vopc("v_cmp_gt_u32", c(below), 59);
    a.vop1("v_mov_b32", 59, lit(0x7FC0_0000));
    a.vop2("v_cndmask_b32", dst, v(dst), 59);
}

const CORPUS: &[Case] = &[
    case("integer add and subtract", Cmp::Exact, |a| {
        load(a, 2, &[INT, INT + ROW]);
        a.vop2("v_add_nc_u32", 4, v(2), 3);
        a.vop2("v_sub_nc_u32", 5, v(2), 3);
        a.vop2("v_subrev_nc_u32", 6, v(2), 3);
        a.vop2("v_add_nc_u32", 7, s(1), 2);
        a.vop2("v_add_nc_u32", 8, lit(0x1234_5678), 2);
        a.vop2("v_sub_nc_u32", 9, c(-7), 3);
        a.vop3("v_add_nc_u32", 10, &[s(1), lit(0xCAFE_F00D)]);
        a.vop3("v_sub_nc_u32", 11, &[v(2), s(3)]);
        for (row, r) in (4..12).enumerate() {
            a.st(r, row as u16);
        }
    }),
    case("integer multiplies", Cmp::Exact, |a| {
        load(a, 2, &[INT, INT + ROW]);
        a.vop3("v_mul_lo_u32", 4, &[v(2), v(3)]);
        a.vop3("v_mul_hi_u32", 5, &[v(2), v(3)]);
        a.vop3("v_mul_hi_i32", 6, &[v(2), v(3)]);
        a.vop3("v_mul_lo_u32", 7, &[v(2), s(1)]);
        a.vop2("v_mul_u32_u24", 8, v(2), 3);
        a.vop2("v_mul_hi_u32_u24", 9, v(2), 3);
        a.vop2("v_mul_i32_i24", 10, v(2), 3);
        a.vop2("v_mul_hi_i32_i24", 11, v(2), 3);
        a.vop3("v_mul_hi_u32", 12, &[v(2), lit(0xDEAD_BEEF)]);
        for (row, r) in (4..13).enumerate() {
            a.st(r, row as u16);
        }
    }),
    case("shifts by any amount", Cmp::Exact, |a| {
        load(a, 2, &[INT, SMALL, INT + ROW]);
        a.vop2("v_lshlrev_b32", 5, v(3), 2);
        a.vop2("v_lshrrev_b32", 6, v(3), 2);
        a.vop2("v_ashrrev_i32", 7, v(3), 2);
        a.vop2("v_lshlrev_b32", 8, v(4), 2);
        a.vop2("v_lshrrev_b32", 9, v(4), 2);
        a.vop2("v_ashrrev_i32", 10, v(4), 2);
        a.vop2("v_lshlrev_b32", 11, c(31), 2);
        a.vop2("v_ashrrev_i32", 12, s(0), 2);
        a.vop3("v_lshrrev_b32", 13, &[v(2), lit(0x8765_4321)]);
        for (row, r) in (5..14).enumerate() {
            a.st(r, row as u16);
        }
    }),
    case("bitwise operations", Cmp::Exact, |a| {
        load(a, 2, &[INT, INT + ROW, INT + 2 * ROW]);
        a.vop2("v_and_b32", 5, v(2), 3);
        a.vop2("v_or_b32", 6, v(2), 3);
        a.vop2("v_xor_b32", 7, v(2), 3);
        a.vop2("v_xnor_b32", 8, v(2), 3);
        a.vop1("v_not_b32", 9, v(2));
        a.vop1("v_bfrev_b32", 10, v(2));
        a.vop3("v_and_or_b32", 11, &[v(2), v(3), v(4)]);
        a.vop3("v_or3_b32", 12, &[v(2), v(3), v(4)]);
        a.vop3("v_xor3_b32", 13, &[v(2), s(1), v(4)]);
        a.vop2("v_and_b32", 14, lit(0xF0F0_0FF0), 2);
        for (row, r) in (5..15).enumerate() {
            a.st(r, row as u16);
        }
    }),
    case("bit fields", Cmp::Exact, |a| {
        load(a, 2, &[INT, SMALL, SMALL + ROW, INT + ROW, INT + 2 * ROW]);
        a.vop3("v_bfe_u32", 7, &[v(2), v(3), v(4)]);
        a.vop3("v_bfe_i32", 8, &[v(2), v(3), v(4)]);
        a.vop3("v_bfi_b32", 9, &[v(2), v(5), v(6)]);
        a.vop3("v_bfm_b32", 10, &[v(3), v(4)]);
        a.vop3("v_alignbit_b32", 11, &[v(2), v(5), v(3)]);
        a.vop3("v_alignbyte_b32", 12, &[v(2), v(5), v(3)]);
        a.vop3("v_bfe_u32", 13, &[v(2), c(4), c(12)]);
        a.vop3("v_bfe_i32", 14, &[v(2), c(28), c(8)]);
        a.vop3("v_bfe_i32", 15, &[v(2), s(0), c(5)]);
        for (row, r) in (7..16).enumerate() {
            a.st(r, row as u16);
        }
    }),
    case("bit counts and lane counts", Cmp::Exact, |a| {
        load(a, 2, &[INT, SMALL]);
        a.vop2("v_add_nc_u32", 4, c(-1), 3); // -1 where the small input is 0
        a.vop1("v_ffbh_u32", 5, v(2));
        a.vop1("v_ffbh_u32", 6, v(3));
        a.vop1("v_ffbl_b32", 7, v(2));
        a.vop1("v_ffbl_b32", 8, v(3));
        a.vop1("v_ffbh_i32", 9, v(2));
        a.vop1("v_ffbh_i32", 10, v(4));
        a.vop3("v_bcnt_u32_b32", 11, &[v(2), v(3)]);
        a.vop3("v_mbcnt_lo_u32_b32", 12, &[s(1), c(0)]);
        a.vop3("v_mbcnt_hi_u32_b32", 13, &[s(3), v(12)]);
        a.vop3("v_mbcnt_lo_u32_b32", 14, &[c(-1), c(0)]);
        for (row, r) in (5..15).enumerate() {
            a.st(r, row as u16);
        }
    }),
    case("integer minimum, maximum and median", Cmp::Exact, |a| {
        load(a, 2, &[INT, INT + ROW, INT + 2 * ROW]);
        a.vop2("v_min_i32", 5, v(2), 3);
        a.vop2("v_max_i32", 6, v(2), 3);
        a.vop2("v_min_u32", 7, v(2), 3);
        a.vop2("v_max_u32", 8, v(2), 3);
        a.vop3("v_min3_i32", 9, &[v(2), v(3), v(4)]);
        a.vop3("v_max3_i32", 10, &[v(2), v(3), v(4)]);
        a.vop3("v_med3_i32", 11, &[v(2), v(3), v(4)]);
        a.vop3("v_min3_u32", 12, &[v(2), v(3), v(4)]);
        a.vop3("v_max3_u32", 13, &[v(2), v(3), v(4)]);
        a.vop3("v_med3_u32", 14, &[v(2), v(3), v(4)]);
        a.vop3("v_med3_i32", 15, &[v(2), c(-5), c(40)]);
        for (row, r) in (5..16).enumerate() {
            a.st(r, row as u16);
        }
    }),
    case("24-bit and three-operand arithmetic", Cmp::Exact, |a| {
        load(a, 2, &[INT, INT + ROW, INT + 2 * ROW, SMALL]);
        a.vop3("v_mad_u32_u24", 6, &[v(2), v(3), v(4)]);
        a.vop3("v_mad_i32_i24", 7, &[v(2), v(3), v(4)]);
        a.vop3("v_add3_u32", 8, &[v(2), v(3), v(4)]);
        a.vop3("v_lshl_add_u32", 9, &[v(2), v(5), v(4)]);
        a.vop3("v_add_lshl_u32", 10, &[v(2), v(3), v(5)]);
        a.vop3("v_xad_u32", 11, &[v(2), v(3), v(4)]);
        a.vop3("v_sad_u32", 12, &[v(2), v(3), v(4)]);
        a.vop3("v_lshl_or_b32", 13, &[v(2), v(5), v(4)]);
        a.vop3("v_lshl_add_u32", 14, &[v(2), c(3), lit(0x1000)]);
        for (row, r) in (6..15).enumerate() {
            a.st(r, row as u16);
        }
    }),
    case("byte permutes, sums and conversions", Cmp::Exact, |a| {
        load(a, 2, &[INT, INT + ROW, INT + 2 * ROW, INT + 3 * ROW]);
        a.vop2("v_and_b32", 6, lit(0x0F0F_0F0F), 4); // selectors 0-15
        a.vop3("v_perm_b32", 7, &[v(2), v(3), v(6)]);
        a.vop3("v_perm_b32", 8, &[v(2), v(3), lit(0x0C0D_0800)]);
        a.vop3("v_perm_b32", 9, &[v(2), v(3), lit(0x0B0A_0908)]);
        a.vop3("v_sad_u8", 10, &[v(2), v(3), v(5)]);
        a.vop3("v_sad_u8", 11, &[v(3), v(2), c(0)]);
        a.vop1("v_cvt_f32_ubyte0", 12, v(2));
        a.vop1("v_cvt_f32_ubyte1", 13, v(2));
        a.vop1("v_cvt_f32_ubyte2", 14, v(2));
        a.vop1("v_cvt_f32_ubyte3", 15, v(2));
        for (row, r) in (7..16).enumerate() {
            a.st(r, row as u16);
        }
    }),
    case("64-bit add and subtract with carries", Cmp::Exact, |a| {
        load(a, 2, &[INT, INT + ROW, INT + 2 * ROW, INT + 3 * ROW]);
        a.vop3b("v_add_co_u32", 6, 106, &[v(2), v(4)]);
        a.vop2("v_add_co_ci_u32", 7, v(3), 5);
        a.vop3("v_cndmask_b32", 8, &[c(0), c(1), VCC]);
        a.vop3b("v_sub_co_u32", 9, 20, &[v(2), v(4)]);
        a.vop3b("v_sub_co_ci_u32", 10, 21, &[v(3), v(5), s(20)]);
        a.vop3("v_cndmask_b32", 11, &[c(0), c(1), s(21)]);
        a.vop3b("v_subrev_co_u32", 12, 22, &[v(2), v(4)]);
        a.vop3("v_cndmask_b32", 13, &[c(0), c(1), s(22)]);
        a.vop3b("v_add_co_u32", 14, 23, &[v(2), s(1)]);
        a.vop3b("v_subrev_co_ci_u32", 15, 24, &[v(3), c(5), s(23)]);
        a.vop3("v_cndmask_b32", 16, &[c(0), c(1), s(24)]);
        for (row, r) in (6..17).enumerate() {
            a.st(r, row as u16);
        }
    }),
    case("64-bit shifts", Cmp::Exact, |a| {
        load(a, 2, &[INT, INT + ROW, SMALL]);
        a.vop3("v_lshlrev_b64", 6, &[v(4), v(2)]);
        a.vop3("v_lshrrev_b64", 8, &[v(4), v(2)]);
        a.vop3("v_ashrrev_i64", 10, &[v(4), v(2)]);
        a.vop3("v_lshrrev_b64", 12, &[c(33), v(2)]);
        for (row, r) in (6..14).enumerate() {
            a.st(r, row as u16);
        }
    }),
    case("64-bit multiply-adds", Cmp::Exact, |a| {
        load(a, 2, &[INT, INT + ROW, INT + 2 * ROW, INT + 3 * ROW]);
        a.vop3b("v_mad_u64_u32", 6, 20, &[v(2), v(3), v(4)]);
        a.vop3b("v_mad_u64_u32", 8, 22, &[v(3), v(3), v(2)]);
        a.vop3("v_cndmask_b32", 10, &[c(0), c(1), s(20)]);
        a.vop3("v_cndmask_b32", 11, &[c(0), c(1), s(22)]);
        a.vop3b("v_mad_u64_u32", 12, 24, &[v(2), s(1), c(7)]);
        for (row, r) in (6..14).enumerate() {
            a.st(r, row as u16);
        }
    }),
    case("float arithmetic", Cmp::Float, |a| {
        load(a, 2, &[FLT, FLT + ROW, FLT + 2 * ROW]);
        a.vop2("v_add_f32", 5, v(2), 3);
        a.vop2("v_sub_f32", 6, v(2), 3);
        a.vop2("v_subrev_f32", 7, v(2), 3);
        a.vop2("v_mul_f32", 8, v(2), 3);
        a.vop3("v_fma_f32", 9, &[v(2), v(3), v(4)]);
        a.vop1("v_mov_b32", 10, v(4));
        a.vop2("v_fmac_f32", 10, v(2), 3);
        a.vop2k("v_fmamk_f32", 11, v(2), 3, 1.5);
        a.vop2k("v_fmaak_f32", 12, v(2), 3, -2.75);
        a.vop2("v_add_f32", 13, s(2), 2);
        a.vop2("v_mul_f32", 14, fk(-4.0), 2);
        a.vop3("v_fma_f32", 15, &[v(2), fk(0.5), s(2)]);
        for (row, r) in (5..16).enumerate() {
            a.st(r, row as u16);
        }
    }),
    case("unfused multiply-adds", Cmp::Float, |a| {
        load(a, 2, &[FLT, FLT + ROW, FLT + 2 * ROW]);
        a.vop3("v_mad_f32", 5, &[v(2), v(3), v(4)]);
        a.vop1("v_mov_b32", 6, v(4));
        a.vop2("v_mac_f32", 6, v(2), 3);
        a.vop2k("v_madmk_f32", 7, v(2), 3, 1.5);
        a.vop2k("v_madak_f32", 8, v(2), 3, -2.75);
        for (row, r) in (5..9).enumerate() {
            a.st(r, row as u16);
        }
    }),
    case(
        "float minimum, maximum and median with NaNs",
        Cmp::Float,
        |a| {
            load(a, 2, &[FLT, FLT + ROW, FLT + 2 * ROW]);
            nan_where(a, 2, 0, 12);
            nan_where(a, 3, ROW, 12);
            nan_where(a, 4, 2 * ROW, 12);
            a.vop2("v_min_f32", 5, v(2), 3);
            a.vop2("v_max_f32", 6, v(2), 3);
            a.vop3("v_min3_f32", 7, &[v(2), v(3), v(4)]);
            a.vop3("v_max3_f32", 8, &[v(2), v(3), v(4)]);
            a.vop3("v_med3_f32", 9, &[v(2), v(3), v(4)]);
            a.vop3("v_med3_f32", 10, &[v(2), fk(-1.0), fk(1.0)]);
            a.vop3m("v_max_f32", 11, &[v(2), v(3)], 0, 3, false, 0);
            for (row, r) in (5..12).enumerate() {
                a.st(r, row as u16);
            }
        },
    ),
    case("float source and output modifiers", Cmp::Float, |a| {
        load(a, 2, &[FLT, FLT + ROW, FLT + 2 * ROW, ANG]);
        a.vop3m("v_add_f32", 6, &[v(2), v(3)], 1, 2, false, 0);
        a.vop3m("v_add_f32", 7, &[v(2), v(3)], 3, 1, false, 0);
        a.vop3m("v_mul_f32", 8, &[v(2), v(3)], 0, 0, true, 0);
        a.vop3m("v_fma_f32", 9, &[v(2), v(3), v(4)], 0, 4, false, 1);
        a.vop3m("v_fma_f32", 10, &[v(2), v(3), v(4)], 4, 0, false, 3);
        a.vop3m("v_mul_f32", 11, &[v(5), fk(0.5)], 0, 0, true, 2);
        a.vop3m("v_fract_f32", 12, &[v(5)], 1, 0, false, 0);
        a.vop3m("v_floor_f32", 13, &[v(5)], 0, 1, false, 0);
        a.vop3m("v_add_f32", 14, &[v(5), fk(-0.5)], 0, 0, true, 0);
        for (row, r) in (6..15).enumerate() {
            a.st(r, row as u16);
        }
    }),
    case("legacy multiply", Cmp::Float, |a| {
        load(a, 2, &[FLT, FLT + ROW]);
        nan_where(a, 3, 0, 10);
        // +inf where the second small row is below 10.
        a.ld(59, SMALL + ROW);
        a.vopc("v_cmp_gt_u32", c(10), 59);
        a.vop1("v_mov_b32", 59, lit(0x7F80_0000));
        a.vop2("v_cndmask_b32", 2, v(2), 59);
        // -0 where the third small row is below 10.
        a.ld(59, SMALL + 2 * ROW);
        a.vopc("v_cmp_gt_u32", c(10), 59);
        a.vop1("v_mov_b32", 59, lit(0x8000_0000));
        a.vop2("v_cndmask_b32", 3, v(3), 59);
        a.vop2("v_mul_legacy_f32", 5, v(2), 3);
        a.vop3m("v_mul_legacy_f32", 6, &[v(2), v(3)], 0, 1, false, 0);
        a.vop2("v_mul_legacy_f32", 7, c(0), 3);
        a.vop2("v_mul_f32", 8, v(2), 3);
        for (row, r) in (5..9).enumerate() {
            a.st(r, row as u16);
        }
    }),
    case("rounding and fractions", Cmp::Float, |a| {
        load(a, 2, &[ANG, ANG + ROW, FLT]);
        for (i, op) in [
            "v_floor_f32",
            "v_ceil_f32",
            "v_trunc_f32",
            "v_rndne_f32",
            "v_fract_f32",
        ]
        .into_iter()
        .enumerate()
        {
            a.vop1(op, 10 + i as u8, v(2));
            a.vop1(op, 20 + i as u8, v(4));
            a.st(10 + i as u8, i as u16);
            a.st(20 + i as u8, 5 + i as u16);
        }
        // Small negatives: v_fract_f32 stays below one.
        a.vop2("v_mul_f32", 5, lit(0x3000_0000), 3); // 2^-31
        a.vop1("v_fract_f32", 6, v(5));
        a.st(6, 10);
        nan_where(a, 4, 0, 10);
        a.vop1("v_fract_f32", 7, v(4));
        a.st(7, 11);
    }),
    // The console's default float mode flushes f32 denormals, operands and
    // results, to signed zero.
    case("denormals are flushed", Cmp::Exact, |a| {
        load(a, 2, &[INT, FLT]);
        a.vop2("v_and_b32", 2, lit(0x807F_FFFF), 2); // denormals of either sign
        a.vop2("v_add_f32", 4, c(0), 2);
        a.vop2("v_mul_f32", 5, lit(0x4B80_0000), 2); // × 2^24
        a.vopc("v_cmp_lt_f32", c(0), 2);
        a.vop3("v_cndmask_b32", 6, &[c(0), c(1), VCC]);
        a.vop2("v_and_b32", 7, lit(0x807F_FFFF), 3);
        a.vop2("v_or_b32", 7, lit(0x0080_0000), 7); // near the smallest normal
        a.vop2("v_mul_f32", 8, fk(0.5), 7); // denormal results
        a.vop2("v_max_f32", 9, c(0), 2);
        for (row, r) in (4..10).enumerate() {
            a.st(r, row as u16);
        }
    }),
    case("float to integer conversions", Cmp::Exact, |a| {
        load(a, 2, &[FLT, ANG, SMALL]);
        // Values up to 2^40: the conversions saturate.
        a.vop2("v_mul_f32", 5, lit(0x4980_0000), 2); // 2^20
        nan_where(a, 5, ROW, 6);
        a.vop1("v_cvt_i32_f32", 6, v(5));
        a.vop1("v_cvt_u32_f32", 7, v(5));
        a.vop1("v_cvt_i32_f32", 8, v(3));
        a.vop1("v_cvt_u32_f32", 9, v(3));
        a.vop1("v_cvt_flr_i32_f32", 10, v(3));
        a.vop1("v_cvt_flr_i32_f32", 11, v(5));
        a.vop1("v_cvt_rpi_i32_f32", 12, v(3));
        for (row, r) in (6..13).enumerate() {
            a.st(r, row as u16);
        }
    }),
    case("integer to float conversions", Cmp::Exact, |a| {
        load(a, 2, &[INT, SMALL]);
        a.vop1("v_cvt_f32_i32", 5, v(2));
        a.vop1("v_cvt_f32_u32", 6, v(2));
        a.vop1("v_cvt_f32_i32", 7, v(3));
        a.vop1("v_cvt_off_f32_i4", 8, v(2));
        a.vop1("v_cvt_off_f32_i4", 9, v(3));
        a.vop1("v_cvt_f32_u32", 10, s(1));
        a.vop2("v_lshrrev_b32", 11, c(7), 2);
        a.vop1("v_cvt_f32_u32", 12, v(11)); // 25-bit values: rounding
        for (row, r) in [5, 6, 7, 8, 9, 10, 12].into_iter().enumerate() {
            a.st(r, row as u16);
        }
    }),
    case("reciprocals and square roots", Cmp::Near(1, 0.0), |a| {
        load(a, 2, &[FLT, FLT + ROW]);
        a.vop1("v_rcp_f32", 5, v(2));
        a.vop3m("v_rsq_f32", 6, &[v(2)], 1, 0, false, 0);
        a.vop3m("v_sqrt_f32", 7, &[v(2)], 1, 0, false, 0);
        a.vop1("v_sqrt_f32", 8, v(3)); // negatives: NaN
        a.vop1("v_rsq_f32", 9, v(3));
        a.vop1("v_rcp_iflag_f32", 10, v(3));
        for (row, r) in (5..11).enumerate() {
            a.st(r, row as u16);
        }
    }),
    // The host's own exp2 and log2 (NVIDIA's are good to 2 ULP and to an
    // absolute 2^-22) are no closer than this; a wrong base or scale is
    // far off.
    case(
        "exponentials and logarithms",
        Cmp::Near(2, 1.0 / 2_097_152.0),
        |a| {
            load(a, 2, &[ANG, FLT, ANG + ROW]);
            a.vop2("v_mul_f32", 5, lit(0.0625f32.to_bits()), 2); // [-19, 19]
            a.vop1("v_exp_f32", 6, v(5));
            a.vop3m("v_log_f32", 7, &[v(3)], 1, 0, false, 0);
            a.vop1("v_log_f32", 8, v(3)); // negatives: NaN
            a.vop2("v_mul_f32", 9, lit(0.25f32.to_bits()), 4);
            a.vop1("v_exp_f32", 10, v(9));
            for (row, r) in [6, 7, 8, 10].into_iter().enumerate() {
                a.st(r, row as u16);
            }
        },
    ),
    case("sines and cosines", Cmp::Near(1, 1e-5), |a| {
        load(a, 2, &[ANG, ANG + ROW, FLT]);
        a.vop1("v_sin_f32", 5, v(2));
        a.vop1("v_cos_f32", 6, v(2));
        a.vop1("v_sin_f32", 7, v(3));
        a.vop1("v_cos_f32", 8, v(3));
        a.vop2("v_mul_f32", 9, lit(0.001f32.to_bits()), 4);
        a.vop1("v_sin_f32", 10, v(9));
        for (row, r) in [5, 6, 7, 8, 10].into_iter().enumerate() {
            a.st(r, row as u16);
        }
    }),
    case("float compares", Cmp::Exact, |a| {
        load(a, 2, &[FLT, FLT + ROW]);
        nan_where(a, 2, 0, 8);
        nan_where(a, 3, ROW, 8);
        // Equal operands where the third small row is below 16.
        a.ld(59, SMALL + 2 * ROW);
        a.vopc("v_cmp_gt_u32", c(16), 59);
        a.vop2("v_cndmask_b32", 3, v(3), 2);
        a.vop1("v_mov_b32", 4, c(0));
        let conds = [
            "f", "lt", "eq", "le", "gt", "lg", "ge", "o", "u", "nge", "nlg", "ngt", "nle", "neq",
            "nlt", "tru",
        ];
        for (k, cond) in conds.iter().enumerate() {
            if k % 2 == 0 {
                a.vopc(&format!("v_cmp_{cond}_f32"), v(2), 3);
                a.bit(4, VCC, k as i32);
            } else {
                a.vop3(&format!("v_cmp_{cond}_f32"), 20, &[v(2), v(3)]);
                a.bit(4, s(20), k as i32);
            }
        }
        a.st(4, 0);
        // Classes (with the second operand's -0, inf and denormals too).
        a.vop1("v_mov_b32", 5, c(0));
        for (k, mask) in [0x3, 0x204, 0x108, 0x90, 0x60, 0x3FF]
            .into_iter()
            .enumerate()
        {
            a.vop3("v_cmp_class_f32", 21, &[v(2), lit(mask)]);
            a.bit(5, s(21), k as i32);
        }
        a.st(5, 1);
        // Against a scalar and a constant.
        a.vop1("v_mov_b32", 6, c(0));
        a.vopc("v_cmp_lt_f32", s(2), 2);
        a.bit(6, VCC, 0);
        a.vopc("v_cmp_ge_f32", fk(1.0), 2);
        a.bit(6, VCC, 1);
        a.vop3m("v_cmp_lt_f32", 22, &[v(2), v(3)], 1, 2, false, 0);
        a.bit(6, s(22), 2);
        a.st(6, 2);
    }),
    case("integer compares", Cmp::Exact, |a| {
        load(a, 2, &[INT, INT + ROW, SMALL]);
        // Equal operands where the small row is below 16.
        a.vopc("v_cmp_gt_u32", c(16), 4);
        a.vop2("v_cndmask_b32", 3, v(3), 2);
        a.vop1("v_mov_b32", 5, c(0));
        let conds = ["f", "lt", "eq", "le", "gt", "ne", "ge", "t"];
        for (k, cond) in conds.iter().enumerate() {
            for (j, ty) in ["i32", "u32"].iter().enumerate() {
                let bit = 2 * k as i32 + j as i32;
                if k % 2 == 0 {
                    a.vopc(&format!("v_cmp_{cond}_{ty}"), v(2), 3);
                    a.bit(5, VCC, bit);
                } else {
                    a.vop3(&format!("v_cmp_{cond}_{ty}"), 20, &[v(2), v(3)]);
                    a.bit(5, s(20), bit);
                }
            }
        }
        a.st(5, 0);
        a.vop1("v_mov_b32", 6, c(0));
        a.vopc("v_cmp_lt_i32", s(1), 2);
        a.bit(6, VCC, 0);
        a.vopc("v_cmp_gt_u32", lit(0x8000_0000), 2);
        a.bit(6, VCC, 1);
        a.vopc("v_cmp_le_i32", c(-3), 2);
        a.bit(6, VCC, 2);
        a.vop3("v_cmp_ne_u32", 21, &[v(4), s(0)]);
        a.bit(6, s(21), 3);
        a.st(6, 1);
    }),
    case("64-bit compares", Cmp::Exact, |a| {
        load(a, 2, &[INT, INT + ROW, INT + 2 * ROW, INT + 3 * ROW, SMALL]);
        // Equal high dwords where the small row is below 24, equal values
        // below 8.
        a.vopc("v_cmp_gt_u32", c(24), 6);
        a.vop2("v_cndmask_b32", 5, v(5), 3);
        a.vopc("v_cmp_gt_u32", c(8), 6);
        a.vop2("v_cndmask_b32", 4, v(4), 2);
        a.vop1("v_mov_b32", 7, c(0));
        let conds = ["lt", "eq", "le", "gt", "ne", "ge"];
        for (k, cond) in conds.iter().enumerate() {
            for (j, ty) in ["i64", "u64"].iter().enumerate() {
                let bit = 2 * k as i32 + j as i32;
                a.vop3(&format!("v_cmp_{cond}_{ty}"), 20, &[v(2), v(4)]);
                a.bit(7, s(20), bit);
            }
        }
        a.st(7, 0);
    }),
    case("scalar arithmetic and carries", Cmp::Exact, |a| {
        a.smem("s_buffer_load_dwordx8", 16, 8, 0);
        a.smem("s_buffer_load_dwordx4", 24, 8, u32::from(SMALL));
        a.sopp("s_waitcnt", 0);
        let mut row = 0;
        let mut put = |a: &mut Asm, d: u8, scc: bool| {
            a.st_s(d, row);
            row += 1;
            if scc {
                a.sop2("s_cselect_b32", 40, c(1), c(0));
                a.st_s(40, row);
                row += 1;
            }
        };
        a.sop2("s_add_u32", 30, s(16), s(17));
        put(a, 30, true);
        a.sop2("s_addc_u32", 31, s(18), s(19));
        put(a, 31, true);
        a.sop2("s_sub_u32", 32, s(16), s(17));
        a.sop2("s_subb_u32", 33, s(18), s(19));
        put(a, 33, true);
        a.sop2("s_add_i32", 34, s(20), s(21));
        put(a, 34, false);
        a.sop2("s_mul_i32", 35, s(16), s(17));
        put(a, 35, false);
        a.sop2("s_mul_hi_u32", 36, s(16), s(17));
        put(a, 36, false);
        a.sop2("s_mul_hi_i32", 37, s(18), s(19));
        put(a, 37, false);
        a.sop2("s_min_i32", 38, s(20), s(21));
        put(a, 38, true);
        a.sop2("s_max_u32", 39, s(22), s(23));
        put(a, 39, false);
        a.sop2("s_lshl2_add_u32", 41, s(16), s(17));
        put(a, 41, false);
    }),
    // SCC of a minimum or maximum says whether S0 was picked: 0 for equal
    // operands, though the result is not zero (a miscompile this test
    // found: the lowering gave "the result is nonzero").
    case("the SCC of scalar minimums and maximums", Cmp::Exact, |a| {
        a.smem("s_buffer_load_dwordx4", 16, 8, 0);
        a.smem("s_buffer_load_dwordx2", 20, 8, u32::from(SMALL));
        a.sopp("s_waitcnt", 0);
        let mut row = 0;
        for op in ["s_min_i32", "s_min_u32", "s_max_i32", "s_max_u32"] {
            for (x, y) in [(16, 17), (17, 16), (20, 20), (20, 21)] {
                a.sop2(op, 30, s(x), s(y));
                a.sop2("s_cselect_b32", 31, c(1), c(0));
                a.st_s(30, row);
                a.st_s(31, row + 1);
                row += 2;
            }
        }
    }),
    case("scalar shifts and bit fields", Cmp::Exact, |a| {
        a.smem("s_buffer_load_dwordx4", 16, 8, 0);
        a.smem("s_buffer_load_dwordx4", 20, 8, u32::from(SMALL));
        a.sopp("s_waitcnt", 0);
        a.sop2("s_lshl_b32", 30, s(16), s(20));
        a.sop2("s_lshr_b32", 31, s(16), s(21));
        a.sop2("s_ashr_i32", 32, s(16), s(17));
        a.sop2("s_lshl_b64", 34, s(16), s(22));
        a.sop2("s_ashr_i64", 36, s(16), s(23));
        // Offset in bits 4:0, width in 22:16.
        a.sop2("s_lshl_b32", 38, s(21), c(16));
        a.sop2("s_or_b32", 38, s(38), s(20));
        a.sop2("s_bfe_u32", 39, s(17), s(38));
        a.sop2("s_bfe_i32", 40, s(17), s(38));
        a.sop2("s_bfe_i32", 41, s(16), lit(0x0008_001C));
        a.sop2("s_bfm_b32", 42, s(21), s(20));
        a.sop2("s_lshr_b64", 44, s(16), s(21));
        for (row, r) in [30, 31, 32, 34, 35, 36, 37, 39, 40, 41, 42, 44, 45]
            .into_iter()
            .enumerate()
        {
            a.st_s(r, row as u16);
        }
    }),
    case("scalar bit operations", Cmp::Exact, |a| {
        a.smem("s_buffer_load_dwordx4", 16, 8, 4);
        a.smem("s_buffer_load_dwordx2", 20, 8, u32::from(SMALL) + 4);
        a.sopp("s_waitcnt", 0);
        let ops1 = [
            "s_not_b32",
            "s_brev_b32",
            "s_bcnt1_i32_b32",
            "s_bcnt1_i32_b64",
            "s_ff1_i32_b64",
            "s_ff1_i32_b32",
            "s_flbit_i32_b32",
            "s_flbit_i32",
            "s_abs_i32",
            "s_quadmask_b32",
        ];
        for (i, op) in ops1.into_iter().enumerate() {
            a.sop1(op, 30 + i as u8, s(16));
        }
        a.sop1("s_mov_b32", 42, s(17));
        a.sop1("s_bitset1_b32", 42, s(20));
        a.sop1("s_bitset0_b32", 42, s(21));
        a.sop2("s_andn2_b32", 43, s(16), s(17));
        a.sop2("s_orn2_b32", 44, s(16), s(17));
        a.sop2("s_nand_b32", 45, s(16), s(17));
        a.sop2("s_xnor_b64", 46, s(16), s(18));
        a.sop2("s_lshr_b32", 48, s(16), c(16));
        a.sop1("s_ff1_i32_b32", 49, s(20)); // -1 when zero
        a.sop1("s_not_b64", 50, s(18));
        a.sop2("s_and_b64", 52, s(18), s(16));
        for (row, r) in (30..53).enumerate() {
            a.st_s(r, row as u16);
        }
    }),
    case("scalar immediates and compares", Cmp::Exact, |a| {
        a.smem("s_buffer_load_dwordx4", 16, 8, 8);
        a.smem("s_buffer_load_dwordx2", 20, 8, u32::from(SMALL) + 8);
        a.sopp("s_waitcnt", 0);
        a.sopk("s_movk_i32", 30, 0x8123);
        a.sop1("s_mov_b32", 31, s(16));
        a.sopk("s_addk_i32", 31, 0x7FFF);
        a.sop1("s_mov_b32", 33, s(17));
        a.sopk("s_mulk_i32", 33, 0xFFF3);
        // SCC of each compare, bit by bit, in s34.
        a.sop1("s_mov_b32", 34, c(0));
        let mut bit = 0;
        let mut scc = |a: &mut Asm| {
            a.sop2("s_cselect_b32", 40, lit(1 << bit), c(0));
            a.sop2("s_or_b32", 34, s(34), s(40));
            bit += 1;
        };
        for cond in ["eq", "lg", "gt", "ge", "lt", "le"] {
            for ty in ["i32", "u32"] {
                a.sopc(&format!("s_cmp_{cond}_{ty}"), s(16), s(17));
                scc(a);
                a.sopk(&format!("s_cmpk_{cond}_{ty}"), 20, 40);
                scc(a);
            }
        }
        a.sopc("s_cmp_eq_u32", s(20), s(21));
        scc(a);
        a.sopc("s_bitcmp1_b32", s(16), s(20));
        scc(a);
        a.sopc("s_bitcmp0_b32", s(16), s(21));
        scc(a);
        a.sopc("s_cmp_lg_u64", s(16), s(18));
        scc(a);
        a.sopc("s_cmp_eq_u32", s(20), c(0));
        a.sop2("s_cselect_b32", 30, c(55), s(30));
        for (row, r) in (30..35).enumerate() {
            a.st_s(r, row as u16);
        }
    }),
    case("uniform branches", Cmp::Exact, |a| {
        a.smem("s_buffer_load_dwordx4", 16, 8, u32::from(SMALL) + 16);
        a.sopp("s_waitcnt", 0);
        a.ld(2, INT);
        // if (s16 < s17) v5 = v2 + 1 else v5 = v2 ^ s0
        a.sopc("s_cmp_lt_u32", s(16), s(17));
        let to_then = a.branch("s_cbranch_scc1");
        a.vop2("v_xor_b32", 5, s(0), 2);
        let to_end = a.branch("s_branch");
        a.bind(to_then);
        a.vop2("v_add_nc_u32", 5, c(1), 2);
        a.bind(to_end);
        a.st(5, 0);
        // if (s18 == 0) skip
        a.vop1("v_mov_b32", 6, c(3));
        a.sopc("s_cmp_eq_u32", s(18), c(0));
        let skip = a.branch("s_cbranch_scc1");
        a.vop1("v_mov_b32", 6, c(9));
        a.bind(skip);
        a.st(6, 1);
        // A uniform loop: s20 = sum of 1..=s0, v7 = v2 * 3^s0.
        a.sop1("s_mov_b32", 21, s(0));
        a.sop1("s_mov_b32", 20, c(0));
        a.vop1("v_mov_b32", 7, v(2));
        a.sopc("s_cmp_eq_u32", s(21), c(0));
        let done = a.branch("s_cbranch_scc1");
        let top = a.here();
        a.sop2("s_add_u32", 20, s(20), s(21));
        a.vop3("v_mul_lo_u32", 7, &[v(7), c(3)]);
        a.sop2("s_sub_u32", 21, s(21), c(1));
        a.sopc("s_cmp_lg_u32", s(21), c(0));
        a.branch_to("s_cbranch_scc1", top);
        a.bind(done);
        a.st_s(20, 2);
        a.st(7, 3);
        // SCC kept across a branch and read by s_cselect.
        a.sopc("s_cmp_gt_u32", s(19), c(30));
        let over = a.branch("s_cbranch_scc0");
        a.sop1("s_mov_b32", 22, c(5));
        a.bind(over);
        a.sop2("s_cselect_b32", 23, c(11), c(12));
        a.st_s(23, 4);
    }),
    case("divergent if and else", Cmp::Exact, |a| {
        load(a, 2, &[INT, INT + ROW, SMALL]);
        a.vopc("v_cmp_gt_u32", c(32), 4);
        a.sop1("s_and_saveexec_b32", 20, VCC);
        let to_else = a.branch("s_cbranch_execz");
        a.vop2("v_add_nc_u32", 5, v(2), 3);
        a.vop1("v_mov_b32", 6, c(1));
        a.bind(to_else);
        a.sop1("s_andn2_saveexec_b32", 20, s(20));
        let to_end = a.branch("s_cbranch_execz");
        a.vop2("v_sub_nc_u32", 5, v(2), 3);
        a.vop1("v_mov_b32", 6, c(2));
        a.bind(to_end);
        a.sop2("s_or_b32", 126, EXEC_LO, s(20));
        a.st(5, 0);
        a.st(6, 1);
    }),
    case("nested divergent branches", Cmp::Exact, |a| {
        load(a, 2, &[INT, SMALL, SMALL + ROW]);
        a.vop1("v_mov_b32", 5, c(0));
        a.vopc("v_cmp_gt_u32", c(40), 3);
        a.sop1("s_and_saveexec_b32", 20, VCC);
        let out1 = a.branch("s_cbranch_execz");
        a.vop1("v_mov_b32", 5, c(1));
        a.vopc("v_cmp_gt_u32", c(20), 4);
        a.sop1("s_and_saveexec_b32", 21, VCC);
        let out2 = a.branch("s_cbranch_execz");
        a.vop2("v_xor_b32", 5, v(2), 5);
        a.bind(out2);
        a.sop2("s_or_b32", 126, EXEC_LO, s(21));
        a.vop2("v_add_nc_u32", 5, c(7), 5);
        a.bind(out1);
        a.sop2("s_or_b32", 126, EXEC_LO, s(20));
        a.st(5, 0);
    }),
    case("divergent loop", Cmp::Exact, |a| {
        load(a, 2, &[INT, SMALL]);
        a.vop2("v_and_b32", 7, c(15), 3); // the count, per lane
        a.vop1("v_mov_b32", 5, c(0));
        a.sop1("s_mov_b32", 20, EXEC_LO);
        a.vopc("v_cmp_ne_u32", c(0), 7);
        a.sop2("s_and_b32", 126, EXEC_LO, VCC);
        let end = a.branch("s_cbranch_execz");
        let top = a.here();
        a.vop2("v_add_nc_u32", 5, v(2), 5);
        a.vop2("v_xor_b32", 5, v(7), 5);
        a.vop2("v_subrev_nc_u32", 7, c(1), 7);
        a.vopc("v_cmp_ne_u32", c(0), 7);
        a.sop2("s_and_b32", 126, EXEC_LO, VCC);
        a.branch_to("s_cbranch_execnz", top);
        a.bind(end);
        a.sop1("s_mov_b32", 126, s(20));
        a.st(5, 0);
        a.st(7, 1);
    }),
    case("a loop with a break and a branch inside", Cmp::Exact, |a| {
        load(a, 2, &[INT, SMALL]);
        a.vop2("v_and_b32", 7, c(15), 3);
        a.vop2("v_add_nc_u32", 7, c(1), 7); // 1-16 trips
        a.vop1("v_mov_b32", 5, c(0));
        a.sop1("s_mov_b32", 20, EXEC_LO);
        let top = a.here();
        a.vop2("v_add_nc_u32", 5, v(2), 5);
        // if (v7 & 1) v5 ^= v7
        a.vop2("v_and_b32", 8, c(1), 7);
        a.vopc("v_cmp_ne_u32", c(0), 8);
        a.sop1("s_and_saveexec_b32", 22, VCC);
        let skip = a.branch("s_cbranch_execz");
        a.vop2("v_xor_b32", 5, v(7), 5);
        a.bind(skip);
        a.sop2("s_or_b32", 126, EXEC_LO, s(22));
        // break when the low byte of v5 is below 16
        a.vop2("v_and_b32", 8, lit(0xFF), 5);
        a.vopc("v_cmp_gt_u32", c(16), 8);
        a.sop2("s_andn2_b32", 126, EXEC_LO, VCC);
        let done = a.branch("s_cbranch_execz");
        a.vop2("v_subrev_nc_u32", 7, c(1), 7);
        a.vopc("v_cmp_ne_u32", c(0), 7);
        a.sop2("s_and_b32", 126, EXEC_LO, VCC);
        a.branch_to("s_cbranch_execnz", top);
        a.bind(done);
        a.sop1("s_mov_b32", 126, s(20));
        a.st(5, 0);
        a.st(7, 1);
    }),
    case("if and else by s_xor_b32 of EXEC", Cmp::Exact, |a| {
        load(a, 2, &[INT, SMALL]);
        a.vop1("v_mov_b32", 5, v(2));
        a.vopc("v_cmp_lt_u32", c(20), 3);
        a.sop1("s_and_saveexec_b32", 20, VCC);
        a.sop2("s_xor_b32", 20, EXEC_LO, s(20)); // the else lanes
        let to_else = a.branch("s_cbranch_execz");
        a.vop2("v_lshlrev_b32", 5, c(3), 5);
        a.bind(to_else);
        a.sop1("s_or_saveexec_b32", 20, s(20));
        a.sop2("s_xor_b32", 126, EXEC_LO, s(20));
        let to_end = a.branch("s_cbranch_execz");
        a.vop2("v_lshrrev_b32", 5, c(5), 5);
        a.bind(to_end);
        a.sop2("s_or_b32", 126, EXEC_LO, s(20));
        a.st(5, 0);
    }),
    case("VCC_HI and M0 as plain registers", Cmp::Exact, |a| {
        a.ld(2, INT);
        a.sop1("s_mov_b32", 107, s(1)); // vcc_hi
        a.sop1("s_mov_b32", 124, s(3)); // m0
        a.vopc("v_cmp_gt_u32", c(20), 0); // wave32: vcc_lo only
        a.vop3("v_cndmask_b32", 3, &[c(0), v(2), VCC]);
        a.vop1("v_mov_b32", 4, s(107));
        a.vop2("v_add_nc_u32", 5, s(124), 2);
        a.st(3, 0);
        a.st(4, 1);
        a.st(5, 2);
    }),
    case("masking by v_cmpx", Cmp::Exact, |a| {
        load(a, 2, &[INT, SMALL]);
        a.sop1("s_mov_b32", 20, EXEC_LO);
        a.vopc("v_cmpx_gt_u32", c(32), 3);
        a.vop2("v_add_nc_u32", 5, c(9), 2);
        a.st(5, 0);
        a.sop1("s_mov_b32", 126, s(20));
        a.vopc("v_cmpx_le_u32", c(48), 3);
        a.st(2, 1);
        a.sop1("s_mov_b32", 126, s(20));
        a.st(3, 2);
    }),
    case("branches on VCC and empty EXEC", Cmp::Exact, |a| {
        a.ld(2, INT);
        // A uniform VCC from a root constant.
        a.vop1("v_mov_b32", 6, s(0));
        a.vop1("v_mov_b32", 7, c(4));
        a.vopc("v_cmp_gt_u32", c(16), 6);
        let set = a.branch("s_cbranch_vccnz");
        a.vop1("v_mov_b32", 7, c(5));
        a.bind(set);
        a.st(7, 1);
        // Threads below 8: the second wave's EXEC is empty and skips.
        a.vop1("v_mov_b32", 8, c(0));
        a.vopc("v_cmp_gt_u32", c(8), 0);
        a.sop1("s_and_saveexec_b32", 20, VCC);
        let none = a.branch("s_cbranch_execz");
        a.vop1("v_mov_b32", 8, v(2));
        a.bind(none);
        a.sop1("s_mov_b32", 126, s(20));
        a.st(8, 2);
        // The SCC s_and_saveexec sets (some lane is left) as a branch.
        a.vopc("v_cmp_gt_u32", c(40), 0);
        a.sop1("s_and_saveexec_b32", 22, VCC);
        let none = a.branch("s_cbranch_scc0");
        a.vop1("v_mov_b32", 9, c(6));
        a.bind(none);
        a.sop1("s_mov_b32", 126, s(22));
        a.st(9, 3);
    }),
    Case {
        lds: 512,
        ..case("LDS across waves", Cmp::Exact, |a| {
            load(a, 2, &[INT, INT + ROW]);
            a.ds("ds_write_b32", 0, 1, 2, 0, 0, 0);
            a.ds("ds_write_b32", 0, 1, 3, 0, 0, 1); // offset 256
            a.sopp("s_barrier", 0);
            a.vop2("v_sub_nc_u32", 4, c(252), 1); // (63 - thread) * 4
            a.ds("ds_read_b32", 5, 4, 0, 0, 0, 0);
            a.ds("ds_read_b32", 6, 1, 0, 0, 4, 0); // the next thread's
            a.ds("ds_read_b32", 7, 4, 0, 0, 0, 1);
            a.st(5, 0);
            a.st(6, 1);
            a.st(7, 2);
        })
    },
    Case {
        lds: 512,
        ..case("LDS pairs and sub-dword accesses", Cmp::Exact, |a| {
            load(a, 2, &[INT, INT + ROW]);
            a.vop2("v_sub_nc_u32", 7, c(252), 1);
            a.ds("ds_write2_b32", 0, 1, 2, 3, 0, 64);
            a.sopp("s_barrier", 0);
            a.ds("ds_read2_b32", 4, 7, 0, 0, 0, 64);
            a.st(4, 0);
            a.st(5, 1);
            a.sopp("s_barrier", 0);
            a.vop2("v_lshlrev_b32", 8, c(3), 0);
            a.ds("ds_write_b64", 0, 8, 2, 0, 0, 0);
            a.sopp("s_barrier", 0);
            a.vop2("v_sub_nc_u32", 9, lit(504), 8);
            a.ds("ds_read_b64", 10, 9, 0, 0, 0, 0);
            a.st(10, 2);
            a.st(11, 3);
            a.sopp("s_barrier", 0);
            a.ds("ds_write_b8", 0, 0, 2, 0, 0, 0); // bytes 0-63
            a.vop3("v_lshl_add_u32", 12, &[v(0), c(1), c(64)]);
            a.ds("ds_write_b16", 0, 12, 3, 0, 0, 0); // bytes 64-191
            a.sopp("s_barrier", 0);
            a.vop2("v_sub_nc_u32", 14, c(63), 0);
            a.vop3("v_lshl_add_u32", 15, &[v(14), c(1), c(64)]);
            a.ds("ds_read_u8", 16, 14, 0, 0, 0, 0);
            a.ds("ds_read_i8", 17, 14, 0, 0, 0, 0);
            a.ds("ds_read_u16", 18, 15, 0, 0, 0, 0);
            a.ds("ds_read_i16", 19, 15, 0, 0, 0, 0);
            for (row, r) in (16..20).enumerate() {
                a.st(r, 4 + row as u16);
            }
        })
    },
    Case {
        lds: 512,
        ..case("LDS atomics", Cmp::Exact, |a| {
            load(a, 2, &[INT, INT + ROW, SMALL]);
            a.ds("ds_write_b32", 0, 1, 3, 0, 0, 0);
            a.ds("ds_write_b32", 0, 1, 4, 0, 0, 1);
            a.sopp("s_barrier", 0);
            a.vop2("v_and_b32", 5, c(7), 0);
            a.vop2("v_lshlrev_b32", 5, c(2), 5); // (thread & 7) * 4
            let ops = [
                "ds_add_u32",
                "ds_max_u32",
                "ds_min_i32",
                "ds_and_b32",
                "ds_or_b32",
                "ds_xor_b32",
                "ds_sub_u32",
                "ds_max_i32",
            ];
            for (i, op) in ops.into_iter().enumerate() {
                a.ds(op, 0, 5, 2, 0, 32 * i as u8, 0);
            }
            a.ds("ds_add_rtn_u32", 6, 1, 4, 0, 0, 1); // its own dword: the old value
            a.sopp("s_barrier", 0);
            a.ds("ds_read_b32", 7, 1, 0, 0, 0, 0);
            a.ds("ds_read_b32", 8, 1, 0, 0, 0, 1);
            a.st(6, 0);
            a.st(7, 1);
            a.st(8, 2);
        })
    },
    case("buffer access widths", Cmp::Exact, |a| {
        a.vop2("v_lshlrev_b32", 2, c(3), 0); // thread * 8
        a.vop2("v_lshlrev_b32", 3, c(4), 0); // thread * 16
        a.mubuf("buffer_load_dwordx2", 4, 2, 8, c(0), 0);
        a.mubuf("buffer_load_dwordx4", 6, 3, 8, c(0), 1024);
        a.mubuf("buffer_store_dwordx2", 4, 2, 12, c(0), 0);
        a.mubuf("buffer_store_dwordx4", 6, 3, 12, c(0), 512);
        a.mubuf("buffer_load_ubyte", 10, 0, 8, c(0), 5);
        a.mubuf("buffer_load_sbyte", 11, 0, 8, c(0), 9);
        a.vop2("v_lshlrev_b32", 12, c(1), 0);
        a.mubuf("buffer_load_ushort", 13, 12, 8, c(0), 2);
        a.mubuf("buffer_load_sshort", 14, 12, 8, c(0), 130);
        for (row, r) in (10..15).enumerate() {
            a.st(r, 6 + row as u16);
        }
        a.mubuf("buffer_store_byte", 13, 0, 12, c(0), 11 * ROW + 1);
        a.mubuf("buffer_store_short", 14, 12, 12, c(0), 12 * ROW);
        // An SGPR offset, and a load past the end (0).
        a.sopk("s_movk_i32", 20, 2048);
        a.mubuf("buffer_load_dword", 15, 1, 8, s(20), 4);
        a.st(15, 13);
        a.sopk("s_movk_i32", 21, 0x7FF0);
        a.mubuf("buffer_load_dword", 16, 1, 8, s(21), 0);
        a.st(16, 14);
    }),
    case("scalar buffer loads", Cmp::Exact, |a| {
        a.smem("s_buffer_load_dword", 20, 8, 0x10);
        a.smem("s_buffer_load_dwordx2", 22, 8, 0x100);
        a.smem("s_buffer_load_dwordx4", 24, 8, 0x200);
        a.smem("s_buffer_load_dwordx8", 32, 8, 0x380);
        a.smem("s_buffer_load_dword", 40, 8, 0x4000); // past the end: 0
        a.sopp("s_waitcnt", 0);
        for (row, r) in [20, 22, 23, 24, 27, 32, 39, 40].into_iter().enumerate() {
            a.st_s(r, row as u16);
        }
    }),
    case("lane reads and writes", Cmp::Exact, |a| {
        load(a, 2, &[INT, INT + ROW]);
        a.vop3("v_readlane_b32", 20, &[v(2), c(5)]);
        a.vop3("v_readlane_b32", 21, &[v(2), s(0)]);
        a.vopc("v_cmp_lt_u32", c(2), 0);
        a.sop1("s_and_saveexec_b32", 24, VCC);
        a.vop1("v_readfirstlane_b32", 22, v(3));
        a.sop1("s_mov_b32", 126, s(24));
        a.vop3("v_writelane_b32", 3, &[s(20), c(7)]);
        a.st_s(20, 0);
        a.st_s(21, 1);
        a.st_s(22, 2);
        a.st(3, 3);
    }),
    case("lane masks read as numbers", Cmp::Exact, |a| {
        load(a, 2, &[SMALL]);
        a.vop3("v_cmp_gt_u32", 20, &[c(30), v(2)]);
        a.sop1("s_bcnt1_i32_b32", 21, s(20));
        a.sop1("s_ff1_i32_b32", 22, s(20));
        a.sop1("s_flbit_i32_b32", 23, s(20));
        a.sop1("s_quadmask_b32", 24, s(20));
        a.sop1("s_bcnt1_i32_b32", 25, EXEC_LO);
        a.vop3("v_mbcnt_lo_u32_b32", 3, &[s(20), c(0)]);
        a.vop3("v_cndmask_b32", 4, &[c(0), c(1), s(20)]);
        for (row, r) in (20..26).enumerate() {
            a.st_s(r, row as u16);
        }
        a.st(3, 6);
        a.st(4, 7);
    }),
    Case {
        threads: 40,
        ..case("a partial last wave", Cmp::Exact, |a| {
            load(a, 2, &[INT]);
            a.sop1("s_bcnt1_i32_b32", 20, EXEC_LO);
            a.vop2("v_add_nc_u32", 3, s(20), 2);
            a.st(3, 0);
            a.st_s(20, 1);
        })
    },
    Case {
        wave64: true,
        ..case("wave64 branches and masks", Cmp::Exact, |a| {
            load(a, 2, &[INT, INT + ROW, SMALL]);
            a.vopc("v_cmp_gt_u32", c(32), 4);
            a.vop2("v_cndmask_b32", 7, v(2), 3);
            a.sop1("s_and_saveexec_b64", 20, VCC);
            let to_else = a.branch("s_cbranch_execz");
            a.vop2("v_add_nc_u32", 5, v(2), 3);
            a.bind(to_else);
            a.sop1("s_andn2_saveexec_b64", 20, s(20));
            let to_end = a.branch("s_cbranch_execz");
            a.vop2("v_xor_b32", 5, v(2), 3);
            a.bind(to_end);
            a.sop2("s_or_b64", 126, Src(126, None), s(20));
            a.st(5, 0);
            a.st(7, 1);
        })
    },
    // A 64-lane mask read as a number needs the host to run the console
    // wave as one: skipped on GPUs of 32-lane waves.
    Case {
        wave64: true,
        ..case("wave64 ballot", Cmp::Exact, |a| {
            load(a, 2, &[SMALL]);
            a.vopc("v_cmp_gt_u32", c(32), 2);
            a.sop1("s_bcnt1_i32_b64", 24, VCC);
            a.sop1("s_ff1_i32_b64", 25, VCC);
            a.st_s(24, 0);
            a.st_s(25, 1);
        })
    },
    Case {
        wave64: true,
        ..case("wave64 divergent loop", Cmp::Exact, |a| {
            load(a, 2, &[INT, SMALL]);
            a.vop2("v_and_b32", 7, c(15), 3);
            a.vop1("v_mov_b32", 5, c(0));
            a.sop1("s_mov_b64", 20, Src(126, None));
            a.vopc("v_cmp_ne_u32", c(0), 7);
            a.sop2("s_and_b64", 126, Src(126, None), VCC);
            let end = a.branch("s_cbranch_execz");
            let top = a.here();
            a.vop2("v_add_nc_u32", 5, v(2), 5);
            a.vop2("v_subrev_nc_u32", 7, c(1), 7);
            a.vopc("v_cmp_ne_u32", c(0), 7);
            a.sop2("s_and_b64", 126, Src(126, None), VCC);
            a.branch_to("s_cbranch_execnz", top);
            a.bind(end);
            a.sop1("s_mov_b64", 126, s(20));
            a.st(5, 0);
        })
    },
];

/// Programs the translation refuses today: an opcode it does not lower, or
/// an SCC it does not model. Each must stay refused or, once translated,
/// match the reference.
const REFUSED: &[Case] = &[
    case("v_add_nc_i32 and v_sub_nc_i32", Cmp::Exact, |a| {
        load(a, 2, &[INT, INT + ROW]);
        a.vop3("v_add_nc_i32", 4, &[v(2), v(3)]);
        a.vop3("v_sub_nc_i32", 5, &[v(2), v(3)]);
        a.st(4, 0);
        a.st(5, 1);
    }),
    case("v_mul_lo_i32", Cmp::Exact, |a| {
        load(a, 2, &[INT, INT + ROW]);
        a.vop3("v_mul_lo_i32", 4, &[v(2), v(3)]);
        a.st(4, 0);
    }),
    case("v_lerp_u8", Cmp::Exact, |a| {
        load(a, 2, &[INT, INT + ROW, INT + 2 * ROW]);
        a.vop3("v_lerp_u8", 5, &[v(2), v(3), v(4)]);
        a.st(5, 0);
    }),
    case("v_mad_i64_i32", Cmp::Exact, |a| {
        load(a, 2, &[INT, INT + ROW, INT + 2 * ROW, INT + 3 * ROW]);
        a.vop3b("v_mad_i64_i32", 6, 20, &[v(2), v(3), v(4)]);
        a.vop3("v_cndmask_b32", 8, &[c(0), c(1), s(20)]);
        a.st(6, 0);
        a.st(7, 1);
        a.st(8, 2);
    }),
    case("v_ldexp_f32", Cmp::Float, |a| {
        load(a, 2, &[FLT, SMALL, INT]);
        a.vop2("v_add_nc_u32", 5, c(-32), 3);
        a.vop3("v_ldexp_f32", 6, &[v(2), v(5)]);
        a.vop3("v_ldexp_f32", 7, &[v(2), v(4)]);
        a.vop3("v_ldexp_f32", 8, &[v(2), c(-140)]);
        for (row, r) in (6..9).enumerate() {
            a.st(r, row as u16);
        }
    }),
    case("the overflow s_add_i32 leaves in SCC", Cmp::Exact, |a| {
        a.smem("s_buffer_load_dwordx2", 16, 8, 0);
        a.sopp("s_waitcnt", 0);
        a.sop2("s_add_i32", 18, s(16), s(17));
        a.sop2("s_cselect_b32", 19, c(1), c(0));
        a.st_s(19, 0);
    }),
    case(
        "s_pack_ll_b32_b16, s_pack_lh_b32_b16 and s_pack_hh_b32_b16",
        Cmp::Exact,
        |a| {
            a.smem("s_buffer_load_dwordx2", 16, 8, 0);
            a.sopp("s_waitcnt", 0);
            a.sop2("s_pack_ll_b32_b16", 18, s(16), s(17));
            a.sop2("s_pack_lh_b32_b16", 19, s(16), s(17));
            a.sop2("s_pack_hh_b32_b16", 20, s(16), s(17));
            a.st_s(18, 0);
            a.st_s(19, 1);
            a.st_s(20, 2);
        },
    ),
    case("s_bcnt0_i32_b32 and s_bcnt0_i32_b64", Cmp::Exact, |a| {
        a.smem("s_buffer_load_dwordx2", 16, 8, 0);
        a.sopp("s_waitcnt", 0);
        a.sop1("s_bcnt0_i32_b32", 18, s(16));
        a.sop1("s_bcnt0_i32_b64", 19, s(16));
        a.st_s(18, 0);
        a.st_s(19, 1);
    }),
    case(
        "s_absdiff_i32, s_bfe_u64 and s_ff0_i32_b32",
        Cmp::Exact,
        |a| {
            a.smem("s_buffer_load_dwordx2", 16, 8, 0);
            a.sopp("s_waitcnt", 0);
            a.sop2("s_absdiff_i32", 18, s(16), s(17));
            a.sop1("s_mov_b32", 19, lit(0x0014_0006));
            a.sop2("s_bfe_u64", 20, s(16), s(19));
            a.sop1("s_ff0_i32_b32", 22, s(16));
            for (row, r) in [18, 20, 21, 22].into_iter().enumerate() {
                a.st_s(r, row as u16);
            }
        },
    ),
    case("s_sext_i32_i8 and s_sext_i32_i16", Cmp::Exact, |a| {
        a.smem("s_buffer_load_dword", 16, 8, 0);
        a.sopp("s_waitcnt", 0);
        a.sop1("s_sext_i32_i8", 18, s(16));
        a.sop1("s_sext_i32_i16", 19, s(16));
        a.st_s(18, 0);
        a.st_s(19, 1);
    }),
    case("s_brev_b64", Cmp::Exact, |a| {
        a.smem("s_buffer_load_dwordx2", 16, 8, 0);
        a.sopp("s_waitcnt", 0);
        a.sop1("s_brev_b64", 18, s(16));
        a.st_s(18, 0);
        a.st_s(19, 1);
    }),
    case("the overflow s_addk_i32 leaves in SCC", Cmp::Exact, |a| {
        a.smem("s_buffer_load_dword", 16, 8, 0);
        a.sopp("s_waitcnt", 0);
        a.sopk("s_addk_i32", 16, 0x7FFF);
        a.sop2("s_cselect_b32", 17, c(1), c(0));
        a.st_s(17, 0);
    }),
    case("s_cmov_b32", Cmp::Exact, |a| {
        a.sopc("s_cmp_eq_u32", s(0), c(5));
        a.sopk("s_movk_i32", 20, 3);
        a.sop1("s_cmov_b32", 20, s(1));
        a.st_s(20, 0);
    }),
    case("s_cmovk_i32", Cmp::Exact, |a| {
        a.sopc("s_cmp_eq_u32", s(0), c(5));
        a.sopk("s_movk_i32", 20, 3);
        a.sopk("s_cmovk_i32", 20, 77);
        a.st_s(20, 0);
    }),
    case(
        "the SCC s_and_saveexec_b32 leaves, read as data",
        Cmp::Exact,
        |a| {
            a.vopc("v_cmp_gt_u32", c(8), 0);
            a.sop1("s_and_saveexec_b32", 20, VCC);
            a.sop2("s_cselect_b32", 21, c(1), c(0));
            a.sop1("s_mov_b32", 126, s(20));
            a.st_s(21, 0);
        },
    ),
];

// Miscompiles the corpus found, each a test to run with `--ignored`.

/// `s_cbranch_vccz`/`s_cbranch_vccnz` on a VCC that differs between the
/// lanes of a wave: the console decides once per wave (whether any lane's
/// bit is set), and every lane in EXEC follows. Only thread 63 sets VCC, so
/// the second wave (threads 32-63) does not take `vccz` and all its threads
/// run the `v_mov_b32 v5, 2` the branch would skip; the first wave takes it.
/// The translation decides per lane (`structure::structure` builds the
/// per-lane form, `Lanes::branch` gives each lane `VCC & EXEC` of its own),
/// so threads 32-62 keep 1. The same for `vccnz` in row 1. (Compilers
/// branch on VCC after uniform compares, where the two agree.)
const DIVERGENT_VCC: Case = case(
    "branches on a VCC that differs between lanes",
    Cmp::Exact,
    |a| {
        a.vop1("v_mov_b32", 5, c(1));
        a.vopc("v_cmp_eq_u32", c(63), 0);
        let skip = a.branch("s_cbranch_vccz");
        a.vop1("v_mov_b32", 5, c(2));
        a.bind(skip);
        a.st(5, 0);
        a.vop1("v_mov_b32", 6, c(1));
        a.vopc("v_cmp_eq_u32", c(5), 0);
        let taken = a.branch("s_cbranch_vccnz");
        a.vop1("v_mov_b32", 6, c(2));
        a.bind(taken);
        a.st(6, 1);
    },
);

#[test]
#[ignore = "miscompile: a branch on a VCC that differs between lanes is decided per lane"]
fn branches_on_a_divergent_vcc_are_decided_per_wave() {
    run_alone(&DIVERGENT_VCC);
}

/// `s_lshl1_add_u32`..`s_lshl4_add_u32` set SCC to the unsigned overflow
/// of the 64-bit `(S0 << n) + S1` (RDNA2 ISA). The lowering gives SCC
/// "the result is nonzero" (no carry case in `Lower`'s SALU match), so
/// rows 1, 3, 5 and 7 read 1 for every sum that does not overflow.
const SHIFT_ADD_SCC: Case = case("the carry of s_lshlN_add_u32", Cmp::Exact, |a| {
    a.smem("s_buffer_load_dwordx2", 16, 8, 0);
    a.sopp("s_waitcnt", 0);
    for (row, op) in [
        "s_lshl1_add_u32",
        "s_lshl2_add_u32",
        "s_lshl3_add_u32",
        "s_lshl4_add_u32",
    ]
    .into_iter()
    .enumerate()
    {
        let row = 2 * row as u16;
        a.sop2(op, 20, s(16), s(17));
        a.sop2("s_cselect_b32", 21, c(1), c(0));
        a.st_s(20, row);
        a.st_s(21, row + 1);
    }
    // No overflow, a nonzero sum: SCC 0.
    a.sop2("s_lshl1_add_u32", 22, c(3), c(4));
    a.sop2("s_cselect_b32", 23, c(1), c(0));
    a.st_s(23, 8);
});

#[test]
#[ignore = "miscompile: the SCC of s_lshlN_add_u32 is 'nonzero', not the carry"]
fn the_scc_of_a_shift_and_add_is_its_carry() {
    run_alone(&SHIFT_ADD_SCC);
}

/// `v_rndne_f32` of a value the host's compiler knows (an inline constant,
/// a literal, an SGPR or VGPR set to one) rounds halves away from zero on
/// the NVIDIA host: 0.5 to 1, -0.5 to -1, 2.5 to 3 (rows 0, 1, 2, 4, 6, 8;
/// the console gives 0, -0, 2). The translation emits DXIL's `Round_ne`
/// (op 26) and halves known only at run time round to even (the corpus's
/// "rounding and fractions" passes), so the driver's constant folding of
/// `Round_ne` is what is wrong; the translation could keep such constants
/// out of its sight. Found by the random programs (seeds 1271, 1508, 1891,
/// 2617 and 2851 from `--seed 1000`).
const KNOWN_ROUNDING: Case = case("rounding a known half to even", Cmp::Exact, |a| {
    a.ld(2, ANG);
    a.vop1("v_rndne_f32", 3, fk(0.5));
    a.vop1("v_rndne_f32", 4, fk(-0.5));
    a.vop1("v_rndne_f32", 5, lit(2.5f32.to_bits()));
    a.vop1("v_rndne_f32", 6, lit(1.5f32.to_bits()));
    a.sop1("s_mov_b32", 20, lit(0.5f32.to_bits()));
    a.vop1("v_rndne_f32", 7, s(20));
    a.vop1("v_mov_b32", 8, lit(0.5f32.to_bits()));
    a.vop1("v_rndne_f32", 9, v(8));
    a.vop2("v_and_b32", 10, c(0), 2);
    a.vop2("v_add_f32", 10, fk(0.5), 10); // 0.5 from a lane value
    a.vop1("v_rndne_f32", 11, v(10));
    a.vop1("v_floor_f32", 12, fk(-1.0));
    a.vop1("v_ceil_f32", 13, lit((-0.25f32).to_bits()));
    a.vop1("v_trunc_f32", 14, lit((-0.25f32).to_bits()));
    a.vop1("v_fract_f32", 15, fk(-1.0));
    for (row, r) in (3..16).enumerate() {
        a.st(r, row as u16);
    }
});

#[test]
#[ignore = "miscompile (host driver): Round_ne of a known half rounds away from zero"]
fn known_halves_round_to_even() {
    run_alone(&KNOWN_ROUNDING);
}

/// The console rounds every float operation; the host's compiler may
/// refactor float math in the translated DXIL, which leaves DXIL's
/// "disable math refactoring" shader flag (bit 1) clear:
/// - `v_fma_f32 1.0, x + 2^20, 2^20` comes out as `x + 2^21` rounded once
///   (row 2, 9 lanes 1 ULP up), and `v_fma_f32 1.0, -2^20, x + 2^20` as
///   `x` itself instead of `x` rounded to 1/8 (row 3; the `fma` is DXIL's
///   `FMad`, which the driver turns into an add and folds with the one
///   before it);
/// - `v_fract_f32` of `x - x` is -0 instead of +0 (row 5).
///
/// Rows 1 and 7 (the same through plain `v_add`/`v_sub`, and a fract of
/// +0) agree. Setting the flag (`flags: 1 << 1` where `shader.rs` builds
/// the module state) made every row agree on the RTX 3070, at a cost in
/// what the driver may optimise; left to the owner. Found by the random
/// programs (seeds 1037, 2042 and 2180 from `--seed 1000`).
const REFACTORED: Case = case("float math the host refactors", Cmp::Exact, |a| {
    a.ld(2, FLT);
    a.vop1("v_mov_b32", 3, lit(0x4980_0000)); // 2^20
    a.vop2("v_add_f32", 4, v(2), 3);
    a.vop2("v_sub_f32", 5, v(4), 3);
    a.vop3("v_fma_f32", 6, &[fk(1.0), v(4), v(3)]);
    a.vop3m("v_fma_f32", 7, &[fk(1.0), v(3), v(4)], 0, 2, false, 0);
    a.vop2("v_sub_f32", 8, v(2), 2);
    a.vop1("v_fract_f32", 9, v(8));
    a.vop1("v_mov_b32", 10, c(0));
    a.vop1("v_fract_f32", 11, v(10));
    for (row, r) in (4..12).enumerate() {
        a.st(r, row as u16);
    }
});

#[test]
#[ignore = "miscompile: the host refactors float math the console rounds step by step"]
fn float_math_rounds_every_step() {
    run_alone(&REFACTORED);
}

#[test]
fn refused_programs_stay_refused_or_match() {
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some(gpus) = Gpus::new() else {
        eprintln!("no SM 6.6 device; skipped");
        return;
    };
    let mut failures = Vec::new();
    for case in REFUSED {
        let code = program(case.body);
        match differ(
            &gpus,
            case.name,
            &code,
            case.wave64,
            case.threads,
            case.lds,
            case.cmp,
            0x5EED,
        ) {
            Outcome::Refused(what) => eprintln!("still refused: {what}"),
            Outcome::Equal => eprintln!("{}: translated now, and equal", case.name),
            Outcome::Unsupported(what) => failures.push(format!("{}: reference {what}", case.name)),
            Outcome::Failed(report) => failures.push(report),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn the_reference_interpreter_refuses_what_it_does_not_model() {
    let code = program(|a| a.vop1("v_cvt_f16_f32", 2, v(0)));
    let (input, k) = inputs(1);
    let got = interp::run(&Group {
        code: &code,
        wave64: false,
        threads: 32,
        lds_bytes: 0,
        constants: k,
        input: &input,
        output_dwords: OUT_DWORDS,
    });
    assert!(
        matches!(&got, Err(interp::Error::Unsupported(w)) if w.contains("v_cvt_f16_f32")),
        "{got:?}"
    );
}

#[test]
fn the_reference_interpreter_runs_the_corpus_by_itself() {
    // Without a GPU too: every case is covered, and the frame's store of
    // the thread index lands where the tests expect.
    for case in CORPUS {
        let code = program(case.body);
        let (input, k) = inputs(0x5EED);
        interp::run(&Group {
            code: &code,
            wave64: case.wave64,
            threads: case.threads,
            lds_bytes: case.lds,
            constants: k,
            input: &input,
            output_dwords: OUT_DWORDS,
        })
        .unwrap_or_else(|e| panic!("{}: {e}", case.name));
    }
    let code = program(|a| {
        a.vop2("v_add_nc_u32", 2, c(5), 0);
        a.st(2, 3);
    });
    let (input, k) = inputs(1);
    let out = interp::run(&Group {
        code: &code,
        wave64: false,
        threads: 64,
        lds_bytes: 0,
        constants: k,
        input: &input,
        output_dwords: OUT_DWORDS,
    })
    .unwrap();
    assert_eq!(out[3 * 64..4 * 64], (5..69).collect::<Vec<u32>>()[..]);
}

#[test]
fn translated_programs_match_the_reference_interpreter() {
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some(gpus) = Gpus::new() else {
        eprintln!("no SM 6.6 device; skipped");
        return;
    };
    assert!(CORPUS.len() >= 30);
    let only = std::env::var("VICTORY_DIFF_ONLY").ok();
    let failures: Vec<String> = CORPUS
        .iter()
        .filter(|c| only.as_ref().is_none_or(|o| o == c.name))
        .filter_map(|c| run_case(&gpus, c).err())
        .collect();
    assert!(
        failures.is_empty(),
        "{} of {} cases differ:\n{}",
        failures.len(),
        CORPUS.len(),
        failures.join("\n")
    );
}

// ---------------------------------------------------------------------------
// Random straight-line programs.

const INT_VOP2: &[&str] = &[
    "v_add_nc_u32",
    "v_sub_nc_u32",
    "v_subrev_nc_u32",
    "v_and_b32",
    "v_or_b32",
    "v_xor_b32",
    "v_xnor_b32",
    "v_lshlrev_b32",
    "v_lshrrev_b32",
    "v_ashrrev_i32",
    "v_min_i32",
    "v_max_i32",
    "v_min_u32",
    "v_max_u32",
    "v_mul_u32_u24",
    "v_mul_hi_u32_u24",
    "v_mul_i32_i24",
    "v_mul_hi_i32_i24",
];
const INT_VOP1: &[&str] = &[
    "v_mov_b32",
    "v_not_b32",
    "v_bfrev_b32",
    "v_ffbh_u32",
    "v_ffbl_b32",
    "v_ffbh_i32",
];
const INT_VOP3: &[&str] = &[
    "v_mul_lo_u32",
    "v_mul_hi_u32",
    "v_mul_hi_i32",
    "v_bfe_u32",
    "v_bfe_i32",
    "v_bfi_b32",
    "v_alignbit_b32",
    "v_alignbyte_b32",
    "v_min3_i32",
    "v_max3_u32",
    "v_med3_i32",
    "v_med3_u32",
    "v_mad_u32_u24",
    "v_mad_i32_i24",
    "v_add3_u32",
    "v_lshl_add_u32",
    "v_add_lshl_u32",
    "v_lshl_or_b32",
    "v_and_or_b32",
    "v_or3_b32",
    "v_xor3_b32",
    "v_xad_u32",
    "v_sad_u32",
    "v_perm_b32",
    "v_bfm_b32",
    "v_bcnt_u32_b32",
];
const INT_SOP2: &[&str] = &[
    "s_add_u32",
    "s_sub_u32",
    "s_addc_u32",
    "s_subb_u32",
    "s_add_i32",
    "s_sub_i32",
    "s_min_i32",
    "s_max_u32",
    "s_and_b32",
    "s_or_b32",
    "s_xor_b32",
    "s_andn2_b32",
    "s_nor_b32",
    "s_lshl_b32",
    "s_lshr_b32",
    "s_ashr_i32",
    "s_bfm_b32",
    "s_mul_i32",
    "s_mul_hi_u32",
    "s_bfe_u32",
    "s_bfe_i32",
    "s_cselect_b32",
];
const INT_SOP1: &[&str] = &[
    "s_mov_b32",
    "s_not_b32",
    "s_brev_b32",
    "s_bcnt1_i32_b32",
    "s_ff1_i32_b32",
    "s_flbit_i32_b32",
    "s_flbit_i32",
    "s_abs_i32",
];
const INT_CMP: &[&str] = &["lt", "eq", "le", "gt", "ne", "ge"];
const FLOAT_VOP2: &[&str] = &[
    "v_add_f32",
    "v_sub_f32",
    "v_subrev_f32",
    "v_mul_f32",
    "v_min_f32",
    "v_max_f32",
    "v_mul_legacy_f32",
];
const FLOAT_VOP1: &[&str] = &[
    "v_floor_f32",
    "v_ceil_f32",
    "v_trunc_f32",
    "v_rndne_f32",
    "v_fract_f32",
    "v_mov_b32",
];
const FLOAT_VOP3: &[&str] = &["v_fma_f32", "v_med3_f32", "v_min3_f32", "v_max3_f32"];
const FLOAT_CMP: &[&str] = &[
    "lt", "eq", "le", "gt", "lg", "ge", "o", "u", "nge", "nlg", "ngt", "nle", "neq", "nlt",
];

fn pick<'a>(r: &mut Rng, list: &[&'a str]) -> &'a str {
    list[r.below(list.len() as u32) as usize]
}

/// A random straight-line program over v2-v9 and s16-s23 (integer, or
/// float when `float`), storing them all at the end.
fn random_program(r: &mut Rng, float: bool) -> Vec<u32> {
    let n = 8 + r.below(24);
    let seed = r.next();
    program(move |a| {
        let mut r = Rng(seed);
        let rows: &[u16] = if float {
            &[
                FLT,
                FLT + ROW,
                FLT + 2 * ROW,
                FLT + 3 * ROW,
                ANG,
                ANG + ROW,
                ANG + 2 * ROW,
                ANG + 3 * ROW,
            ]
        } else {
            &[
                INT,
                INT + ROW,
                INT + 2 * ROW,
                INT + 3 * ROW,
                SMALL,
                SMALL + ROW,
                SMALL + 2 * ROW,
                SMALL + 3 * ROW,
            ]
        };
        load(a, 2, rows);
        let base = if float {
            u32::from(FLT)
        } else {
            u32::from(INT)
        };
        a.smem("s_buffer_load_dwordx4", 16, 8, base + 16 * r.below(16));
        a.smem(
            "s_buffer_load_dwordx4",
            20,
            8,
            u32::from(if float { ANG } else { SMALL }) + 16 * r.below(16),
        );
        a.sopp("s_waitcnt", 0);
        // SCC is read before any operation sets it otherwise.
        a.sopc("s_cmp_lt_u32", s(16), s(17));
        let vreg = |r: &mut Rng| 2 + r.below(8) as u8;
        let sreg = |r: &mut Rng| 16 + r.below(8) as u8;
        // A source: mostly VGPRs, else an SGPR, an inline constant or a literal.
        let src = |r: &mut Rng| match r.below(8) {
            0 => s(16 + r.below(8) as u8),
            1 if float => fk([0.5, -1.0, 2.0, -4.0][r.below(4) as usize]),
            1 => c(r.below(81) as i32 - 16),
            2 if float => lit(r.float(6).to_bits()),
            2 => lit(r.u32()),
            _ => v(2 + r.below(8) as u8),
        };
        for _ in 0..n {
            match (float, r.below(10)) {
                (false, 0..=2) => {
                    let op = pick(&mut r, INT_VOP2);
                    let (d, x, y) = (vreg(&mut r), src(&mut r), vreg(&mut r));
                    a.vop2(op, d, x, y);
                }
                (false, 3) => {
                    let op = pick(&mut r, INT_VOP1);
                    let (d, x) = (vreg(&mut r), src(&mut r));
                    a.vop1(op, d, x);
                }
                (false, 4..=5) => {
                    let op = pick(&mut r, INT_VOP3);
                    let two = matches!(
                        op,
                        "v_mul_lo_u32"
                            | "v_mul_hi_u32"
                            | "v_mul_hi_i32"
                            | "v_bfm_b32"
                            | "v_bcnt_u32_b32"
                    );
                    let d = vreg(&mut r);
                    // At most one scalar or literal source (the constant bus).
                    let x = src(&mut r);
                    let (y, z) = (v(vreg(&mut r)), v(vreg(&mut r)));
                    let srcs = if r.below(2) == 0 {
                        [x, y, z]
                    } else {
                        [y, z, x]
                    };
                    a.vop3(op, d, &srcs[..if two { 2 } else { 3 }]);
                }
                (false, 6) => {
                    let op = pick(&mut r, INT_SOP2);
                    let (d, x) = (sreg(&mut r), s(sreg(&mut r)));
                    let y = match r.below(3) {
                        0 => c(r.below(81) as i32 - 16),
                        1 => lit(r.u32()),
                        _ => s(sreg(&mut r)),
                    };
                    a.sop2(op, d, x, y);
                }
                (false, 7) => {
                    let op = pick(&mut r, INT_SOP1);
                    let (d, x) = (sreg(&mut r), s(sreg(&mut r)));
                    a.sop1(op, d, x);
                }
                (false, 8) => {
                    let cond = pick(&mut r, INT_CMP);
                    let ty = if r.below(2) == 0 { "i32" } else { "u32" };
                    let (x, y) = (src(&mut r), vreg(&mut r));
                    a.vopc(&format!("v_cmp_{cond}_{ty}"), x, y);
                    let (d, p, q) = (vreg(&mut r), src(&mut r), vreg(&mut r));
                    a.vop2("v_cndmask_b32", d, p, q);
                }
                (false, _) => {
                    let (d, x, y) = (vreg(&mut r), v(vreg(&mut r)), v(vreg(&mut r)));
                    a.vop3b("v_add_co_u32", d, 106, &[x, y]);
                    let (d, x, y) = (vreg(&mut r), src(&mut r), vreg(&mut r));
                    a.vop2("v_add_co_ci_u32", d, x, y);
                    if r.below(2) == 0 {
                        a.sopc("s_cmp_lt_u32", s(sreg(&mut r)), s(sreg(&mut r)));
                        let d = sreg(&mut r);
                        a.sop2("s_cselect_b32", d, s(sreg(&mut r)), s(sreg(&mut r)));
                    }
                }
                // (Clear of the two known miscompiles above: no `x - x`,
                // and no rounding of a value the host's compiler knows.)
                (true, 0..=3) => {
                    let op = pick(&mut r, FLOAT_VOP2);
                    let (d, x, mut y) = (vreg(&mut r), src(&mut r), vreg(&mut r));
                    if x.0 == 256 + u16::from(y) {
                        y = 2 + (y - 1) % 8;
                    }
                    a.vop2(op, d, x, y);
                }
                (true, 4..=5) => {
                    let op = pick(&mut r, FLOAT_VOP1);
                    let d = vreg(&mut r);
                    let x = if op == "v_mov_b32" {
                        src(&mut r)
                    } else {
                        v(vreg(&mut r))
                    };
                    a.vop1(op, d, x);
                }
                (true, 6..=7) => {
                    let op = pick(&mut r, FLOAT_VOP3);
                    let d = vreg(&mut r);
                    let x = src(&mut r);
                    let (y, z) = (v(vreg(&mut r)), v(vreg(&mut r)));
                    let (abs, neg) = (r.below(8) as u8 & r.below(8) as u8, r.below(8) as u8);
                    a.vop3m(op, d, &[x, y, z], abs, neg, r.below(8) == 0, 0);
                }
                (true, _) => {
                    let cond = pick(&mut r, FLOAT_CMP);
                    let (x, y) = (src(&mut r), vreg(&mut r));
                    a.vopc(&format!("v_cmp_{cond}_f32"), x, y);
                    let (d, p, q) = (vreg(&mut r), src(&mut r), vreg(&mut r));
                    a.vop2("v_cndmask_b32", d, p, q);
                }
            }
        }
        for i in 0..8u8 {
            a.st(2 + i, u16::from(i));
            a.st_s(16 + i, 8 + u16::from(i));
        }
    })
}

#[test]
fn random_programs_match_the_reference_interpreter() {
    let cases: u64 = std::env::var("VICTORY_DIFF_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let seed: u64 = std::env::var("VICTORY_DIFF_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    if cases == 0 {
        eprintln!("VICTORY_DIFF_CASES unset or 0: no random programs");
        return;
    }
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some(gpus) = Gpus::new() else {
        eprintln!("no SM 6.6 device; skipped");
        return;
    };
    let (mut equal, mut skipped, mut failures) = (0, 0, Vec::new());
    for i in 0..cases {
        let case_seed = seed.wrapping_add(i);
        let mut r = Rng(case_seed);
        let float = r.below(3) == 0;
        let code = random_program(&mut r, float);
        let name = format!("random program {case_seed}");
        let cmp = if float { Cmp::Float } else { Cmp::Exact };
        match differ(&gpus, &name, &code, false, 64, 0, cmp, case_seed) {
            Outcome::Equal => equal += 1,
            Outcome::Unsupported(what) | Outcome::Refused(what) => {
                eprintln!("{name}: skipped, {what}");
                skipped += 1;
            }
            Outcome::Failed(report) => {
                eprintln!("{report}");
                failures.push(case_seed);
            }
        }
    }
    eprintln!(
        "random programs from seed {seed}: {equal} equal, {skipped} skipped, {} differ{}",
        failures.len(),
        if failures.is_empty() {
            String::new()
        } else {
            format!(
                " (seeds {failures:?}; rerun one with VICTORY_DIFF_SEED=<seed> VICTORY_DIFF_CASES=1)"
            )
        }
    );
    assert!(
        failures.is_empty(),
        "{} random programs differ: seeds {failures:?}",
        failures.len()
    );
}
