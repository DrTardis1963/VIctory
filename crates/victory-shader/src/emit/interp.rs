//! A reference interpreter for synthetic GFX10 compute programs, lane by lane.
//!
//! It runs the decoded machine instructions directly on a model of the
//! hardware (SGPRs, VGPRs, EXEC, VCC, SCC, LDS, one input and one output
//! buffer) so that the differential test (`interp_tests.rs`) has a result to
//! hold the translated DXIL against that does not come from the IR, the
//! structurizer or the lowering. Only the decoder ([`crate::gfx10`]) is
//! shared with the code under test: it gives the operand fields, which are
//! plain bit slicing.
//!
//! Float rules follow RDNA2 where the existing GPU tests pin them down:
//! f32 denormals are flushed to signed zero (the console's default float
//! mode), minimums and maximums take the non-NaN operand and order -0 below
//! +0, `v_med3_f32` with NaNs takes the minimum of the rest, `v_fract_f32`
//! stays below 1.0, the legacy multiply gives +0 for a zero operand,
//! float-to-integer conversions saturate (NaN to 0), and `v_sin`/`v_cos`
//! take turns. Anything outside the covered subset is refused with
//! [`Error::Unsupported`], so that a test can skip it.
//!
//! The memory model is the differential test's: user SGPRs s0-s3 hold root
//! constants and s4 the address (0) of a descriptor table whose first entry
//! (byte 0) is a raw input buffer and whose second (byte 32) is a raw output
//! buffer; v0 holds the thread index. Accesses past a buffer's end read 0
//! and write nothing, as D3D12 raw views do.

use std::collections::HashMap;
use std::fmt;

use crate::gfx10::{self, Encoding, Fields, Instruction, NULL, Operand, Valu};

/// The input buffer's base address, as its fabricated descriptor holds it.
const SRV_BASE: u32 = 0x1000_0000;
/// The output buffer's base address.
const UAV_BASE: u32 = 0x2000_0000;
/// Instructions one wave may run before the program is taken to hang.
const STEP_LIMIT: usize = 1 << 20;

const VCC: u8 = 106;
const EXEC: u8 = 126;

/// A dispatch of one thread group.
pub struct Group<'a> {
    pub code: &'a [u32],
    pub wave64: bool,
    pub threads: u32,
    pub lds_bytes: u32,
    pub constants: [u32; 4],
    pub input: &'a [u32],
    pub output_dwords: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// An opcode or form outside the interpreter's subset.
    Unsupported(String),
    /// The program did something the model has no answer for.
    Fault(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Unsupported(what) => write!(f, "unsupported: {what}"),
            Error::Fault(what) => write!(f, "fault: {what}"),
        }
    }
}

fn unsupported<T>(what: impl Into<String>) -> Result<T, Error> {
    Err(Error::Unsupported(what.into()))
}

fn fault<T>(what: impl Into<String>) -> Result<T, Error> {
    Err(Error::Fault(what.into()))
}

/// Runs the group and returns the output buffer.
pub fn run(g: &Group) -> Result<Vec<u32>, Error> {
    let bytes: Vec<u8> = g.code.iter().flat_map(|w| w.to_le_bytes()).collect();
    let insts = gfx10::decode(&bytes).map_err(|e| Error::Fault(e.to_string()))?;
    let at: HashMap<usize, usize> = insts
        .iter()
        .enumerate()
        .map(|(i, x)| (x.offset, i))
        .collect();
    let lanes = if g.wave64 { 64 } else { 32 };
    let mut mem = Memory {
        input: g.input,
        output: vec![0; g.output_dwords],
        lds: vec![0; g.lds_bytes.div_ceil(4) as usize],
    };
    let mut waves: Vec<Wave> = (0..g.threads.div_ceil(lanes as u32))
        .map(|w| Wave::new(g, lanes, w * lanes as u32))
        .collect();
    loop {
        for wave in waves.iter_mut().filter(|w| w.state == State::Running) {
            let mut steps = 0;
            while wave.state == State::Running {
                steps += 1;
                if steps > STEP_LIMIT {
                    return fault("the program runs too long (a loop that never ends?)");
                }
                let Some(&i) = at.get(&wave.pc) else {
                    return fault(format!("no instruction at {:#x}", wave.pc));
                };
                let inst = &insts[i];
                wave.pc = inst.offset + 4 * inst.words().len();
                wave.step(inst, &mut mem).map_err(|e| match e {
                    Error::Unsupported(w) => Error::Unsupported(format!(
                        "{w} (in `{}` at {:#x})",
                        inst.text(g.wave64),
                        inst.offset
                    )),
                    Error::Fault(w) => Error::Fault(format!(
                        "{w} (in `{}` at {:#x})",
                        inst.text(g.wave64),
                        inst.offset
                    )),
                })?;
            }
        }
        if waves.iter().all(|w| w.state == State::Done) {
            return Ok(mem.output);
        }
        // Every wave left waits at a barrier: release them.
        for w in waves.iter_mut().filter(|w| w.state == State::Barrier) {
            w.state = State::Running;
        }
    }
}

struct Memory<'a> {
    input: &'a [u32],
    output: Vec<u32>,
    lds: Vec<u32>,
}

/// Which buffer a descriptor names, and the byte its records start at.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Buf {
    Input,
    Output,
}

impl Memory<'_> {
    /// The descriptor table: the input's descriptor at byte 0, the output's
    /// at byte 32 (base, stride 0, size in bytes, format bits unused).
    fn table(&self, dword: u64) -> u32 {
        match dword {
            0 => SRV_BASE,
            2 => 4 * self.input.len() as u32,
            8 => UAV_BASE,
            10 => 4 * self.output.len() as u32,
            _ => 0,
        }
    }

    fn buffer(desc: [u32; 4]) -> Result<(Buf, u64), Error> {
        if desc[1] & 0x3FFF_0000 != 0 {
            return unsupported("a buffer descriptor with a stride");
        }
        match desc[0] & 0xF000_0000 {
            SRV_BASE => Ok((Buf::Input, u64::from(desc[0] & 0x0FFF_FFFF))),
            UAV_BASE => Ok((Buf::Output, u64::from(desc[0] & 0x0FFF_FFFF))),
            _ => fault(format!("a descriptor naming no buffer ({desc:08x?})")),
        }
    }

    fn words(&self, b: Buf) -> &[u32] {
        match b {
            Buf::Input => self.input,
            Buf::Output => &self.output,
        }
    }

    /// The dword at byte `at` (a multiple of 4) of the buffer, 0 past its end
    /// or past the descriptor's record count.
    fn load(&self, desc: [u32; 4], at: u64) -> Result<u32, Error> {
        let (b, base) = Self::buffer(desc)?;
        if at + 4 > u64::from(desc[2]) {
            return Ok(0);
        }
        Ok(self
            .words(b)
            .get(((base + at) / 4) as usize)
            .copied()
            .unwrap_or(0))
    }

    /// Writes `bytes` (1, 2 or 4) of `value` at byte `at` of the buffer.
    fn store(&mut self, desc: [u32; 4], at: u64, value: u32, bytes: u32) -> Result<(), Error> {
        let (b, base) = Self::buffer(desc)?;
        if b == Buf::Input {
            return fault("a store to the input buffer (a read-only view)");
        }
        if at + u64::from(bytes) > u64::from(desc[2]) {
            return Ok(());
        }
        let at = base + at;
        let Some(w) = self.output.get_mut((at / 4) as usize) else {
            return Ok(());
        };
        let shift = 8 * (at % 4) as u32;
        let mask = if bytes == 4 {
            u32::MAX
        } else {
            ((1u32 << (8 * bytes)) - 1) << shift
        };
        *w = *w & !mask | (value << shift) & mask;
        Ok(())
    }

    fn lds_read(&self, at: u32, bytes: u32) -> u32 {
        let Some(&w) = self.lds.get((at / 4) as usize) else {
            return 0;
        };
        if bytes == 4 {
            return w;
        }
        (w >> (8 * (at % 4))) & ((1u32 << (8 * bytes)) - 1)
    }

    fn lds_write(&mut self, at: u32, value: u32, bytes: u32) {
        let Some(w) = self.lds.get_mut((at / 4) as usize) else {
            return;
        };
        let shift = 8 * (at % 4);
        let mask = if bytes == 4 {
            u32::MAX
        } else {
            ((1u32 << (8 * bytes)) - 1) << shift
        };
        *w = *w & !mask | (value << shift) & mask;
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Running,
    Barrier,
    Done,
}

struct Wave {
    lanes: usize,
    s: [u32; 128],
    v: Vec<[u32; 64]>,
    scc: bool,
    pc: usize,
    state: State,
}

impl Wave {
    fn new(g: &Group, lanes: usize, first: u32) -> Self {
        let mut w = Wave {
            lanes,
            s: [0; 128],
            v: vec![[0; 64]; 256],
            scc: false,
            pc: 0,
            state: State::Running,
        };
        w.s[..4].copy_from_slice(&g.constants);
        let live = (g.threads - first).min(lanes as u32);
        let exec = if live == 64 {
            u64::MAX
        } else {
            (1u64 << live) - 1
        };
        w.s[EXEC as usize] = exec as u32;
        w.s[EXEC as usize + 1] = (exec >> 32) as u32;
        for lane in 0..lanes {
            w.v[0][lane] = first + lane as u32;
        }
        w
    }

    fn lane_mask(&self) -> u64 {
        if self.lanes == 64 {
            u64::MAX
        } else {
            u64::from(u32::MAX)
        }
    }

    /// A lane mask held at `code` (and `code + 1` with 64 lanes).
    fn mask_at(&self, code: u8) -> u64 {
        let lo = u64::from(self.sreg(code));
        if self.lanes == 64 {
            lo | u64::from(self.sreg(code + 1)) << 32
        } else {
            lo
        }
    }

    fn set_mask_at(&mut self, code: u8, m: u64) {
        self.set_sreg(code, m as u32);
        if self.lanes == 64 {
            self.set_sreg(code + 1, (m >> 32) as u32);
        }
    }

    fn exec(&self) -> u64 {
        self.mask_at(EXEC)
    }

    fn active(&self, lane: usize) -> bool {
        self.exec() >> lane & 1 != 0
    }

    fn sreg(&self, code: u8) -> u32 {
        match code {
            NULL => 0,
            0..=127 => self.s[usize::from(code)],
            _ => 0,
        }
    }

    fn set_sreg(&mut self, code: u8, value: u32) {
        if code < 128 && code != NULL {
            self.s[usize::from(code)] = value;
        }
    }

    /// An inline constant as 32 bits (floats as their f32 bits).
    fn inline32(&self, c: u8) -> Result<u32, Error> {
        Ok(match c {
            128..=192 => u32::from(c - 128),
            193..=208 => (-(i32::from(c) - 192)) as u32,
            240 => 0.5f32.to_bits(),
            241 => (-0.5f32).to_bits(),
            242 => 1.0f32.to_bits(),
            243 => (-1.0f32).to_bits(),
            244 => 2.0f32.to_bits(),
            245 => (-2.0f32).to_bits(),
            246 => 4.0f32.to_bits(),
            247 => (-4.0f32).to_bits(),
            248 => 0x3E22_F983,
            251 => u32::from(self.mask_at(VCC) & self.lane_mask() == 0),
            252 => u32::from(self.exec() == 0),
            253 => u32::from(self.scc),
            _ => return unsupported(format!("source operand code {c}")),
        })
    }

    /// A 32-bit source in `lane`.
    fn rd(&self, op: &Operand, lane: usize) -> Result<u32, Error> {
        Ok(match *op {
            Operand::Scalar { code, .. } => self.sreg(code),
            Operand::Vector { reg, .. } => self.v[usize::from(reg)][lane],
            Operand::Const(c) => self.inline32(c)?,
            Operand::Literal(l) => l,
            Operand::Attr { .. } => return unsupported("an interpolation attribute"),
        })
    }

    /// A 64-bit source in `lane`: a register pair, or an integer inline
    /// constant sign-extended.
    fn rd64(&self, op: &Operand, lane: usize) -> Result<u64, Error> {
        Ok(match *op {
            Operand::Scalar { code, .. } => {
                u64::from(self.sreg(code)) | u64::from(self.sreg(code.wrapping_add(1))) << 32
            }
            Operand::Vector { reg, .. } => {
                let r = usize::from(reg);
                u64::from(self.v[r][lane]) | u64::from(self.v[r + 1][lane]) << 32
            }
            Operand::Const(c @ 128..=208) => i64::from(self.inline32(c)? as i32) as u64,
            Operand::Const(c @ 251..=253) => u64::from(self.inline32(c)?),
            _ => return unsupported("a literal or float constant as a 64-bit operand"),
        })
    }

    /// A scalar source of its own size: register pairs as 64 bits, integer
    /// inline constants sign-extended (32-bit operations use the low half).
    /// `wide`: the operation is a 64-bit one, where literals are refused.
    fn srd(&self, op: &Operand, wide: bool) -> Result<u64, Error> {
        match *op {
            Operand::Scalar { dwords: 2, .. } | Operand::Const(128..=208) => self.rd64(op, 0),
            Operand::Literal(_) | Operand::Const(240..=248) if wide => {
                unsupported("a literal or float constant in a 64-bit scalar operation")
            }
            _ => Ok(u64::from(self.rd(op, 0)?)),
        }
    }

    fn swr(&mut self, op: &Operand, value: u64) -> Result<(), Error> {
        let Operand::Scalar { code, dwords } = *op else {
            return fault("a scalar destination that is no SGPR");
        };
        self.set_sreg(code, value as u32);
        if dwords == 2 {
            self.set_sreg(code + 1, (value >> 32) as u32);
        }
        Ok(())
    }

    fn vwr(&mut self, op: &Operand, lane: usize, value: u32) -> Result<(), Error> {
        let Operand::Vector { reg, .. } = *op else {
            return fault("a vector destination that is no VGPR");
        };
        self.v[usize::from(reg)][lane] = value;
        Ok(())
    }

    fn vwr64(&mut self, op: &Operand, lane: usize, value: u64) -> Result<(), Error> {
        let Operand::Vector { reg, .. } = *op else {
            return fault("a vector destination that is no VGPR");
        };
        self.v[usize::from(reg)][lane] = value as u32;
        self.v[usize::from(reg) + 1][lane] = (value >> 32) as u32;
        Ok(())
    }

    fn step(&mut self, inst: &Instruction, mem: &mut Memory) -> Result<(), Error> {
        let name = inst.name().unwrap_or("?");
        let wave64 = self.lanes == 64;
        match (inst.encoding, inst.fields(wave64)) {
            (Encoding::Sopp, Fields::Sopp { imm }) => self.sopp(name, imm),
            (Encoding::Sopk, Fields::Sop { dst, src, imm }) => {
                self.sopk(name, dst, &src, imm.unwrap_or(0))
            }
            (Encoding::Sop1 | Encoding::Sop2 | Encoding::Sopc, Fields::Sop { dst, src, .. }) => {
                self.salu(name, dst, &src)
            }
            (
                Encoding::Smem,
                Fields::Smem {
                    data,
                    base,
                    offset,
                    soffset,
                    ..
                },
            ) => self.smem(name, data, base, offset, soffset, mem),
            (_, Fields::Valu(v)) => self.valu(inst, name, &v),
            (
                Encoding::Ds,
                Fields::Ds {
                    dst,
                    addr,
                    data,
                    offset0,
                    offset1,
                    gds,
                },
            ) => {
                if gds {
                    return unsupported("GDS");
                }
                self.ds(name, dst, addr, &data, offset0, offset1, mem)
            }
            (Encoding::Mubuf, Fields::Buffer { .. }) => self.mubuf(name, inst, mem),
            _ => unsupported(format!("opcode {name}")),
        }
    }

    fn sopp(&mut self, name: &str, imm: u16) -> Result<(), Error> {
        let taken = match name {
            "s_endpgm" => {
                self.state = State::Done;
                return Ok(());
            }
            "s_barrier" => {
                self.state = State::Barrier;
                return Ok(());
            }
            "s_nop" | "s_waitcnt" | "s_waitcnt_depctr" | "s_inst_prefetch" | "s_clause"
            | "s_setprio" | "s_sleep" => return Ok(()),
            "s_branch" => true,
            "s_cbranch_scc0" => !self.scc,
            "s_cbranch_scc1" => self.scc,
            "s_cbranch_vccz" => self.mask_at(VCC) & self.lane_mask() == 0,
            "s_cbranch_vccnz" => self.mask_at(VCC) & self.lane_mask() != 0,
            "s_cbranch_execz" => self.exec() == 0,
            "s_cbranch_execnz" => self.exec() != 0,
            _ => return unsupported(format!("opcode {name}")),
        };
        if taken {
            self.pc = (self.pc as i64 + 4 * i64::from(imm as i16)) as usize;
        }
        Ok(())
    }

    fn sopk(
        &mut self,
        name: &str,
        dst: Option<Operand>,
        src: &[Operand],
        imm: u16,
    ) -> Result<(), Error> {
        let simm = i32::from(imm as i16);
        if name.starts_with("s_waitcnt_") {
            return Ok(());
        }
        if let Some(cond) = name.strip_prefix("s_cmpk_") {
            let a = self.rd(&src[0], 0)?;
            let (c, ty) = cond.split_once('_').unwrap_or((cond, ""));
            let b = if ty == "i32" {
                simm as u32
            } else {
                u32::from(imm)
            };
            self.scc = int_cmp(c, ty == "i32", u64::from(a), u64::from(b), 32)
                .ok_or_else(|| Error::Unsupported(format!("opcode {name}")))?;
            return Ok(());
        }
        let Some(d) = dst else {
            return unsupported(format!("opcode {name}"));
        };
        let old = self.rd(&d, 0)?;
        let value = match name {
            "s_movk_i32" => simm as u32,
            "s_cmovk_i32" => {
                if self.scc {
                    simm as u32
                } else {
                    old
                }
            }
            "s_addk_i32" => {
                let (r, o) = (old as i32).overflowing_add(simm);
                self.scc = o;
                r as u32
            }
            "s_mulk_i32" => (old as i32).wrapping_mul(simm) as u32,
            _ => return unsupported(format!("opcode {name}")),
        };
        self.swr(&d, u64::from(value))
    }

    fn salu(&mut self, name: &str, dst: Option<Operand>, src: &[Operand]) -> Result<(), Error> {
        let wide = name.ends_with("_b64") || name.ends_with("_u64") || name.ends_with("_i64");
        let a = match src.first() {
            Some(op) => self.srd(op, wide)?,
            None => 0,
        };
        let b = match src.get(1) {
            Some(op) => self.srd(op, wide)?,
            None => 0,
        };
        let (a32, b32) = (a as u32, b as u32);
        // Results whose SCC is "the result is not zero".
        let nz = |s: &mut Self, r: u64| {
            s.scc = r != 0;
            r
        };
        if let Some(rest) = name.strip_prefix("s_cmp_") {
            let (c, ty) = rest.split_once('_').unwrap_or((rest, ""));
            let bits = if ty.ends_with("64") { 64 } else { 32 };
            self.scc = int_cmp(c, ty.starts_with('i'), a, b, bits)
                .ok_or_else(|| Error::Unsupported(format!("opcode {name}")))?;
            return Ok(());
        }
        if let Some(rest) = name.strip_prefix("s_bitcmp") {
            let bit = if rest.ends_with("b64") {
                a >> (b & 63) & 1
            } else {
                a >> (b & 31) & 1
            };
            self.scc = (bit == 1) == rest.starts_with('1');
            return Ok(());
        }
        if name.contains("saveexec") || name.contains("wrexec") {
            let exec = self.exec();
            let s0 = a;
            let op = name
                .trim_start_matches("s_")
                .split('_')
                .next()
                .unwrap_or("");
            let new = match op {
                "and" => s0 & exec,
                "or" => s0 | exec,
                "xor" => s0 ^ exec,
                "andn1" => !s0 & exec,
                "orn1" => !s0 | exec,
                "andn2" => s0 & !exec,
                "orn2" => s0 | !exec,
                "nand" => !(s0 & exec),
                "nor" => !(s0 | exec),
                "xnor" => !(s0 ^ exec),
                _ => return unsupported(format!("opcode {name}")),
            } & self.lane_mask();
            self.set_mask_at(EXEC, new);
            self.scc = new != 0;
            let saved = if name.contains("wrexec") { new } else { exec };
            if let Some(d) = dst {
                self.swr(&d, saved)?;
            }
            return Ok(());
        }
        let Some(d) = dst else {
            return unsupported(format!("opcode {name}"));
        };
        let mask64 = |w: bool| if w { u64::MAX } else { u64::from(u32::MAX) };
        let m = mask64(wide);
        let r: u64 = match name {
            "s_add_u32" => {
                let r = a32 as u64 + b32 as u64;
                self.scc = r >> 32 != 0;
                r & 0xFFFF_FFFF
            }
            "s_addc_u32" => {
                let r = a32 as u64 + b32 as u64 + u64::from(self.scc);
                self.scc = r >> 32 != 0;
                r & 0xFFFF_FFFF
            }
            "s_sub_u32" => {
                self.scc = b32 > a32;
                u64::from(a32.wrapping_sub(b32))
            }
            "s_subb_u32" => {
                let borrow = u64::from(self.scc);
                self.scc = u64::from(b32) + borrow > u64::from(a32);
                u64::from(a32.wrapping_sub(b32).wrapping_sub(borrow as u32))
            }
            "s_add_i32" => {
                let (r, o) = (a32 as i32).overflowing_add(b32 as i32);
                self.scc = o;
                u64::from(r as u32)
            }
            "s_sub_i32" => {
                let (r, o) = (a32 as i32).overflowing_sub(b32 as i32);
                self.scc = o;
                u64::from(r as u32)
            }
            "s_min_i32" => {
                self.scc = (a32 as i32) < (b32 as i32);
                u64::from(if self.scc { a32 } else { b32 })
            }
            "s_min_u32" => {
                self.scc = a32 < b32;
                u64::from(if self.scc { a32 } else { b32 })
            }
            "s_max_i32" => {
                self.scc = (a32 as i32) > (b32 as i32);
                u64::from(if self.scc { a32 } else { b32 })
            }
            "s_max_u32" => {
                self.scc = a32 > b32;
                u64::from(if self.scc { a32 } else { b32 })
            }
            "s_cselect_b32" | "s_cselect_b64" => {
                if self.scc {
                    a
                } else {
                    b
                }
            }
            "s_and_b32" | "s_and_b64" => nz(self, a & b & m),
            "s_or_b32" | "s_or_b64" => nz(self, (a | b) & m),
            "s_xor_b32" | "s_xor_b64" => nz(self, (a ^ b) & m),
            "s_andn2_b32" | "s_andn2_b64" => nz(self, a & !b & m),
            "s_orn2_b32" | "s_orn2_b64" => nz(self, (a | !b) & m),
            "s_nand_b32" | "s_nand_b64" => nz(self, !(a & b) & m),
            "s_nor_b32" | "s_nor_b64" => nz(self, !(a | b) & m),
            "s_xnor_b32" | "s_xnor_b64" => nz(self, !(a ^ b) & m),
            "s_lshl_b32" => nz(self, u64::from(a32 << (b32 & 31))),
            "s_lshl_b64" => nz(self, a << (b & 63)),
            "s_lshr_b32" => nz(self, u64::from(a32 >> (b32 & 31))),
            "s_lshr_b64" => nz(self, a >> (b & 63)),
            "s_ashr_i32" => nz(self, u64::from(((a32 as i32) >> (b32 & 31)) as u32)),
            "s_ashr_i64" => nz(self, ((a as i64) >> (b & 63)) as u64),
            "s_bfm_b32" => u64::from(bfm(a32, b32)),
            "s_bfm_b64" => {
                let w = a & 63;
                let ones = if w == 0 { 0 } else { u64::MAX >> (64 - w) };
                ones << (b & 63)
            }
            "s_mul_i32" => u64::from(a32.wrapping_mul(b32)),
            "s_mul_hi_u32" => (a32 as u64 * b32 as u64) >> 32,
            "s_mul_hi_i32" => u64::from((((a32 as i32 as i64) * (b32 as i32 as i64)) >> 32) as u32),
            "s_bfe_u32" => nz(self, u64::from(bfe_u(a32, b32 & 31, (b32 >> 16) & 0x7F))),
            "s_bfe_i32" => nz(self, u64::from(bfe_i(a32, b32 & 31, (b32 >> 16) & 0x7F))),
            "s_bfe_u64" | "s_bfe_i64" => {
                let (off, w) = (b32 & 63, (b32 >> 16) & 0x7F);
                let r = if w == 0 {
                    0
                } else if name == "s_bfe_u64" {
                    let x = a >> off;
                    if w >= 64 { x } else { x & ((1u64 << w) - 1) }
                } else {
                    let x = (a as i64 >> off) as u64;
                    if w >= 64 {
                        x
                    } else {
                        ((x << (64 - w)) as i64 >> (64 - w)) as u64
                    }
                };
                nz(self, r)
            }
            "s_absdiff_i32" => nz(
                self,
                u64::from((a32 as i32).wrapping_sub(b32 as i32).unsigned_abs()),
            ),
            "s_lshl1_add_u32" | "s_lshl2_add_u32" | "s_lshl3_add_u32" | "s_lshl4_add_u32" => {
                let n = u32::from(name.as_bytes()[6] - b'0');
                let r = (u64::from(a32) << n) + u64::from(b32);
                self.scc = r >> 32 != 0;
                r & 0xFFFF_FFFF
            }
            "s_pack_ll_b32_b16" => u64::from(a32 & 0xFFFF | b32 << 16),
            "s_pack_lh_b32_b16" => u64::from(a32 & 0xFFFF | b32 & 0xFFFF_0000),
            "s_pack_hh_b32_b16" => u64::from(a32 >> 16 | b32 & 0xFFFF_0000),
            // SOP1.
            "s_mov_b32" | "s_mov_b64" => a,
            "s_cmov_b32" | "s_cmov_b64" => {
                if self.scc {
                    a
                } else {
                    self.srd(&d, wide)?
                }
            }
            "s_not_b32" | "s_not_b64" => nz(self, !a & m),
            "s_brev_b32" => u64::from(a32.reverse_bits()),
            "s_brev_b64" => a.reverse_bits(),
            "s_bcnt0_i32_b32" => nz(self, u64::from(a32.count_zeros())),
            "s_bcnt0_i32_b64" => nz(self, u64::from(a.count_zeros())),
            "s_bcnt1_i32_b32" => nz(self, u64::from(a32.count_ones())),
            "s_bcnt1_i32_b64" => nz(self, u64::from(a.count_ones())),
            "s_ff0_i32_b32" => u64::from(first(!a32 != 0, (!a32).trailing_zeros())),
            "s_ff0_i32_b64" => u64::from(first(!a != 0, (!a).trailing_zeros())),
            "s_ff1_i32_b32" => u64::from(first(a32 != 0, a32.trailing_zeros())),
            "s_ff1_i32_b64" => u64::from(first(a != 0, a.trailing_zeros())),
            "s_flbit_i32_b32" => u64::from(first(a32 != 0, a32.leading_zeros())),
            "s_flbit_i32_b64" => u64::from(first(a != 0, a.leading_zeros())),
            "s_flbit_i32" => u64::from(flbit_i32(a32)),
            "s_flbit_i32_i64" => {
                let x = if (a as i64) < 0 { !a } else { a };
                u64::from(first(x != 0, x.leading_zeros()))
            }
            "s_sext_i32_i8" => u64::from(a32 as i8 as i32 as u32),
            "s_sext_i32_i16" => u64::from(a32 as i16 as i32 as u32),
            "s_bitset0_b32" | "s_bitset1_b32" | "s_bitset0_b64" | "s_bitset1_b64" => {
                let old = self.srd(&d, wide)?;
                let bit = 1u64 << (a & if wide { 63 } else { 31 });
                if name.starts_with("s_bitset1") {
                    old | bit
                } else {
                    old & !bit
                }
            }
            "s_abs_i32" => nz(self, u64::from((a32 as i32).unsigned_abs())),
            "s_quadmask_b32" | "s_quadmask_b64" => {
                let quads = if wide { 16 } else { 8 };
                let r = (0..quads).fold(0u64, |r, q| r | u64::from(a >> (4 * q) & 15 != 0) << q);
                nz(self, r)
            }
            "s_bitreplicate_b64_b32" => {
                (0..32).fold(0u64, |r, i| r | (u64::from(a32 >> i & 1) * 3) << (2 * i))
            }
            _ => return unsupported(format!("opcode {name}")),
        };
        self.swr(&d, r)
    }

    fn smem(
        &mut self,
        name: &str,
        data: Option<Operand>,
        base: Operand,
        offset: i32,
        soffset: Operand,
        mem: &Memory,
    ) -> Result<(), Error> {
        let Some(Operand::Scalar { code, dwords: n }) = data else {
            return unsupported(format!("opcode {name}"));
        };
        let off = (i64::from(offset) + i64::from(self.rd(&soffset, 0)?)) as u64;
        if name.starts_with("s_load_dword") {
            let addr = self.rd64(&base, 0)?.wrapping_add(off);
            if addr % 4 != 0 || addr + 4 * u64::from(n) > 64 {
                return unsupported("a scalar load from memory other than the descriptor table");
            }
            for i in 0..n {
                let value = mem.table(addr / 4 + u64::from(i));
                self.set_sreg(code + i, value);
            }
            return Ok(());
        }
        if name.starts_with("s_buffer_load_dword") {
            let Operand::Scalar { code: b, .. } = base else {
                return fault("a buffer descriptor that is no SGPR range");
            };
            let desc = [
                self.sreg(b),
                self.sreg(b + 1),
                self.sreg(b + 2),
                self.sreg(b + 3),
            ];
            for i in 0..n {
                let value = mem.load(desc, (off & !3) + 4 * u64::from(i))?;
                self.set_sreg(code + i, value);
            }
            return Ok(());
        }
        unsupported(format!("opcode {name}"))
    }

    #[allow(clippy::too_many_arguments)]
    fn ds(
        &mut self,
        name: &str,
        dst: Option<Operand>,
        addr: Option<Operand>,
        data: &[Operand],
        offset0: u8,
        offset1: u8,
        mem: &mut Memory,
    ) -> Result<(), Error> {
        let Some(addr) = addr else {
            return unsupported(format!("opcode {name}"));
        };
        let offset = u32::from(offset0) | u32::from(offset1) << 8;
        // Lanes run in order, as the hardware's LDS unit does for one wave.
        for lane in 0..self.lanes {
            if !self.active(lane) {
                continue;
            }
            let base = self.rd(&addr, lane)?;
            let d0 = match data.first() {
                Some(op) => self.rd(op, lane)?,
                None => 0,
            };
            let d0_hi = match data.first() {
                Some(Operand::Vector { reg, .. }) => self.v[usize::from(*reg) + 1][lane],
                _ => 0,
            };
            let d1 = match data.get(1) {
                Some(op) => self.rd(op, lane)?,
                None => 0,
            };
            let at = base.wrapping_add(offset);
            match name {
                "ds_write_b32" => mem.lds_write(at, d0, 4),
                "ds_write_b16" => mem.lds_write(at, d0, 2),
                "ds_write_b8" => mem.lds_write(at, d0, 1),
                "ds_write_b64" => {
                    mem.lds_write(at, d0, 4);
                    mem.lds_write(at.wrapping_add(4), d0_hi, 4);
                }
                "ds_write2_b32" | "ds_write2st64_b32" => {
                    let unit = if name.contains("st64") { 256 } else { 4 };
                    mem.lds_write(base.wrapping_add(unit * u32::from(offset0)), d0, 4);
                    mem.lds_write(base.wrapping_add(unit * u32::from(offset1)), d1, 4);
                }
                "ds_read_b32" | "ds_read_u16" | "ds_read_i16" | "ds_read_u8" | "ds_read_i8" => {
                    let r = match name {
                        "ds_read_b32" => mem.lds_read(at, 4),
                        "ds_read_u16" => mem.lds_read(at, 2),
                        "ds_read_i16" => mem.lds_read(at, 2) as u16 as i16 as i32 as u32,
                        "ds_read_u8" => mem.lds_read(at, 1),
                        _ => mem.lds_read(at, 1) as u8 as i8 as i32 as u32,
                    };
                    self.vwr(&dst.unwrap(), lane, r)?;
                }
                "ds_read_b64" => {
                    let lo = u64::from(mem.lds_read(at, 4));
                    let hi = u64::from(mem.lds_read(at.wrapping_add(4), 4));
                    self.vwr64(&dst.unwrap(), lane, lo | hi << 32)?;
                }
                "ds_read2_b32" | "ds_read2st64_b32" => {
                    let unit = if name.contains("st64") { 256 } else { 4 };
                    let lo =
                        u64::from(mem.lds_read(base.wrapping_add(unit * u32::from(offset0)), 4));
                    let hi =
                        u64::from(mem.lds_read(base.wrapping_add(unit * u32::from(offset1)), 4));
                    self.vwr64(&dst.unwrap(), lane, lo | hi << 32)?;
                }
                _ => {
                    // Read-modify-write atomics, with or without the old value.
                    let op = name
                        .trim_start_matches("ds_")
                        .trim_end_matches("_u32")
                        .trim_end_matches("_i32")
                        .trim_end_matches("_b32")
                        .trim_end_matches("_rtn");
                    let old = mem.lds_read(at, 4);
                    let new = match op {
                        "add" => old.wrapping_add(d0),
                        "sub" => old.wrapping_sub(d0),
                        "rsub" => d0.wrapping_sub(old),
                        "inc" => {
                            if old >= d0 {
                                0
                            } else {
                                old + 1
                            }
                        }
                        "dec" => {
                            if old == 0 || old > d0 {
                                d0
                            } else {
                                old - 1
                            }
                        }
                        "min" if name.ends_with("i32") => (old as i32).min(d0 as i32) as u32,
                        "max" if name.ends_with("i32") => (old as i32).max(d0 as i32) as u32,
                        "min" => old.min(d0),
                        "max" => old.max(d0),
                        "and" => old & d0,
                        "or" => old | d0,
                        "xor" => old ^ d0,
                        "wrxchg" => d0,
                        "cmpst" => {
                            // ds_cmpst_b32: data0 is the compare value, data1 the new one.
                            if old == d0 { d1 } else { old }
                        }
                        _ => return unsupported(format!("opcode {name}")),
                    };
                    mem.lds_write(at, new, 4);
                    if let Some(d) = dst {
                        self.vwr(&d, lane, old)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn mubuf(&mut self, name: &str, inst: &Instruction, mem: &mut Memory) -> Result<(), Error> {
        let Fields::Buffer {
            data,
            addr,
            rsrc,
            soffset,
            offset,
            offen,
            idxen,
            lds,
            tfe,
            ..
        } = inst.fields(self.lanes == 64)
        else {
            unreachable!()
        };
        if idxen || lds || tfe {
            return unsupported("a buffer access with idxen, lds or tfe");
        }
        let Operand::Scalar { code: r, .. } = rsrc else {
            return fault("a buffer descriptor that is no SGPR range");
        };
        let desc = [
            self.sreg(r),
            self.sreg(r + 1),
            self.sreg(r + 2),
            self.sreg(r + 3),
        ];
        let Some(Operand::Vector { reg: vdata, .. }) = data else {
            return unsupported(format!("opcode {name}"));
        };
        let vdata = usize::from(vdata);
        let so = self.rd(&soffset, 0)?;
        let (op, ty) = name
            .trim_start_matches("buffer_")
            .rsplit_once('_')
            .unwrap_or(("", ""));
        let count: usize = match ty {
            "dword" | "ubyte" | "sbyte" | "ushort" | "sshort" | "byte" | "short" => 1,
            "dwordx2" => 2,
            "dwordx3" => 3,
            "dwordx4" => 4,
            _ => 0,
        };
        for lane in 0..self.lanes {
            if !self.active(lane) {
                continue;
            }
            let voff = if offen { self.rd(&addr, lane)? } else { 0 };
            let at = u64::from(voff) + u64::from(so) + u64::from(offset);
            match (op, ty) {
                ("load", _) if count > 0 && ty.starts_with("dword") => {
                    if at % 4 != 0 {
                        return unsupported("an unaligned dword load");
                    }
                    for i in 0..count {
                        self.v[vdata + i][lane] = mem.load(desc, at + 4 * i as u64)?;
                    }
                }
                ("load", "ubyte" | "sbyte" | "ushort" | "sshort") => {
                    let word = mem.load(desc, at & !3)?;
                    let x = word >> (8 * (at % 4));
                    self.v[vdata][lane] = match ty {
                        "ubyte" => x & 0xFF,
                        "sbyte" => x as u8 as i8 as i32 as u32,
                        "ushort" if at % 2 == 0 => x & 0xFFFF,
                        "sshort" if at % 2 == 0 => x as u16 as i16 as i32 as u32,
                        _ => return unsupported("an unaligned 16-bit load"),
                    };
                }
                ("store", _) if count > 0 && ty.starts_with("dword") => {
                    if at % 4 != 0 {
                        return unsupported("an unaligned dword store");
                    }
                    for i in 0..count {
                        mem.store(desc, at + 4 * i as u64, self.v[vdata + i][lane], 4)?;
                    }
                }
                ("store", "byte") => mem.store(desc, at, self.v[vdata][lane], 1)?,
                ("store", "short") if at % 2 == 0 => mem.store(desc, at, self.v[vdata][lane], 2)?,
                _ => return unsupported(format!("opcode {name}")),
            }
        }
        Ok(())
    }

    fn valu(&mut self, inst: &Instruction, name: &str, v: &Valu) -> Result<(), Error> {
        if v.sdwa.is_some() || v.dpp.is_some() {
            return unsupported("SDWA and DPP forms");
        }
        if inst.encoding == Encoding::Vop3p {
            return unsupported(format!("opcode {name}"));
        }
        if v.opsel != 0 {
            return unsupported("op_sel");
        }
        let lanes = self.lanes;
        let exec = self.exec();
        match name {
            "v_nop" => return Ok(()),
            "v_readlane_b32" => {
                let lane = self.rd(&v.src[1], 0)? as usize & (lanes - 1);
                let value = self.rd(&v.src[0], lane)?;
                return self.swr(&v.dst.unwrap(), u64::from(value));
            }
            "v_readfirstlane_b32" => {
                let lane = if exec == 0 {
                    0
                } else {
                    exec.trailing_zeros() as usize
                };
                let value = self.rd(&v.src[0], lane)?;
                return self.swr(&v.dst.unwrap(), u64::from(value));
            }
            "v_writelane_b32" => {
                let lane = self.rd(&v.src[1], 0)? as usize & (lanes - 1);
                let value = self.rd(&v.src[0], 0)?;
                return self.vwr(&v.dst.unwrap(), lane, value);
            }
            _ => {}
        }
        if let Some(rest) = name.strip_prefix("v_cmp_").or(name.strip_prefix("v_cmpx_")) {
            let (cond, ty) = rest.split_once('_').unwrap_or((rest, ""));
            let mut bits = 0u64;
            for lane in (0..lanes).filter(|&l| exec >> l & 1 != 0) {
                let r = match ty {
                    "f32" if cond == "class" => {
                        let x = self.rd(&v.src[0], lane)?;
                        let x = modifiers(x, v, 0);
                        class_f32(x) & self.rd(&v.src[1], lane)? != 0
                    }
                    "f32" => {
                        let x = f32::from_bits(ftz(modifiers(self.rd(&v.src[0], lane)?, v, 0)));
                        let y = f32::from_bits(ftz(modifiers(self.rd(&v.src[1], lane)?, v, 1)));
                        float_cmp(cond, x, y)
                            .ok_or_else(|| Error::Unsupported(format!("opcode {name}")))?
                    }
                    "i32" | "u32" => int_cmp(
                        cond,
                        ty == "i32",
                        u64::from(self.rd(&v.src[0], lane)?),
                        u64::from(self.rd(&v.src[1], lane)?),
                        32,
                    )
                    .ok_or_else(|| Error::Unsupported(format!("opcode {name}")))?,
                    "i64" | "u64" => int_cmp(
                        cond,
                        ty == "i64",
                        self.rd64(&v.src[0], lane)?,
                        self.rd64(&v.src[1], lane)?,
                        64,
                    )
                    .ok_or_else(|| Error::Unsupported(format!("opcode {name}")))?,
                    _ => return unsupported(format!("opcode {name}")),
                };
                bits |= u64::from(r) << lane;
            }
            if name.starts_with("v_cmpx_") {
                self.set_mask_at(EXEC, bits);
            } else {
                let Some(Operand::Scalar { code, .. }) = v.dst else {
                    return fault("a compare without a mask destination");
                };
                self.set_mask_at(code, bits);
            }
            return Ok(());
        }
        let dst = v
            .dst
            .ok_or_else(|| Error::Unsupported(format!("opcode {name}")))?;
        // Lane-mask carries: written for every lane, 0 where inactive.
        let carry_ops = matches!(
            name,
            "v_add_co_u32"
                | "v_sub_co_u32"
                | "v_subrev_co_u32"
                | "v_add_co_ci_u32"
                | "v_sub_co_ci_u32"
                | "v_subrev_co_ci_u32"
                | "v_mad_u64_u32"
                | "v_mad_i64_i32"
        );
        if carry_ops {
            if v.clamp {
                return unsupported("clamp on a carry");
            }
            let mut carries = 0u64;
            for lane in (0..lanes).filter(|&l| exec >> l & 1 != 0) {
                let a = self.rd(&v.src[0], lane)?;
                let b = self.rd(&v.src[1], lane)?;
                let cin = match v.src.get(2) {
                    Some(Operand::Scalar { code, .. }) if name.contains("_ci_") => {
                        (self.mask_at(*code) >> lane & 1) as u32
                    }
                    _ => 0,
                };
                let (r, c) = match name {
                    "v_add_co_u32" | "v_add_co_ci_u32" => {
                        let r = u64::from(a) + u64::from(b) + u64::from(cin);
                        (r as u32, r >> 32 != 0)
                    }
                    "v_sub_co_u32" | "v_sub_co_ci_u32" => (
                        a.wrapping_sub(b).wrapping_sub(cin),
                        u64::from(b) + u64::from(cin) > u64::from(a),
                    ),
                    "v_subrev_co_u32" | "v_subrev_co_ci_u32" => (
                        b.wrapping_sub(a).wrapping_sub(cin),
                        u64::from(a) + u64::from(cin) > u64::from(b),
                    ),
                    "v_mad_u64_u32" => {
                        let c = self.rd64(&v.src[2], lane)?;
                        let (r, o) = (u64::from(a) * u64::from(b)).overflowing_add(c);
                        self.vwr64(&dst, lane, r)?;
                        carries |= u64::from(o) << lane;
                        continue;
                    }
                    _ => {
                        let c = self.rd64(&v.src[2], lane)? as i64;
                        let p = i64::from(a as i32) * i64::from(b as i32);
                        let (r, o) = p.overflowing_add(c);
                        self.vwr64(&dst, lane, r as u64)?;
                        carries |= u64::from(o) << lane;
                        continue;
                    }
                };
                self.vwr(&dst, lane, r)?;
                carries |= u64::from(c) << lane;
            }
            if let Some(Operand::Scalar { code, .. }) = v.carry {
                self.set_mask_at(code, carries);
            }
            return Ok(());
        }
        let float_dst = float_result(name);
        if (v.clamp || v.omod != 0) && !float_dst {
            return unsupported("clamp or omod on an integer result");
        }
        if (v.abs != 0 || v.neg != 0) && !(float_source(name) || name == "v_cndmask_b32") {
            return unsupported("abs or neg on an integer source");
        }
        for lane in (0..lanes).filter(|&l| exec >> l & 1 != 0) {
            match name {
                "v_lshlrev_b64" | "v_lshrrev_b64" | "v_ashrrev_i64" => {
                    let s = self.rd(&v.src[0], lane)? & 63;
                    let x = self.rd64(&v.src[1], lane)?;
                    let r = match name {
                        "v_lshlrev_b64" => x << s,
                        "v_lshrrev_b64" => x >> s,
                        _ => ((x as i64) >> s) as u64,
                    };
                    self.vwr64(&dst, lane, r)?;
                    continue;
                }
                "v_cndmask_b32" => {
                    let Some(Operand::Scalar { code, .. }) = v.src.get(2) else {
                        return fault("v_cndmask_b32 without a mask");
                    };
                    let pick = self.mask_at(*code) >> lane & 1 != 0;
                    let i = usize::from(pick);
                    let x = modifiers(self.rd(&v.src[i], lane)?, v, i);
                    self.vwr(&dst, lane, x)?;
                    continue;
                }
                _ => {}
            }
            let mut s = [0u32; 3];
            for (i, op) in v.src.iter().enumerate().take(3) {
                s[i] = self.rd(op, lane)?;
                if float_source(name) && !(name == "v_ldexp_f32" && i == 1) {
                    s[i] = modifiers(s[i], v, i);
                }
            }
            let old = match dst {
                Operand::Vector { reg, .. } => self.v[usize::from(reg)][lane],
                _ => 0,
            };
            let mut r = alu(name, s, old, lane)?;
            if float_dst {
                r = output_modifiers(r, v.omod, v.clamp);
            }
            self.vwr(&dst, lane, r)?;
        }
        Ok(())
    }
}

/// The f32 bits flushed to signed zero when a denormal.
fn ftz(x: u32) -> u32 {
    if x & 0x7F80_0000 == 0 {
        x & 0x8000_0000
    } else {
        x
    }
}

fn f(x: u32) -> f32 {
    f32::from_bits(ftz(x))
}

fn bits(x: f32) -> u32 {
    ftz(x.to_bits())
}

/// VOP3 `abs` then `neg` of source `i`, on the bits.
fn modifiers(x: u32, v: &Valu, i: usize) -> u32 {
    let mut x = x;
    if v.abs >> i & 1 != 0 {
        x &= 0x7FFF_FFFF;
    }
    if v.neg >> i & 1 != 0 {
        x ^= 0x8000_0000;
    }
    x
}

fn output_modifiers(r: u32, omod: u8, clamp: bool) -> u32 {
    let mut x = f32::from_bits(r);
    x = match omod {
        1 => x * 2.0,
        2 => x * 4.0,
        3 => x * 0.5,
        _ => x,
    };
    // The clamp is a maximum and a minimum, so -0 clamps to +0.
    if clamp {
        x = if x.is_nan() {
            0.0
        } else {
            fmin(fmax(x, 0.0), 1.0)
        };
    }
    bits(x)
}

/// Opcodes whose sources are f32 (VOP3 modifiers apply).
fn float_source(name: &str) -> bool {
    name.ends_with("_f32")
}

/// Opcodes whose result is an f32.
fn float_result(name: &str) -> bool {
    if let Some(rest) = name.strip_prefix("v_cvt_") {
        return rest.starts_with("f32_");
    }
    name.ends_with("_f32") && !name.starts_with("v_cmp") && !name.starts_with("v_frexp_exp")
}

/// -1 (as u32) when `any` is false, else `n`.
fn first(any: bool, n: u32) -> u32 {
    if any { n } else { u32::MAX }
}

fn flbit_i32(a: u32) -> u32 {
    let x = if (a as i32) < 0 { !a } else { a };
    first(x != 0, x.leading_zeros())
}

fn bfm(width: u32, offset: u32) -> u32 {
    let w = width & 31;
    let ones = if w == 0 { 0 } else { u32::MAX >> (32 - w) };
    ones << (offset & 31)
}

/// Bits `off..off + width` of `a` (width 32 or more: all from `off`).
fn bfe_u(a: u32, off: u32, width: u32) -> u32 {
    if width == 0 {
        return 0;
    }
    let x = a >> off;
    if width >= 32 {
        x
    } else {
        x & ((1 << width) - 1)
    }
}

/// [`bfe_u`] sign-extended from its top bit (the shift is arithmetic).
fn bfe_i(a: u32, off: u32, width: u32) -> u32 {
    if width == 0 {
        return 0;
    }
    let x = ((a as i32) >> off) as u32;
    if width >= 32 {
        x
    } else {
        (((x << (32 - width)) as i32) >> (32 - width)) as u32
    }
}

fn int_cmp(cond: &str, signed: bool, a: u64, b: u64, bits: u32) -> Option<bool> {
    let (x, y) = if signed {
        let sh = 64 - bits;
        (((a << sh) as i64) >> sh, ((b << sh) as i64) >> sh)
    } else {
        let m = if bits == 64 {
            u64::MAX
        } else {
            (1u64 << bits) - 1
        };
        ((a & m) as i64, (b & m) as i64)
    };
    // Unsigned 64-bit values do not fit an i64: compare them as u64.
    let ord = if !signed && bits == 64 {
        a.cmp(&b)
    } else {
        x.cmp(&y)
    };
    use std::cmp::Ordering::*;
    Some(match cond {
        "f" => false,
        "t" | "tru" => true,
        "eq" => ord == Equal,
        "lg" | "ne" => ord != Equal,
        "lt" => ord == Less,
        "le" => ord != Greater,
        "gt" => ord == Greater,
        "ge" => ord != Less,
        _ => return None,
    })
}

fn float_cmp(cond: &str, a: f32, b: f32) -> Option<bool> {
    let un = a.is_nan() || b.is_nan();
    Some(match cond {
        "f" => false,
        "lt" => a < b,
        "eq" => a == b,
        "le" => a <= b,
        "gt" => a > b,
        "lg" => !un && a != b,
        "ge" => a >= b,
        "o" => !un,
        "u" => un,
        // The negated ones hold for unordered operands.
        "nge" => un || a < b,
        "nlg" => un || a == b,
        "ngt" => un || a <= b,
        "nle" => un || a > b,
        "neq" => a != b,
        "nlt" => un || a >= b,
        "tru" => true,
        _ => return None,
    })
}

/// `v_cmp_class_f32`'s class bit of `x` (denormals are their own class).
fn class_f32(x: u32) -> u32 {
    let (neg, e, m) = (x >> 31 != 0, x >> 23 & 0xFF, x & 0x7F_FFFF);
    let bit = match (e, m) {
        (0xFF, m) if m != 0 => {
            if m & 0x40_0000 != 0 {
                1
            } else {
                0
            }
        }
        (0xFF, _) => {
            if neg {
                2
            } else {
                9
            }
        }
        (0, 0) => {
            if neg {
                5
            } else {
                6
            }
        }
        (0, _) => {
            if neg {
                4
            } else {
                7
            }
        }
        _ => {
            if neg {
                3
            } else {
                8
            }
        }
    };
    1 << bit
}

/// RDNA2 `v_min_f32`: the non-NaN operand; -0 below +0.
fn fmin(a: f32, b: f32) -> f32 {
    if a.is_nan() {
        return b;
    }
    if b.is_nan() {
        return a;
    }
    if a == b {
        return if a.is_sign_negative() { a } else { b };
    }
    if a < b { a } else { b }
}

fn fmax(a: f32, b: f32) -> f32 {
    if a.is_nan() {
        return b;
    }
    if b.is_nan() {
        return a;
    }
    if a == b {
        return if a.is_sign_positive() { a } else { b };
    }
    if a > b { a } else { b }
}

/// RDNA2 `v_med3_f32`: with NaNs, the minimum of the other operands.
fn fmed3(a: f32, b: f32, c: f32) -> f32 {
    let rest: Vec<f32> = [a, b, c].into_iter().filter(|x| !x.is_nan()).collect();
    match rest[..] {
        [] => f32::NAN,
        [x] => x,
        [x, y] => fmin(x, y),
        _ => fmax(fmin(a, b), fmin(fmax(a, b), c)),
    }
}

/// `v_sin_f32`/`v_cos_f32` of `x` turns, exact at quarter turns.
fn turn(x: f32, cos: bool) -> f32 {
    if !x.is_finite() {
        return f32::NAN;
    }
    let r = f64::from(x) - f64::from(x).round();
    let q = r * 4.0;
    if q == q.round() {
        let q = (q as i64 + i64::from(cos)).rem_euclid(4);
        return match q {
            0 if !cos => {
                if x.is_sign_negative() && x == 0.0 {
                    -0.0
                } else {
                    0.0
                }
            }
            0 | 2 => 0.0,
            1 => 1.0,
            _ => -1.0,
        };
    }
    let a = r * std::f64::consts::TAU;
    (if cos { a.cos() } else { a.sin() }) as f32
}

/// A 32-bit VALU operation of up to three sources; `old` is the
/// destination's value (the accumulator of `v_mac`/`v_fmac`).
fn alu(name: &str, s: [u32; 3], old: u32, lane: usize) -> Result<u32, Error> {
    let [a, b, c] = s;
    let (fa, fb, fc) = (f(a), f(b), f(c));
    let u24 = |x: u32| x & 0xFF_FFFF;
    let i24 = |x: u32| ((x << 8) as i32 >> 8) as i64;
    let lt_mask: u64 = if lane == 0 {
        0
    } else {
        u64::MAX >> (64 - lane)
    };
    Ok(match name {
        "v_mov_b32" => a,
        "v_not_b32" => !a,
        "v_bfrev_b32" => a.reverse_bits(),
        "v_ffbh_u32" => first(a != 0, a.leading_zeros()),
        "v_ffbl_b32" => first(a != 0, a.trailing_zeros()),
        "v_ffbh_i32" => flbit_i32(a),
        "v_cvt_f32_i32" => bits(a as i32 as f32),
        "v_cvt_f32_u32" => bits(a as f32),
        "v_cvt_i32_f32" => fa as i32 as u32,
        "v_cvt_u32_f32" => fa as u32,
        "v_cvt_flr_i32_f32" => fa.floor() as i32 as u32,
        "v_cvt_rpi_i32_f32" => (fa + 0.5).floor() as i32 as u32,
        "v_cvt_f32_ubyte0" => bits((a & 0xFF) as f32),
        "v_cvt_f32_ubyte1" => bits((a >> 8 & 0xFF) as f32),
        "v_cvt_f32_ubyte2" => bits((a >> 16 & 0xFF) as f32),
        "v_cvt_f32_ubyte3" => bits((a >> 24) as f32),
        "v_cvt_off_f32_i4" => bits(f32::from(((a << 28) as i32 >> 28) as i8) * 0.0625),
        "v_fract_f32" => {
            if !fa.is_finite() {
                f32::NAN.to_bits()
            } else {
                bits((fa - fa.floor()).min(f32::from_bits(0x3F7F_FFFF)))
            }
        }
        "v_trunc_f32" => bits(fa.trunc()),
        "v_ceil_f32" => bits(fa.ceil()),
        "v_floor_f32" => bits(fa.floor()),
        "v_rndne_f32" => bits(fa.round_ties_even()),
        "v_exp_f32" => bits(f64::from(fa).exp2() as f32),
        "v_log_f32" => bits(f64::from(fa).log2() as f32),
        "v_rcp_f32" | "v_rcp_iflag_f32" => bits((1.0 / f64::from(fa)) as f32),
        "v_rsq_f32" => bits((1.0 / f64::from(fa).sqrt()) as f32),
        "v_sqrt_f32" => bits(f64::from(fa).sqrt() as f32),
        "v_sin_f32" => bits(turn(fa, false)),
        "v_cos_f32" => bits(turn(fa, true)),
        // VOP2 floats.
        "v_add_f32" => bits(fa + fb),
        "v_sub_f32" => bits(fa - fb),
        "v_subrev_f32" => bits(fb - fa),
        "v_mul_f32" => bits(fa * fb),
        "v_mul_legacy_f32" => {
            if fa == 0.0 || fb == 0.0 {
                0
            } else {
                bits(fa * fb)
            }
        }
        "v_min_f32" => bits(fmin(fa, fb)),
        "v_max_f32" => bits(fmax(fa, fb)),
        "v_mac_f32" => bits(f32::from_bits(bits(fa * fb)) + f(old)),
        "v_fmac_f32" => bits(fa.mul_add(fb, f(old))),
        // (v_madmk: src = [a, K, b]; v_madak: [a, b, K].)
        "v_madmk_f32" | "v_madak_f32" | "v_mad_f32" => bits(f32::from_bits(bits(fa * fb)) + fc),
        "v_fmamk_f32" | "v_fmaak_f32" | "v_fma_f32" => bits(fa.mul_add(fb, fc)),
        "v_ldexp_f32" => {
            let e = (b as i32).clamp(-400, 400);
            bits((f64::from(fa) * 2f64.powi(e)) as f32)
        }
        "v_min3_f32" => bits(fmin(fmin(fa, fb), fc)),
        "v_max3_f32" => bits(fmax(fmax(fa, fb), fc)),
        "v_med3_f32" => bits(fmed3(fa, fb, fc)),
        // VOP2 integers.
        "v_mul_i32_i24" => (i24(a) * i24(b)) as u32,
        "v_mul_hi_i32_i24" => ((i24(a) * i24(b)) >> 32) as u32,
        "v_mul_u32_u24" => (u64::from(u24(a)) * u64::from(u24(b))) as u32,
        "v_mul_hi_u32_u24" => ((u64::from(u24(a)) * u64::from(u24(b))) >> 32) as u32,
        "v_min_i32" => (a as i32).min(b as i32) as u32,
        "v_max_i32" => (a as i32).max(b as i32) as u32,
        "v_min_u32" => a.min(b),
        "v_max_u32" => a.max(b),
        "v_lshrrev_b32" => b >> (a & 31),
        "v_ashrrev_i32" => ((b as i32) >> (a & 31)) as u32,
        "v_lshlrev_b32" => b << (a & 31),
        "v_and_b32" => a & b,
        "v_or_b32" => a | b,
        "v_xor_b32" => a ^ b,
        "v_xnor_b32" => !(a ^ b),
        "v_add_nc_u32" | "v_add_nc_i32" => a.wrapping_add(b),
        "v_sub_nc_u32" | "v_sub_nc_i32" => a.wrapping_sub(b),
        "v_subrev_nc_u32" => b.wrapping_sub(a),
        // VOP3 integers.
        "v_mad_i32_i24" => (i24(a) * i24(b)).wrapping_add(i64::from(c as i32)) as u32,
        "v_mad_u32_u24" => {
            (u64::from(u24(a)) * u64::from(u24(b))).wrapping_add(u64::from(c)) as u32
        }
        "v_bfe_u32" => bfe_u(a, b & 31, c & 31),
        "v_bfe_i32" => bfe_i(a, b & 31, c & 31),
        "v_bfi_b32" => a & b | !a & c,
        "v_alignbit_b32" => ((u64::from(a) << 32 | u64::from(b)) >> (c & 31)) as u32,
        "v_alignbyte_b32" => ((u64::from(a) << 32 | u64::from(b)) >> (8 * (c & 3))) as u32,
        "v_min3_i32" => (a as i32).min(b as i32).min(c as i32) as u32,
        "v_max3_i32" => (a as i32).max(b as i32).max(c as i32) as u32,
        "v_med3_i32" => {
            let (x, y, z) = (a as i32, b as i32, c as i32);
            x.min(y).max(x.max(y).min(z)) as u32
        }
        "v_min3_u32" => a.min(b).min(c),
        "v_max3_u32" => a.max(b).max(c),
        "v_med3_u32" => a.min(b).max(a.max(b).min(c)),
        "v_sad_u32" => a.abs_diff(b).wrapping_add(c),
        "v_sad_u8" => (0..4)
            .map(|i| ((a >> (8 * i)) & 0xFF).abs_diff((b >> (8 * i)) & 0xFF))
            .sum::<u32>()
            .wrapping_add(c),
        "v_lerp_u8" => (0..4).fold(0, |r, i| {
            let (x, y) = ((a >> (8 * i)) & 0xFF, (b >> (8 * i)) & 0xFF);
            r | ((x + y + (c >> (8 * i) & 1)) >> 1) << (8 * i)
        }),
        "v_mul_lo_u32" | "v_mul_lo_i32" => a.wrapping_mul(b),
        "v_mul_hi_u32" => ((u64::from(a) * u64::from(b)) >> 32) as u32,
        "v_mul_hi_i32" => ((i64::from(a as i32) * i64::from(b as i32)) >> 32) as u32,
        "v_xor3_b32" => a ^ b ^ c,
        "v_or3_b32" => a | b | c,
        "v_and_or_b32" => a & b | c,
        "v_lshl_or_b32" => a << (b & 31) | c,
        "v_lshl_add_u32" => (a << (b & 31)).wrapping_add(c),
        "v_add_lshl_u32" => a.wrapping_add(b) << (c & 31),
        "v_add3_u32" => a.wrapping_add(b).wrapping_add(c),
        "v_xad_u32" => (a ^ b).wrapping_add(c),
        "v_perm_b32" => perm(a, b, c),
        "v_bfm_b32" => bfm(a, b),
        "v_bcnt_u32_b32" => a.count_ones().wrapping_add(b),
        "v_mbcnt_lo_u32_b32" => (a & lt_mask as u32).count_ones().wrapping_add(b),
        "v_mbcnt_hi_u32_b32" => (a & (lt_mask >> 32) as u32).count_ones().wrapping_add(b),
        _ => return unsupported(format!("opcode {name}")),
    })
}

/// `v_perm_b32`: each result byte picks a byte of {a, b} (b low) by the
/// selector byte of `c`: 0-7 a byte, 8-11 a sign replicated, 12 zero, 13+ ones.
fn perm(a: u32, b: u32, c: u32) -> u32 {
    let x = u64::from(a) << 32 | u64::from(b);
    (0..4).fold(0, |r, i| {
        let sel = c >> (8 * i) & 0xFF;
        let byte = match sel {
            0..=7 => (x >> (8 * sel) & 0xFF) as u32,
            8..=11 => {
                // The sign bits of bytes 1, 3, 5 and 7.
                let bit = 15 + 16 * (sel - 8);
                if x >> bit & 1 != 0 { 0xFF } else { 0 }
            }
            12 => 0,
            _ => 0xFF,
        };
        r | byte << (8 * i)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rdna2_float_rules_hold_in_the_reference() {
        let nan = f32::NAN;
        assert_eq!(fmed3(1.0, 2.0, nan), 1.0);
        assert_eq!(fmed3(nan, 1.0, 0.0), 0.0);
        assert_eq!(fmed3(nan, 5.0, nan), 5.0);
        assert_eq!(fmed3(3.0, 1.0, 2.0), 2.0);
        assert!(fmed3(nan, nan, nan).is_nan());
        assert_eq!(fmin(-0.0, 0.0).to_bits(), 0x8000_0000);
        assert_eq!(fmax(-0.0, 0.0).to_bits(), 0);
        let fract = |x: f32| alu("v_fract_f32", [x.to_bits(), 0, 0], 0, 0).unwrap();
        assert_eq!(fract(-1e-10), 0x3F7F_FFFF);
        assert_eq!(fract(-3.25), 0.75f32.to_bits());
        assert_eq!(turn(255.5, false).to_bits(), 0);
        assert_eq!(turn(0.25, true).to_bits(), 0);
        assert_eq!(turn(-255.25, false), -1.0);
        assert_eq!(ftz(0x0000_0001), 0);
        assert_eq!(ftz(0x8000_0001), 0x8000_0000);
        assert_eq!(perm(0x8811_2233, 0x4455_6677, 0x0C0D_0800), 0x00FF_0077);
        assert_eq!(bfe_i(0x8000_0000, 28, 8), 0xFFFF_FFF8);
        assert_eq!(bfe_u(0x8000_0000, 28, 8), 8);
    }
}
