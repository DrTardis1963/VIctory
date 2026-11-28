//! A small GFX10 assembler for tests: SP3 text in, machine words out.
//!
//! Tests used to spell instructions as raw words through hand-written
//! encoders, which hid what a test runs. This reads the syntax
//! [`Instruction::text`] prints, so a test can say what it means and a
//! decoded program can be printed and assembled back to the same words.
//!
//! It knows each opcode's operands by asking the decoder: an instruction of
//! that opcode with zeroed fields says which destinations and sources it
//! has. The assembler then only places the text's operands in their bit
//! fields, and cannot disagree with the decoder about which operands exist.
//!
//! Choices where the text is ambiguous:
//! * A VOP1, VOP2 or VOPC opcode uses its short encoding when the operands
//!   allow it (no modifiers, a VGPR second source, VCC for implicit lane
//!   masks), else VOP3. `_e32` / `_e64` on the name forces one.
//! * A number that is an inline constant (-16 to 64, the float constants)
//!   becomes one; `lit(...)` always makes a literal dword. Other numbers are
//!   literals: integers as written, floats as their f32 bits.
//! * Register ranges give only their first register; the opcode sets the size.
//! * Branch targets are labels (`name:` on a line of its own or before an
//!   instruction) or, as the printer writes them, `label_<hex byte offset>`.
//! * Lines end at `//`. SDWA and DPP forms are not supported.

use super::fields::{Fields, NULL, Valu};
use super::{Encoding, Instruction, opcodes as t, sp3};

/// Assembles `text`, panicking with the line and reason on an error.
pub(crate) fn asm(text: &str) -> Vec<u32> {
    assemble(text).unwrap_or_else(|e| panic!("asm: {e}"))
}

/// Assembles `text`, one instruction per line, into GFX10 words.
pub(crate) fn assemble(text: &str) -> Result<Vec<u32>, String> {
    let mut lines = Vec::new();
    let mut labels = Vec::<(String, usize)>::new();
    for (n, line) in text.lines().enumerate() {
        let mut line = line.split("//").next().unwrap_or("").trim();
        if let Some((label, rest)) = line.split_once(':')
            && !label.is_empty()
            && label
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
            && !rest.starts_with(|c: char| !c.is_whitespace())
        {
            labels.push((label.to_owned(), lines.len()));
            line = rest.trim();
        }
        if !line.is_empty() {
            lines.push((n + 1, line));
        }
    }
    // Sizes do not depend on label values, so a first pass with every label
    // at 0 finds each instruction's offset.
    let mut offsets = Vec::with_capacity(lines.len());
    let mut at = 0;
    for &(n, line) in &lines {
        offsets.push(at);
        let size = statement(line, at, &|_| Some(0)).map_err(|e| format!("line {n}: {e}"))?;
        at += 4 * size.len();
    }
    let label_at = |name: &str| -> Option<usize> {
        if let Some(&(_, index)) = labels.iter().find(|(l, _)| l == name) {
            return Some(offsets.get(index).copied().unwrap_or(at));
        }
        usize::from_str_radix(name.strip_prefix("label_")?, 16).ok()
    };
    let mut words = Vec::new();
    for (&(n, line), &offset) in lines.iter().zip(&offsets) {
        words.extend(statement(line, offset, &label_at).map_err(|e| format!("line {n}: {e}"))?);
    }
    Ok(words)
}

type Labels<'a> = dyn Fn(&str) -> Option<usize> + 'a;

/// One instruction at byte `offset`.
fn statement(line: &str, offset: usize, labels: &Labels) -> Result<Vec<u32>, String> {
    let (mnemonic, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let (name, force) = match mnemonic {
        n if n.ends_with("_e32") => (&n[..n.len() - 4], Some(false)),
        n if n.ends_with("_e64") => (&n[..n.len() - 4], Some(true)),
        n => (n, None),
    };
    let (ops, mods) = if name == "s_waitcnt" {
        (vec![rest.trim().to_owned()], Mods::default())
    } else {
        split_operands(rest.trim())
    };
    let (encoding, opcode) = find(name).ok_or_else(|| format!("unknown opcode `{name}`"))?;
    let mut a = Asm {
        name,
        ops,
        mods,
        offset,
        labels,
        literal: None,
    };
    let mut words = match encoding {
        Encoding::Sop2 | Encoding::Sop1 | Encoding::Sopc => a.sop(encoding, opcode)?,
        Encoding::Sopk => a.sopk(opcode)?,
        Encoding::Sopp => a.sopp(opcode)?,
        Encoding::Smem => a.smem(opcode)?,
        Encoding::Vop1 | Encoding::Vop2 | Encoding::Vopc => {
            if force != Some(true)
                && let Some(words) = a.short_valu(encoding, opcode)?
            {
                words
            } else if force == Some(false) {
                return Err("the operands need the VOP3 encoding".into());
            } else {
                // The short form may have taken a literal before it gave up.
                a.literal = None;
                let base = match encoding {
                    Encoding::Vopc => 0,
                    Encoding::Vop2 => 0x100,
                    _ => 0x180,
                };
                a.vop3(base + opcode)?
            }
        }
        Encoding::Vop3 => a.vop3(opcode)?,
        Encoding::Vop3p => a.vop3p(opcode)?,
        Encoding::Vintrp => a.vintrp(opcode)?,
        Encoding::Ds => a.ds(opcode)?,
        Encoding::Mubuf | Encoding::Mtbuf => a.buffer(encoding, opcode)?,
        Encoding::Mimg => a.image(opcode)?,
        Encoding::Flat | Encoding::Global | Encoding::Scratch => a.flat(encoding, opcode)?,
        Encoding::Exp => a.exp()?,
    };
    a.mods.finish()?;
    if let Some(literal) = a.literal {
        words.push(literal);
    }
    Ok(words)
}

/// The encoding and opcode of `name`.
fn find(name: &str) -> Option<(Encoding, u16)> {
    if name == "exp" {
        return Some((Encoding::Exp, 0));
    }
    [
        (Encoding::Sop2, t::SOP2),
        (Encoding::Sopk, t::SOPK),
        (Encoding::Sop1, t::SOP1),
        (Encoding::Sopc, t::SOPC),
        (Encoding::Sopp, t::SOPP),
        (Encoding::Smem, t::SMEM),
        (Encoding::Vintrp, t::VINTRP),
        (Encoding::Vop1, t::VOP1),
        (Encoding::Vop2, t::VOP2),
        (Encoding::Vopc, t::VOPC),
        (Encoding::Vop3, t::VOP3),
        (Encoding::Vop3p, t::VOP3P),
        (Encoding::Ds, t::DS),
        (Encoding::Mubuf, t::MUBUF),
        (Encoding::Mtbuf, t::MTBUF),
        (Encoding::Mimg, t::MIMG),
        (Encoding::Flat, t::FLAT),
        (Encoding::Global, t::GLOBAL),
        (Encoding::Scratch, t::SCRATCH),
    ]
    .into_iter()
    .find_map(|(encoding, table)| {
        Some((encoding, table.iter().find(|e| e.1 == name).map(|e| e.0)?))
    })
}

/// The decoder's view of an instruction with these words (zero-padded).
fn probe(words: &[u32]) -> Fields {
    first_instruction(words)
        .expect("a zeroed instruction decodes")
        .fields(false)
}

/// The instruction `words` start with, zero-padded to the longest form and
/// without following control flow: for tests that try words one at a time.
pub(crate) fn first_instruction(words: &[u32]) -> Result<Instruction, crate::ShaderError> {
    const MAX_WORDS: usize = 5;
    let mut code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    code.resize(4 * MAX_WORDS + code.len(), 0);
    super::instruction(&code, 0)
}

/// Operands split at top-level commas; the words after the last operand
/// that are modifiers (`glc`, `offset:16`...).
fn split_operands(text: &str) -> (Vec<String>, Mods) {
    let mut ops = split_top(text, |c| c == ',');
    let mut mods = Mods::default();
    if let Some(last) = ops.pop() {
        let words = split_top(&last, char::is_whitespace);
        let mut words = words.into_iter().filter(|w| !w.is_empty()).peekable();
        if let Some(first) = words.next_if(|w| !is_modifier(w)) {
            ops.push(first);
        }
        for w in words {
            match w.split_once(':') {
                Some((k, v)) => mods.items.push((k.to_owned(), Some(v.to_owned()), false)),
                None => mods.items.push((w, None, false)),
            }
        }
    }
    (ops, mods)
}

fn is_modifier(word: &str) -> bool {
    word.split_once(':').is_some_and(|(key, _)| {
        !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    }) || matches!(
        word,
        "glc"
            | "slc"
            | "dlc"
            | "idxen"
            | "offen"
            | "lds"
            | "tfe"
            | "gds"
            | "done"
            | "compr"
            | "vm"
            | "r128"
            | "unorm"
            | "a16"
            | "d16"
            | "lwe"
            | "clamp"
            | "high"
            | "row_mirror"
            | "row_half_mirror"
    )
}

/// Splits at `sep` outside brackets and parentheses, trimming each piece.
fn split_top(text: &str, sep: impl Fn(char) -> bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut piece = String::new();
    for c in text.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            _ => {}
        }
        if depth == 0 && sep(c) {
            out.push(piece.trim().to_owned());
            piece.clear();
        } else {
            piece.push(c);
        }
    }
    if !piece.trim().is_empty() || !out.is_empty() {
        out.push(piece.trim().to_owned());
    }
    out
}

/// Modifiers after the operands; each must be used by the encoder.
#[derive(Default)]
struct Mods {
    items: Vec<(String, Option<String>, bool)>,
}

impl Mods {
    fn flag(&mut self, name: &str) -> bool {
        self.items
            .iter_mut()
            .find(|(k, v, _)| k == name && v.is_none())
            .map(|item| item.2 = true)
            .is_some()
    }

    fn value(&mut self, name: &str) -> Option<String> {
        let item = self
            .items
            .iter_mut()
            .find(|(k, v, _)| k == name && v.is_some())?;
        item.2 = true;
        item.1.clone()
    }

    fn number(&mut self, name: &str) -> Result<Option<i64>, String> {
        self.value(name)
            .map(|v| int(&v).ok_or_else(|| format!("bad {name}:{v}")))
            .transpose()
    }

    fn finish(&self) -> Result<(), String> {
        match self.items.iter().find(|i| !i.2) {
            Some((k, Some(v), _)) => Err(format!("unexpected modifier `{k}:{v}`")),
            Some((k, None, _)) => Err(format!("unexpected modifier `{k}`")),
            None => Ok(()),
        }
    }
}

/// A register or constant operand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reg {
    /// Scalar operand code (0-127).
    S(u8),
    V(u8),
    /// Inline constant code (128-254).
    Const(u8),
    Lit(u32),
    Attr {
        index: u8,
        chan: u8,
    },
    /// `off`: an absent address or export channel.
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Sel {
    #[default]
    None,
    Lo,
    Hi,
    HiLo,
}

#[derive(Debug, Clone, Copy)]
struct Src {
    reg: Reg,
    neg: bool,
    abs: bool,
    sel: Sel,
    sext: bool,
}

impl Src {
    fn plain(&self) -> bool {
        !self.neg && !self.abs && !self.sext && self.sel == Sel::None
    }
}

fn int(text: &str) -> Option<i64> {
    let (neg, digits) = match text.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, text),
    };
    let value = match digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        Some(hex) => i64::from_str_radix(hex, 16).ok()?,
        None => digits.parse().ok()?,
    };
    Some(if neg { -value } else { value })
}

/// The value of inline constant `code`, as the 32 bits an f32 or i32 op sees.
fn const_value(code: u8) -> Option<u32> {
    Some(match code {
        128..=192 => u32::from(code) - 128,
        193..=208 => (192 - i32::from(code)) as u32,
        240..=248 => {
            let f: f32 = sp3::const_name(code).parse().ok()?;
            f.to_bits()
        }
        _ => return None,
    })
}

fn reg(text: &str) -> Result<Reg, String> {
    let bad = || format!("bad operand `{text}`");
    let range = |prefix: &str| -> Option<u32> {
        let rest = text.strip_prefix(prefix)?;
        if let Some(inner) = rest.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            let (lo, hi) = inner.split_once(':')?;
            let (lo, hi): (u32, u32) = (lo.trim().parse().ok()?, hi.trim().parse().ok()?);
            (hi >= lo).then_some(lo)
        } else if rest.starts_with(|c: char| c.is_ascii_digit()) {
            rest.parse().ok()
        } else {
            None
        }
    };
    Ok(match text {
        "vcc" | "vcc_lo" => Reg::S(106),
        "vcc_hi" => Reg::S(107),
        "m0" => Reg::S(124),
        "null" => Reg::S(NULL),
        "exec" | "exec_lo" => Reg::S(126),
        "exec_hi" => Reg::S(127),
        "off" => Reg::Off,
        _ => {
            if let Some(n) = range("ttmp") {
                return (n < 16).then(|| Reg::S(108 + n as u8)).ok_or_else(bad);
            }
            if let Some(n) = range("s") {
                return (n < 106).then_some(Reg::S(n as u8)).ok_or_else(bad);
            }
            if let Some(n) = range("v") {
                return (n < 256).then_some(Reg::V(n as u8)).ok_or_else(bad);
            }
            if let Some(inner) = text.strip_prefix("lit(").and_then(|r| r.strip_suffix(')')) {
                let v = int(inner).ok_or_else(bad)?;
                return Ok(Reg::Lit(v as u32));
            }
            if let Some(rest) = text.strip_prefix("attr") {
                let (index, chan) = rest.split_once('.').ok_or_else(bad)?;
                let chan = ["x", "y", "z", "w"]
                    .iter()
                    .position(|c| *c == chan)
                    .ok_or_else(bad)?;
                return Ok(Reg::Attr {
                    index: index.parse().map_err(|_| bad())?,
                    chan: chan as u8,
                });
            }
            if let Some(code) =
                (128..=254u8).find(|&c| text.starts_with("src_") && sp3::const_name(c) == text)
            {
                return Ok(Reg::Const(code));
            }
            if let Some(v) = int(text) {
                return Ok(match v {
                    0..=64 => Reg::Const(128 + v as u8),
                    -16..=-1 => Reg::Const((192 - v) as u8),
                    _ if (-(1 << 31)..1 << 32).contains(&v) => Reg::Lit(v as u32),
                    _ => return Err(bad()),
                });
            }
            let f: f32 = text.parse().map_err(|_| bad())?;
            match (240..=248u8).find(|&c| const_value(c) == Some(f.to_bits())) {
                Some(code) => Reg::Const(code),
                None => Reg::Lit(f.to_bits()),
            }
        }
    })
}

/// A source with its modifiers: `-x`, `abs(x)`, `|x|`, `sel_hi(x)`, `sext(x)`.
fn src(text: &str) -> Result<Src, String> {
    let mut s = Src {
        reg: Reg::Off,
        neg: false,
        abs: false,
        sel: Sel::None,
        sext: false,
    };
    let mut t = text.trim();
    loop {
        let wrapped = |p: &str| t.strip_prefix(p).and_then(|r| r.strip_suffix(')'));
        if let Some(rest) = t.strip_prefix('-')
            && !rest.starts_with(|c: char| c.is_ascii_digit() || c == '.')
        {
            s.neg = true;
            t = rest;
        } else if let Some(inner) = wrapped("abs(") {
            s.abs = true;
            t = inner;
        } else if let Some(inner) = t.strip_prefix('|').and_then(|r| r.strip_suffix('|')) {
            s.abs = true;
            t = inner;
        } else if let Some(inner) = wrapped("sel_hi_lo(") {
            s.sel = Sel::HiLo;
            t = inner;
        } else if let Some(inner) = wrapped("sel_hi(") {
            s.sel = Sel::Hi;
            t = inner;
        } else if let Some(inner) = wrapped("sel_lo(") {
            s.sel = Sel::Lo;
            t = inner;
        } else if let Some(inner) = wrapped("sext(") {
            s.sext = true;
            t = inner;
        } else {
            break;
        }
    }
    s.reg = reg(t)?;
    Ok(s)
}

/// Branch offset in dwords from the instruction after a 1-dword one at `at`.
fn branch(target: usize, at: usize) -> Result<u32, String> {
    let delta = target as i64 - (at as i64 + 4);
    if delta % 4 != 0 {
        return Err(format!("branch target {target:#x} is not dword-aligned"));
    }
    i16::try_from(delta / 4)
        .map(|d| u32::from(d as u16))
        .map_err(|_| format!("branch to {target:#x} out of range"))
}

struct Asm<'a> {
    name: &'a str,
    ops: Vec<String>,
    mods: Mods,
    offset: usize,
    labels: &'a Labels<'a>,
    literal: Option<u32>,
}

impl Asm<'_> {
    fn count(&self, n: usize) -> Result<(), String> {
        if self.ops.len() == n {
            Ok(())
        } else {
            Err(format!(
                "`{}` takes {n} operands, not {}",
                self.name,
                self.ops.len()
            ))
        }
    }

    fn reg(&self, i: usize) -> Result<Reg, String> {
        reg(&self.ops[i])
    }

    fn sgpr(&self, i: usize) -> Result<u32, String> {
        match self.reg(i)? {
            Reg::S(c) => Ok(c.into()),
            r => Err(format!(
                "operand {} must be a scalar register, not {r:?}",
                i + 1
            )),
        }
    }

    fn vgpr(&self, i: usize) -> Result<u32, String> {
        match self.reg(i)? {
            Reg::V(r) => Ok(r.into()),
            r => Err(format!("operand {} must be a VGPR, not {r:?}", i + 1)),
        }
    }

    /// A VGPR, or 0 for `off`.
    fn vgpr_or_off(&self, i: usize) -> Result<u32, String> {
        match self.reg(i)? {
            Reg::Off => Ok(0),
            _ => self.vgpr(i),
        }
    }

    /// The instruction's one literal dword; every literal operand shares it.
    fn literal(&mut self, value: u32) -> Result<(), String> {
        match self.literal.replace(value) {
            Some(old) if old != value => Err("two different literals".into()),
            _ => Ok(()),
        }
    }

    /// A 9-bit VALU source field (8 bits for scalar sources: VGPRs refused).
    fn code(&mut self, r: Reg, bits9: bool) -> Result<u32, String> {
        Ok(match r {
            Reg::S(c) | Reg::Const(c) => c.into(),
            Reg::Lit(v) => {
                self.literal(v)?;
                255
            }
            Reg::V(v) if bits9 => 256 + u32::from(v),
            Reg::Attr { index, chan } if bits9 => u32::from(index) | u32::from(chan) << 6,
            r => return Err(format!("{r:?} cannot be a source here")),
        })
    }

    fn scalar_src(&mut self, i: usize) -> Result<u32, String> {
        let r = self.reg(i)?;
        self.code(r, false)
    }

    fn number(&self, i: usize) -> Result<u32, String> {
        int(&self.ops[i])
            .map(|v| v as u32)
            .ok_or_else(|| format!("bad number `{}`", self.ops[i]))
    }

    fn label(&self, i: usize) -> Result<u32, String> {
        let text = &self.ops[i];
        if let Some(v) = int(text) {
            return Ok(v as u16 as u32);
        }
        let target = (self.labels)(text).ok_or_else(|| format!("unknown label `{text}`"))?;
        branch(target, self.offset)
    }

    fn sop(&mut self, encoding: Encoding, op: u16) -> Result<Vec<u32>, String> {
        let op = u32::from(op);
        let base = match encoding {
            Encoding::Sop2 => 0x8000_0000 | op << 23,
            Encoding::Sop1 => 0xBE80_0000 | op << 8,
            _ => 0xBF00_0000 | op << 16,
        };
        let Fields::Sop { dst, src, .. } = probe(&[base]) else {
            unreachable!("a scalar ALU opcode")
        };
        self.count(usize::from(dst.is_some()) + src.len())?;
        let mut w = base;
        let mut i = 0;
        if dst.is_some() {
            w |= self.sgpr(0)? << 16;
            i = 1;
        }
        for (k, shift) in [0, 8].into_iter().enumerate().take(src.len()) {
            w |= self.scalar_src(i + k)? << shift;
        }
        Ok(vec![w])
    }

    fn sopk(&mut self, op: u16) -> Result<Vec<u32>, String> {
        let base = 0xB000_0000 | u32::from(op) << 23;
        let Fields::Sop { dst, src, .. } = probe(&[base]) else {
            unreachable!("a SOPK opcode")
        };
        let regs = usize::from(dst.is_some()) + src.len();
        self.count(regs + 1)?;
        let name = self.name;
        // s_setreg*: the hwreg first; others: the registers, then the immediate.
        let (imm_at, reg_at) = if name.starts_with("s_setreg") {
            (0, 1)
        } else {
            (regs, 0)
        };
        let imm = match name {
            "s_getreg_b32" | "s_setreg_b32" | "s_setreg_imm32_b32" => hwreg(&self.ops[imm_at])?,
            "s_call_b64" => self.label(imm_at)?,
            _ => self.number(imm_at)? & 0xFFFF,
        };
        let mut w = base | imm;
        if name == "s_setreg_imm32_b32" {
            let lit = match self.reg(reg_at)? {
                Reg::Lit(v) => v,
                Reg::Const(c) => const_value(c).ok_or("not a value")?,
                r => return Err(format!("{r:?} is not an immediate")),
            };
            self.literal(lit)?;
        } else if regs == 1 {
            w |= self.sgpr(reg_at)? << 16;
        }
        Ok(vec![w])
    }

    fn sopp(&mut self, op: u16) -> Result<Vec<u32>, String> {
        let base = 0xBF80_0000 | u32::from(op) << 16;
        let name = self.name;
        let branchy = name == "s_branch" || name.starts_with("s_cbranch_");
        if self.ops.is_empty() && !branchy {
            return Ok(vec![base]);
        }
        self.count(1)?;
        let imm = if branchy {
            self.label(0)?
        } else if name == "s_waitcnt" {
            waitcnt(&self.ops[0])?
        } else if name.starts_with("s_sendmsg") {
            sendmsg(&self.ops[0])?
        } else {
            self.number(0)? & 0xFFFF
        };
        Ok(vec![base | imm])
    }

    fn smem(&mut self, op: u16) -> Result<Vec<u32>, String> {
        let base = 0xF400_0000 | u32::from(op) << 18;
        let Fields::Smem { data, .. } = probe(&[base, 0]) else {
            unreachable!("an SMEM opcode")
        };
        let probe_op = matches!(self.name, "s_atc_probe" | "s_atc_probe_buffer");
        let mut w0 = base;
        if !probe_op {
            w0 |= u32::from(self.mods.flag("glc")) << 16 | u32::from(self.mods.flag("dlc")) << 14;
        }
        let Some(_) = data else {
            self.count(0)?;
            return Ok(vec![w0, u32::from(NULL) << 25]);
        };
        if matches!(
            self.name,
            "s_memtime" | "s_memrealtime" | "s_get_waveid_in_workgroup"
        ) {
            self.count(1)?;
            w0 |= self.sgpr(0)? << 6;
            return Ok(vec![w0, u32::from(NULL) << 25]);
        }
        self.count(3)?;
        let sdata = if probe_op {
            self.number(0)? & 0x7F
        } else {
            self.sgpr(0)?
        };
        let sbase = self.sgpr(1)?;
        if sbase % 2 != 0 {
            return Err("an SMEM base must be an even SGPR".into());
        }
        let (soffset, offset) = match self.reg(2)? {
            Reg::S(c) => (u32::from(c), self.mods.number("offset")?.unwrap_or(0)),
            _ => (u32::from(NULL), i64::from(self.number(2)?)),
        };
        w0 |= sdata << 6 | sbase >> 1;
        Ok(vec![w0, soffset << 25 | (offset as u32 & 0x1F_FFFF)])
    }

    fn srcs(&self, from: usize) -> Result<Vec<Src>, String> {
        self.ops[from..].iter().map(|o| src(o)).collect()
    }

    fn omod(&mut self) -> Result<u32, String> {
        match (
            self.mods.value("mul").as_deref(),
            self.mods.value("div").as_deref(),
        ) {
            (None, None) => Ok(0),
            (Some("2"), None) => Ok(1),
            (Some("4"), None) => Ok(2),
            (None, Some("2")) => Ok(3),
            _ => Err("the output modifier is mul:2, mul:4 or div:2".into()),
        }
    }

    /// VOP1, VOP2 or VOPC when the operands fit; `None` when they need VOP3.
    fn short_valu(&mut self, encoding: Encoding, op: u16) -> Result<Option<Vec<u32>>, String> {
        let op32 = u32::from(op);
        let base = match encoding {
            Encoding::Vop1 => 0x7E00_0000 | op32 << 9,
            Encoding::Vop2 => op32 << 25,
            _ => 0x7C00_0000 | op32 << 17,
        };
        let Fields::Valu(v) = probe(&[base, 0]) else {
            unreachable!("a VALU opcode")
        };
        let lead = usize::from(v.dst.is_some()) + usize::from(v.carry.is_some());
        if self.ops.len() != lead + v.src.len() {
            // The VOP3 form may take another count (madak/madmk take one less).
            return Ok(None);
        }
        let global = ["clamp", "high", "mul", "div", "op_sel"];
        if self
            .mods
            .items
            .iter()
            .any(|(k, ..)| global.contains(&k.as_str()))
        {
            return Ok(None);
        }
        let srcs = self.srcs(lead)?;
        if srcs.iter().any(|s| !s.plain()) {
            return Ok(None);
        }
        let name = self.name;
        let mut w = base;
        if let Some(dst) = v.dst {
            match (dst, self.reg(0)?) {
                // VOPC: the implicit VCC or EXEC.
                (_, Reg::S(c)) if encoding == Encoding::Vopc => {
                    let super::Operand::Scalar { code, .. } = dst else {
                        unreachable!("a lane mask")
                    };
                    if c != code {
                        return Ok(None);
                    }
                }
                (super::Operand::Vector { .. }, Reg::V(r))
                | (super::Operand::Scalar { .. }, Reg::S(r)) => w |= u32::from(r) << 17,
                _ => return Ok(None),
            }
        }
        if v.carry.is_some() && self.reg(1)? != Reg::S(106) {
            return Ok(None);
        }
        let mk = name.ends_with("mk_f32") || name.ends_with("mk_f16");
        let ak = name.ends_with("ak_f32") || name.ends_with("ak_f16");
        let carry_in = name.ends_with("_co_ci_u32") || name == "v_cndmask_b32";
        // Which field each source goes to: src0, vsrc1, the literal, implicit VCC.
        let slots: &[u8] = match encoding {
            Encoding::Vop1 => b"0",
            Encoding::Vopc => b"01",
            _ if mk => b"0L1",
            _ if ak => b"01L",
            _ if carry_in => b"01c",
            _ => b"01",
        };
        for (s, slot) in srcs.iter().zip(slots) {
            match slot {
                b'0' => w |= self.code(s.reg, true)?,
                b'1' => match s.reg {
                    Reg::V(r) => w |= u32::from(r) << 9,
                    _ => return Ok(None),
                },
                b'L' => match s.reg {
                    Reg::Lit(v) => self.literal(v)?,
                    Reg::Const(c) => self.literal(const_value(c).ok_or("not a value")?)?,
                    _ => return Err("the constant operand must be a number".into()),
                },
                _ => {
                    if s.reg != Reg::S(106) {
                        return Ok(None);
                    }
                }
            }
        }
        if (mk || ak) && w & 0x1FF == 255 {
            return Err("a madmk/madak form has no room for a second literal".into());
        }
        Ok(Some(vec![w]))
    }

    fn vop3(&mut self, op: u16) -> Result<Vec<u32>, String> {
        let base = 0xD400_0000 | u32::from(op) << 16;
        let Fields::Valu(v) = probe(&[base, 0]) else {
            unreachable!("a VOP3 opcode")
        };
        let name = self.name;
        let lead = usize::from(v.dst.is_some()) + usize::from(v.carry.is_some());
        self.count(lead + v.src.len())?;
        let mut srcs = self.srcs(lead)?;
        if name.starts_with("v_interp_") && srcs.len() > 1 {
            srcs.swap(0, 1);
        }
        let mut w0 = base;
        if let Some(dst) = v.dst {
            w0 |= self.dst(dst)?;
        }
        // The decoder gives a carry exactly for VOP3B, whose bits 14:8 hold it.
        let vop3b = v.carry.is_some();
        if vop3b {
            w0 |= self.sgpr(1)? << 8;
        }
        let (mut neg, mut abs, mut opsel) = (0, 0, 0);
        let mut fields = [0u32; 3];
        for (i, s) in srcs.iter().enumerate() {
            fields[i] = self.code(s.reg, true)?;
            neg |= u32::from(s.neg) << i;
            abs |= u32::from(s.abs) << i;
            match s.sel {
                Sel::None => {}
                Sel::Hi => opsel |= 1 << i,
                _ => return Err("VOP3 takes only sel_hi".into()),
            }
            if s.sext {
                return Err("sext is SDWA only".into());
            }
        }
        if self.mods.flag("high") {
            fields[0] |= 1 << 8;
        }
        if name.starts_with("v_permlane") {
            opsel |= u32::from(self.mods.value("fi").as_deref() == Some("1"));
            opsel |= u32::from(self.mods.value("bound_ctrl").as_deref() == Some("1")) << 1;
        }
        if let Some(bits) = self.mods.value("op_sel") {
            opsel = bit_list(&bits)?;
        }
        if vop3b && (abs != 0 || opsel != 0) {
            return Err("a VOP3B opcode has no abs or op_sel".into());
        }
        w0 |= u32::from(self.mods.flag("clamp")) << 15 | (abs & 7) << 8 | (opsel & 0xF) << 11;
        let w1 = neg << 29 | self.omod()? << 27 | fields[0] | fields[1] << 9 | fields[2] << 18;
        Ok(vec![w0, w1])
    }

    /// A VALU destination field: a VGPR, or an SGPR where the decoder has one.
    fn dst(&self, dst: super::Operand) -> Result<u32, String> {
        match (dst, self.reg(0)?) {
            (super::Operand::Vector { .. }, Reg::V(r))
            | (super::Operand::Scalar { .. }, Reg::S(r)) => Ok(r.into()),
            (_, r) => Err(format!("bad destination {r:?}")),
        }
    }

    fn vop3p(&mut self, op: u16) -> Result<Vec<u32>, String> {
        let base = 0xCC00_0000 | u32::from(op) << 16;
        let Fields::Valu(Valu {
            dst, src: shape, ..
        }) = probe(&[base, 0])
        else {
            unreachable!("a VOP3P opcode")
        };
        self.count(usize::from(dst.is_some()) + shape.len())?;
        let mix = self.name.starts_with("v_fma_mix");
        let srcs = self.srcs(1)?;
        let (mut neg, mut neg_hi, mut opsel, mut opsel_hi) = (0, 0, 0, 0);
        let mut fields = [0u32; 3];
        for (i, s) in srcs.iter().enumerate() {
            fields[i] = self.code(s.reg, true)?;
            let bit = 1 << i;
            if mix {
                match s.sel {
                    Sel::None => {}
                    Sel::Lo => opsel_hi |= bit,
                    Sel::Hi => (opsel, opsel_hi) = (opsel | bit, opsel_hi | bit),
                    Sel::HiLo => return Err("v_fma_mix takes sel_lo or sel_hi".into()),
                }
                neg_hi |= u32::from(s.abs) << i;
                neg |= u32::from(s.neg) << i;
            } else {
                match s.sel {
                    Sel::None => opsel_hi |= bit,
                    Sel::Lo => {}
                    Sel::Hi => (opsel, opsel_hi) = (opsel | bit, opsel_hi | bit),
                    Sel::HiLo => opsel |= bit,
                }
                if s.abs {
                    return Err("packed math has no abs".into());
                }
                if s.neg {
                    (neg, neg_hi) = (neg | bit, neg_hi | bit);
                }
            }
        }
        let mut w0 = base | self.dst(dst.expect("VOP3P writes a VGPR"))?;
        w0 |= u32::from(self.mods.flag("clamp")) << 15 | (opsel_hi >> 2 & 1) << 14;
        w0 |= opsel << 11 | neg_hi << 8;
        let w1 = neg << 29 | (opsel_hi & 3) << 27 | fields[0] | fields[1] << 9 | fields[2] << 18;
        Ok(vec![w0, w1])
    }

    fn vintrp(&mut self, op: u16) -> Result<Vec<u32>, String> {
        self.count(3)?;
        let vsrc = if self.name == "v_interp_mov_f32" {
            ["p10", "p20", "p0"]
                .iter()
                .position(|p| *p == self.ops[1])
                .ok_or("v_interp_mov_f32 takes p10, p20 or p0")? as u32
        } else {
            self.vgpr(1)?
        };
        let Reg::Attr { index, chan } = self.reg(2)? else {
            return Err("the third operand is an attribute".into());
        };
        let w = 0xC800_0000
            | u32::from(op) << 16
            | self.vgpr(0)? << 18
            | u32::from(index) << 10
            | u32::from(chan) << 8
            | vsrc;
        Ok(vec![w])
    }

    fn ds(&mut self, op: u16) -> Result<Vec<u32>, String> {
        let base = 0xD800_0000 | u32::from(op) << 18;
        let Fields::Ds {
            dst, addr, data, ..
        } = probe(&[base, 0])
        else {
            unreachable!("a DS opcode")
        };
        self.count(usize::from(dst.is_some()) + usize::from(addr.is_some()) + data.len())?;
        let mut i = 0;
        let mut w1 = 0;
        if dst.is_some() {
            w1 |= self.vgpr(i)? << 24;
            i += 1;
        }
        if addr.is_some() {
            w1 |= self.vgpr(i)?;
            i += 1;
        }
        for (k, shift) in [8, 16].into_iter().enumerate().take(data.len()) {
            w1 |= self.vgpr(i + k)? << shift;
        }
        let offset = self.mods.number("offset")?.unwrap_or(0) as u32 & 0xFFFF;
        let offset0 = self.mods.number("offset0")?.unwrap_or(0) as u32 & 0xFF;
        let offset1 = self.mods.number("offset1")?.unwrap_or(0) as u32 & 0xFF;
        let gds = u32::from(self.mods.flag("gds"));
        Ok(vec![base | gds << 17 | offset | offset1 << 8 | offset0, w1])
    }

    fn buffer(&mut self, encoding: Encoding, op: u16) -> Result<Vec<u32>, String> {
        let op = u32::from(op);
        let format = match self.mods.value("format") {
            None => 0,
            Some(f) => {
                let f = f.trim_start_matches('[').trim_end_matches(']').to_owned();
                (0..128u8)
                    .find(|&i| sp3::buffer_format(i) == f)
                    .map(u32::from)
                    .or_else(|| int(&f).map(|v| v as u32 & 0x7F))
                    .ok_or_else(|| format!("unknown format `{f}`"))?
            }
        };
        let (mut w0, mut w1) = if encoding == Encoding::Mubuf {
            (0xE000_0000 | (op & 0x7F) << 18 | (op >> 7) << 25, 0)
        } else {
            (
                0xE800_0000 | (op & 7) << 16 | format << 19,
                (op >> 3 & 1) << 21,
            )
        };
        let Fields::Buffer { data, .. } = probe(&[w0, w1]) else {
            unreachable!("a buffer opcode")
        };
        let flag = |m: &mut Mods, name: &str, bit: u32| u32::from(m.flag(name)) << bit;
        w0 |= flag(&mut self.mods, "dlc", 15) | flag(&mut self.mods, "glc", 14);
        w1 |= flag(&mut self.mods, "slc", 22) | flag(&mut self.mods, "tfe", 23);
        if encoding == Encoding::Mubuf {
            w0 |= flag(&mut self.mods, "lds", 16);
        }
        if data.is_none() {
            self.count(0)?;
            return Ok(vec![w0, w1]);
        }
        self.count(4)?;
        w0 |= flag(&mut self.mods, "idxen", 13) | flag(&mut self.mods, "offen", 12);
        w0 |= self.mods.number("offset")?.unwrap_or(0) as u32 & 0xFFF;
        let rsrc = self.sgpr(2)?;
        if rsrc % 4 != 0 {
            return Err("a buffer resource starts at a multiple of 4".into());
        }
        w1 |= self.scalar_src(3)? << 24 | rsrc >> 2 << 16 | self.vgpr(0)? << 8;
        w1 |= self.vgpr_or_off(1)?;
        Ok(vec![w0, w1])
    }

    fn image(&mut self, op: u16) -> Result<Vec<u32>, String> {
        let op = u32::from(op);
        let mut w0 = 0xF000_0000 | (op & 0x7F) << 18 | op >> 7;
        let Fields::Image { sampler, .. } = probe(&[w0, 0]) else {
            unreachable!("an image opcode")
        };
        self.count(3 + usize::from(sampler.is_some()))?;
        let addr: Vec<u32> = match self.ops[1]
            .strip_prefix('[')
            .and_then(|a| a.strip_suffix(']'))
        {
            Some(list) => list
                .split(',')
                .map(|r| match reg(r.trim())? {
                    Reg::V(v) => Ok(u32::from(v)),
                    r => Err(format!("an image address is a VGPR, not {r:?}")),
                })
                .collect::<Result<_, String>>()?,
            None => vec![self.vgpr(1)?],
        };
        let extra = (addr.len() - 1).div_ceil(4);
        if extra > 3 {
            return Err("at most 13 image addresses".into());
        }
        let rsrc = self.sgpr(2)?;
        let samp = if sampler.is_some() { self.sgpr(3)? } else { 0 };
        if rsrc % 4 != 0 || samp % 4 != 0 {
            return Err("image descriptors start at a multiple of 4".into());
        }
        let dmask = self.mods.number("dmask")?.unwrap_or(1) as u32 & 0xF;
        let dim = match self.mods.value("dim") {
            None => 0,
            Some(d) => {
                let d = d.strip_prefix("SQ_RSRC_IMG_").unwrap_or(&d).to_owned();
                [
                    "1D",
                    "2D",
                    "3D",
                    "CUBE",
                    "1D_ARRAY",
                    "2D_ARRAY",
                    "2D_MSAA",
                    "2D_MSAA_ARRAY",
                ]
                .iter()
                .position(|n| *n == d)
                .ok_or_else(|| format!("unknown dim `{d}`"))? as u32
            }
        };
        let m = &mut self.mods;
        let flag = |m: &mut Mods, name: &str, bit: u32| u32::from(m.flag(name)) << bit;
        w0 |= flag(m, "slc", 25) | flag(m, "lwe", 17) | flag(m, "tfe", 16) | flag(m, "r128", 15);
        w0 |= flag(m, "glc", 13) | flag(m, "unorm", 12) | flag(m, "dlc", 7);
        w0 |= dmask << 8 | dim << 3 | (extra as u32) << 1;
        let mut w1 = flag(m, "d16", 31) | flag(m, "a16", 30);
        w1 |= samp >> 2 << 21 | rsrc >> 2 << 16 | self.vgpr(0)? << 8 | addr[0];
        let mut words = vec![w0, w1];
        for chunk in addr[1..].chunks(4) {
            words.push(chunk.iter().enumerate().map(|(i, r)| r << (8 * i)).sum());
        }
        Ok(words)
    }

    fn flat(&mut self, encoding: Encoding, op: u16) -> Result<Vec<u32>, String> {
        let seg = match encoding {
            Encoding::Flat => 0,
            Encoding::Scratch => 1,
            _ => 2,
        };
        let mut w0 = 0xDC00_0000 | u32::from(op) << 18 | seg << 14;
        let m = &mut self.mods;
        let flag = |m: &mut Mods, name: &str, bit: u32| u32::from(m.flag(name)) << bit;
        w0 |= flag(m, "slc", 17) | flag(m, "glc", 16) | flag(m, "lds", 13) | flag(m, "dlc", 12);
        let offset = m.number("inst_offset")?.unwrap_or(0) as u32;
        w0 |= offset
            & if encoding == Encoding::Flat {
                0x7FF
            } else {
                0xFFF
            };
        let Fields::Flat {
            dst, data, saddr, ..
        } = probe(&[w0, 0])
        else {
            unreachable!("a FLAT opcode")
        };
        let n = usize::from(dst.is_some())
            + 1
            + usize::from(data.is_some())
            + usize::from(saddr.is_some());
        self.count(n)?;
        let mut i = 0;
        let mut w1 = 0;
        if dst.is_some() {
            w1 |= self.vgpr(0)? << 24;
            i = 1;
        }
        w1 |= self.vgpr_or_off(i)?;
        if data.is_some() {
            w1 |= self.vgpr(i + 1)? << 8;
        }
        if saddr.is_some() {
            let s = match self.reg(n - 1)? {
                Reg::Off => u32::from(NULL),
                _ => self.sgpr(n - 1)?,
            };
            w1 |= s << 16;
        } else {
            w1 |= u32::from(NULL) << 16;
        }
        Ok(vec![w0, w1])
    }

    fn exp(&mut self) -> Result<Vec<u32>, String> {
        self.count(5)?;
        let target = match self.ops[0].as_str() {
            "mrtz" => 8,
            "null" => 9,
            "prim" => 20,
            t => {
                let n = |p: &str| t.strip_prefix(p).and_then(|n| n.parse::<u32>().ok());
                match (n("mrt"), n("pos"), n("param")) {
                    (Some(i), ..) if i < 8 => i,
                    (_, Some(i), _) if i < 4 => 12 + i,
                    (.., Some(i)) if i < 32 => 32 + i,
                    _ => return Err(format!("unknown export target `{t}`")),
                }
            }
        };
        let compr = self.mods.flag("compr");
        let mut w0 = 0xF800_0000 | target << 4;
        let mut w1 = 0;
        for i in 0..4 {
            if self.reg(1 + i)? == Reg::Off {
                continue;
            }
            let slot = if compr { i / 2 } else { i };
            w0 |= 1 << i;
            w1 |= self.vgpr(1 + i)? << (8 * slot);
        }
        w0 |= u32::from(self.mods.flag("vm")) << 12
            | u32::from(self.mods.flag("done")) << 11
            | u32::from(compr) << 10;
        Ok(vec![w0, w1])
    }
}

/// A seeded xorshift64* generator for property tests: the workspace has no
/// property-testing crate, and a failing case is reproduced from its seed.
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        // Zero is xorshift's fixed point; mix the seed so nearby seeds differ.
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub(crate) fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub(crate) fn word(&mut self) -> u32 {
        (self.next() >> 32) as u32
    }

    /// A value in `0..n`.
    pub(crate) fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// A first dword of a random instruction, spread over the encodings.
pub(crate) fn random_word(rng: &mut Rng) -> u32 {
    let r = rng.word();
    match rng.below(19) {
        0 | 1 => r & 0x7FFF_FFFF,
        2 => 0x7E00_0000 | r & 0x01FF_FFFF,
        3 => 0x7C00_0000 | r & 0x01FF_FFFF,
        4 => 0x8000_0000 | r & 0x3FFF_FFFF,
        5 => 0xB000_0000 | r & 0x0FFF_FFFF,
        6 => 0xBE80_0000 | r & 0x007F_FFFF,
        7 => 0xBF00_0000 | r & 0x007F_FFFF,
        8 => 0xBF80_0000 | r & 0x007F_FFFF,
        k => {
            let prefix = [0x32, 0x33, 0x35, 0x36, 0x37, 0x38, 0x3A, 0x3C, 0x3D, 0x3E][k - 9];
            prefix << 26 | r & 0x03FF_FFFF
        }
    }
}

/// `[1,0,1]` as bits, the first lowest.
fn bit_list(text: &str) -> Result<u32, String> {
    let inner = text
        .strip_prefix('[')
        .and_then(|t| t.strip_suffix(']'))
        .ok_or_else(|| format!("bad bit list `{text}`"))?;
    inner
        .split(',')
        .enumerate()
        .try_fold(0, |acc, (i, b)| match b.trim() {
            "0" => Ok(acc),
            "1" => Ok(acc | 1 << i),
            _ => Err(format!("bad bit list `{text}`")),
        })
}

/// `hwreg(NAME[, offset, size])` or a number.
fn hwreg(text: &str) -> Result<u32, String> {
    if let Some(v) = int(text) {
        return Ok(v as u32 & 0xFFFF);
    }
    let bad = || format!("bad hwreg `{text}`");
    let inner = text
        .strip_prefix("hwreg(")
        .and_then(|t| t.strip_suffix(')'))
        .ok_or_else(bad)?;
    let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
    let id = match parts[0] {
        "HW_REG_MODE" => 1,
        "HW_REG_SHADER_FLAT_SCRATCH_LO" => 20,
        "HW_REG_SHADER_FLAT_SCRATCH_HI" => 21,
        "HW_REG_POPS_PACKER" => 25,
        n => int(n).ok_or_else(bad)? as u32,
    };
    let (offset, size) = match parts[1..] {
        [] => (0, 32),
        [o, s] => (
            int(o).ok_or_else(bad)? as u32,
            int(s).ok_or_else(bad)? as u32,
        ),
        _ => return Err(bad()),
    };
    if id > 0x3F || offset > 31 || !(1..=32).contains(&size) {
        return Err(bad());
    }
    Ok(id | offset << 6 | (size - 1) << 11)
}

/// `vmcnt(n) & expcnt(n) & lgkmcnt(n)`, any subset, or a number.
fn waitcnt(text: &str) -> Result<u32, String> {
    if let Some(v) = int(text) {
        return Ok(v as u32 & 0xFFFF);
    }
    let (mut vm, mut exp, mut lgkm) = (0x3F, 7, 0x3F);
    for part in text.split('&').map(str::trim) {
        let (name, n) = part
            .strip_suffix(')')
            .and_then(|p| p.split_once('('))
            .ok_or_else(|| format!("bad wait count `{part}`"))?;
        let n = int(n).ok_or_else(|| format!("bad wait count `{part}`"))? as u32;
        match name {
            "vmcnt" if n <= 0x3F => vm = n,
            "expcnt" if n <= 7 => exp = n,
            "lgkmcnt" if n <= 0x3F => lgkm = n,
            _ => return Err(format!("bad wait count `{part}`")),
        }
    }
    Ok(vm & 0xF | (vm >> 4) << 14 | exp << 4 | lgkm << 8)
}

/// `sendmsg(MSG, OP[, stream])` or a number.
fn sendmsg(text: &str) -> Result<u32, String> {
    if let Some(v) = int(text) {
        return Ok(v as u32 & 0xFFFF);
    }
    let bad = || format!("bad message `{text}`");
    let inner = text
        .strip_prefix("sendmsg(")
        .and_then(|t| t.strip_suffix(')'))
        .ok_or_else(bad)?;
    let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
    let msg = match parts[0] {
        "MSG_GS" => 2,
        "MSG_GS_DONE" => 3,
        "MSG_ORDERED_PS_DONE" => 7,
        "MSG_GS_ALLOC_REQ" => 9,
        n => int(n).ok_or_else(bad)? as u32 & 0xF,
    };
    let op = match parts.get(1).copied() {
        None => 0,
        Some(op) => match ["GS_OP_NOP", "GS_OP_CUT", "GS_OP_EMIT", "GS_OP_EMIT_CUT"]
            .iter()
            .position(|n| *n == op)
        {
            Some(i) => i as u32,
            None => int(op).ok_or_else(bad)? as u32 & 7,
        },
    };
    let stream = match parts.get(2) {
        None => 0,
        Some(s) => int(s).ok_or_else(bad)? as u32 & 3,
    };
    Ok(msg | op << 4 | stream << 8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gfx10::decode;

    fn bytes(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    /// The printer's text of each instruction in `words` (wave32).
    fn printed(words: &[u32]) -> Vec<String> {
        decode(&bytes(words))
            .unwrap_or_else(|e| panic!("decode: {e}"))
            .iter()
            .map(|i| i.text(false))
            .collect()
    }

    /// Assembles `program`, checks that the decoder prints every line back as
    /// written, and that the printed text assembles to the same words.
    fn round_trip(program: &str) {
        let words = asm(program);
        let lines: Vec<&str> = program
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        let text = printed(&words);
        assert_eq!(text, lines, "words {words:08x?}");
        assert_eq!(asm(&text.join("\n")), words, "re-assembling {text:#?}");
    }

    #[test]
    fn an_sgpr_second_source_takes_vop3_and_a_vgpr_one_vop2() {
        assert_eq!(
            asm("v_add_f32 v0, v1, s2\ns_endpgm"),
            [0xD503_0000, 0x0000_0501, 0xBF81_0000]
        );
        assert_eq!(asm("v_add_f32 v0, s2, v1"), [0x0600_0202]);
    }

    /// Words other tests wrote by hand (several checked against llvm-mc)
    /// print and assemble back to themselves, so the encoders here agree with
    /// more than the decoder they were written next to.
    #[test]
    fn hand_encoded_words_assemble_back_to_themselves() {
        let cases: &[&[u32]] = &[
            &[0xBF88_0001, 0xBF81_0000],
            &[0xBF8C_0070],
            &[0xBE80_03FF, 0x1234_5678],
            &[0x8782_027E],
            &[0xF408_0501, 0xFA00_0040],
            &[0xF420_0144, 0x0400_0000],
            &[0xF498_01C2, 0xFA00_0064],
            &[0xF498_1FF4, 0x0A00_0001],
            &[0xF49C_01C4, 0xFA00_0020],
            &[0x7C08_06F0],
            &[0xD401_0106, 0x0001_E501],
            &[0x0202_0480],
            &[0x5006_0801],
            &[0x1004_0AFF, 0x3F00_0001],
            &[0xD54B_8004, 0x2BC0_0501],
            &[0xCC20_0C01, 0x2C10_0702],
            &[0xE034_7010, 0x0102_0402],
            &[0xD8DC_0302, 0x0600_0001],
            &[0xF800_1C0F, 0x0000_0302],
            &[0xF800_00FF, 0x0102_0304],
            &[0xF09C_070A, 0x8024_0801, 0x0000_0005],
            &[0xBE88_1F00],
            &[0x8008_FF08, 20],
            &[0x8209_8009],
            &[0xC808_0000],
            &[0xF800_1883, 0x0000_0100],
            &[0x7E00_02F2],
            &[0xBE81_0380],
            &[0xF40C_0000, 0xFA00_0000],
        ];
        for words in cases {
            let mut code = words.to_vec();
            code.push(0xBF81_0000);
            let text = printed(&code);
            assert_eq!(asm(&text.join("\n")), code, "{text:?}");
        }
    }

    #[test]
    fn scalar_programs_round_trip() {
        round_trip(
            "
            s_mov_b32 s0, lit(0x12345678)
            s_mov_b32 s1, 64
            s_mov_b32 s2, -16
            s_mov_b32 s3, 0.5
            s_mov_b64 s[4:5], exec
            s_and_b64 s[2:3], exec, s[2:3]
            s_add_u32 s8, s8, lit(0x00000014)
            s_addc_u32 s9, s9, 0
            s_lshl_b64 s[10:11], s[10:11], 4
            s_cselect_b32 s18, 1, 0
            s_or_b32 m0, s16, vcc_lo
            s_getpc_b64 s[8:9]
            s_cmp_eq_u32 s0, s1
            s_cmp_lg_u64 s[0:1], 0
            s_bitcmp1_b32 s0, 3
            s_movk_i32 s3, 0x1234
            s_cmpk_eq_u32 s3, 0x0010
            s_getreg_b32 s5, hwreg(HW_REG_MODE)
            s_setreg_b32 hwreg(HW_REG_MODE, 4, 2), s6
            s_setreg_imm32_b32 hwreg(HW_REG_MODE, 0, 4), lit(0x00000003)
            s_waitcnt vmcnt(0) & lgkmcnt(0)
            s_waitcnt lgkmcnt(3)
            s_waitcnt 0x0000
            s_nop 0x0003
            s_barrier
            s_sendmsg sendmsg(MSG_GS_DONE, GS_OP_NOP)
            s_sendmsg sendmsg(MSG_GS, GS_OP_EMIT, 1)
            s_sleep 0x0002
            s_endpgm
            ",
        );
    }

    #[test]
    fn scalar_memory_round_trips() {
        round_trip(
            "
            s_load_dword s0, s[2:3], 0x000010
            s_load_dwordx2 s[4:5], s[2:3], null
            s_load_dwordx4 s[20:23], s[2:3], 0x000040
            s_load_dwordx8 s[8:15], s[0:1], s4
            s_buffer_load_dword s5, s[8:11], s2 offset:0x000004
            s_buffer_load_dwordx4 s[16:19], s[8:11], null glc
            s_load_dword s1, s[6:7], 0x000008 dlc
            s_dcache_inv
            s_memtime s[2:3]
            s_atc_probe 7, s[4:5], 0x000064
            s_endpgm
            ",
        );
    }

    #[test]
    fn branches_use_labels() {
        round_trip(
            "
            s_cmp_eq_u32 s0, 0
            s_cbranch_scc1 label_0010
            v_mov_b32 v0, 1.0
            s_branch label_0014
            v_mov_b32 v0, 2.0
            s_cbranch_execz label_0018
            s_endpgm
            ",
        );
        // Named labels, forward and backward, and a label before an instruction.
        let named = asm("
            top:
                s_sub_u32 s0, s0, 1
                s_cmp_eq_u32 s0, 0
                s_cbranch_scc0 top
                s_branch end
            end: s_endpgm
        ");
        assert_eq!(
            printed(&named),
            [
                "s_sub_u32 s0, s0, 1",
                "s_cmp_eq_u32 s0, 0",
                "s_cbranch_scc0 label_0000",
                "s_branch label_0010",
                "s_endpgm",
            ]
        );
        assert_eq!(named[2] & 0xFFFF, 0xFFFD, "back three dwords");
    }

    #[test]
    fn vector_alu_round_trips() {
        round_trip(
            "
            v_mov_b32 v0, s1
            v_mov_b32 v1, lit(0x3f000001)
            v_mov_b32 v2, -4.0
            v_cvt_f32_u32 v3, v0
            v_readfirstlane_b32 s4, v1
            v_add_f32 v0, v1, s2
            v_mul_f32 v2, lit(0x3f000001), v5
            v_cndmask_b32 v1, 0, v2, vcc_lo
            v_add_co_ci_u32 v3, vcc_lo, s1, v4, vcc_lo
            v_fmamk_f32 v1, v2, lit(0x40400000), v3
            v_fmaak_f32 v1, s2, v3, lit(0x40400000)
            v_cmp_gt_f32 vcc_lo, 0.5, v3
            v_cmpx_eq_u32 exec_lo, s0, v1
            v_cmp_lt_f32 s6, abs(v1), 1.0
            v_cmp_eq_u32 s8, v1, s2
            v_add_f32 v0, -v1, abs(v2) mul:2 clamp
            v_add_f32 v0, v1, s2 div:2
            v_mov_b32 v7, -v1
            v_fma_f32 v4, -v1, s2, 0.5 mul:2 clamp
            v_fma_f32 v4, v1, s2, lit(0x40490fdb)
            v_med3_f32 v1, abs(v2), -abs(v3), v4
            v_cndmask_b32 v1, v2, v3, s4
            v_add_co_u32 v1, s2, v3, v4
            v_add_co_ci_u32 v1, s2, v3, v4, s6
            v_mad_u64_u32 v[2:3], s4, v1, s5, v[6:7]
            v_readlane_b32 s1, v2, 3
            v_writelane_b32 v1, s2, 3
            v_lshlrev_b64 v[0:1], 2, v[2:3]
            v_add_f64 v[0:1], v[2:3], -v[4:5]
            v_mul_lo_u32 v1, v2, s3
            v_bfe_u32 v0, v1, 8, 4
            v_perm_b32 v0, v1, v2, lit(0x05040100)
            v_cvt_f16_f32 v1, sel_hi(v2)
            v_pk_add_f16 v0, v1, v2
            v_pk_mul_f16 v0, sel_lo(v1), sel_hi(-v2)
            v_pk_fma_f16 v0, v1, sel_hi_lo(v2), v3 clamp
            v_fma_mix_f32 v1, -sel_hi(v2), s3, abs(v4)
            v_fma_mixlo_f16 v1, sel_lo(v2), v3, v4
            v_interp_p1_f32 v2, v0, attr0.x
            v_interp_p2_f32 v2, v1, attr3.w
            v_interp_mov_f32 v3, p0, attr1.y
            s_endpgm
            ",
        );
    }

    #[test]
    fn forced_encodings_follow_the_suffix() {
        let short = asm("v_add_f32 v0, v1, v2");
        let long = asm("v_add_f32_e64 v0, v1, v2");
        assert_eq!(short.len(), 1);
        assert_eq!(long, [0xD503_0000, 0x0002_0501]);
        assert_eq!(
            printed(&[long[0], long[1], 0xBF81_0000])[0],
            "v_add_f32 v0, v1, v2"
        );
        assert!(assemble("v_add_f32_e32 v0, s1, v2").is_ok());
        assert!(assemble("v_add_f32_e32 v0, v1, s2").is_err());
        // An SGPR second source has no VOP2 form: VOP3 without the suffix.
        assert_eq!(asm("v_add_f32 v0, s1, s2").len(), 2);
    }

    #[test]
    fn op_sel_sets_the_destination_half() {
        let w = asm("v_interp_p2_f16 v2, v1, attr0.x, v2 op_sel:[0,0,0,1] mul:2 clamp");
        assert_eq!(w[0] >> 11 & 0xF, 0b1000);
        assert_eq!(w[0] >> 15 & 1, 1);
        assert_eq!(w[1] >> 27 & 3, 1);
    }

    #[test]
    fn lds_round_trips() {
        round_trip(
            "
            ds_write_b32 v1, v2
            ds_write_b32 v1, v2 offset:256
            ds_read_b32 v3, v1 offset:4
            ds_read_b64 v[4:5], v1
            ds_read2_b32 v[6:7], v1 offset0:2 offset1:3
            ds_write2_b32 v1, v2, v3 offset1:1
            ds_add_u32 v1, v2
            ds_add_rtn_u32 v3, v1, v2
            ds_cmpst_b32 v1, v2, v3
            ds_swizzle_b32 v4, v1 offset:32799
            ds_bpermute_b32 v2, v1, v3
            ds_min_f32 v1, v2
            ds_append v5 gds
            s_endpgm
            ",
        );
    }

    #[test]
    fn buffer_and_image_round_trip() {
        round_trip(
            "
            buffer_load_dword v1, v0, s[8:11], 0 offen
            buffer_load_dwordx2 v[4:5], v[2:3], s[8:11], s1 idxen offen offset:16 glc
            buffer_load_dwordx4 v[4:7], v0, s[12:15], s2 offset:4095 slc
            buffer_store_dword v1, v0, s[8:11], 0 offset:8
            buffer_store_dwordx2 v[2:3], v0, s[8:11], 0 idxen dlc
            buffer_atomic_add v1, v0, s[8:11], 0 offen glc
            buffer_load_format_xyzw v[0:3], v4, s[4:7], 0 idxen
            buffer_gl0_inv
            tbuffer_load_format_x v1, v0, s[8:11], 0 offen format:[BUF_FMT_32_FLOAT]
            image_sample v[0:3], v[4:5], s[8:15], s[16:19] dmask:0xf dim:SQ_RSRC_IMG_2D
            image_sample_lz v[8:9], [v1,v5], s[16:23], s[4:7] dmask:0x7 dim:SQ_RSRC_IMG_2D d16
            image_sample_l v0, [v1,v2,v7], s[8:15], s[16:19] dim:SQ_RSRC_IMG_2D
            image_load v[0:3], v[4:5], s[8:15] dmask:0xf dim:SQ_RSRC_IMG_2D unorm
            image_store v[0:1], v[4:6], s[8:15] dmask:0x3 dim:SQ_RSRC_IMG_3D unorm glc
            image_load_mip v0, v[1:3], s[8:11] dim:SQ_RSRC_IMG_2D r128 unorm
            image_get_resinfo v[0:3], v4, s[8:15] dmask:0xf dim:SQ_RSRC_IMG_2D unorm
            s_endpgm
            ",
        );
    }

    #[test]
    fn global_flat_scratch_and_export_round_trip() {
        round_trip(
            "
            global_load_dword v1, v[2:3], off
            global_load_dwordx4 v[4:7], v2, s[4:5] inst_offset:-16
            global_store_dword v[2:3], v1, off inst_offset:256 glc slc
            global_atomic_add v0, v[2:3], v1, off glc
            global_atomic_add v[2:3], v1, off
            flat_load_dword v1, v[2:3]
            flat_store_dwordx2 v[2:3], v[4:5] inst_offset:8
            scratch_load_dword v1, v2, off
            scratch_store_dword off, v1, s4 inst_offset:4
            exp mrt0, v0, v1, v2, v3 done vm
            exp mrt0, v2, v2, v3, v3 done compr vm
            exp mrtz, v0, v1, off, off done vm
            exp pos0, v4, v5, v6, v7
            exp param3, v0, off, off, off
            exp pos3, v4, v3, v2, v1
            s_endpgm
            ",
        );
    }

    /// Printing is a fixed point of assembling: any instruction the printer
    /// writes in a form this assembler takes assembles to words that print
    /// the same. Random words reach every opcode and operand kind, including
    /// field values no hand-written program uses.
    #[test]
    fn printed_random_instructions_assemble_to_the_same_text() {
        let mut rng = Rng::new(0x5EED);
        let (mut tried, mut assembled) = (0, 0);
        for case in 0..20_000 {
            let mut words = vec![random_word(&mut rng)];
            words.extend((0..4).map(|_| rng.word()));
            let Ok(inst) = crate::gfx10::instruction(&bytes(&words), 0) else {
                continue;
            };
            let wave64 = case % 2 == 1;
            let text = inst.text(wave64);
            // `-39` is the printer's negated inline constant 39 but reads as
            // the literal -39 here, as a test author means it.
            let negated_constant = text
                .as_bytes()
                .windows(3)
                .any(|w| matches!(w[0], b' ' | b'(') && w[1] == b'-' && w[2].is_ascii_digit());
            if text.contains('?') || text.contains('<') || inst.name().is_none() || negated_constant
            {
                continue;
            }
            tried += 1;
            // Not taken: SDWA and DPP forms.
            let Ok(again) = assemble(&text) else {
                continue;
            };
            assembled += 1;
            let back = first_instruction(&again)
                .unwrap_or_else(|e| panic!("{text}: {again:08x?} does not decode: {e}"));
            assert_eq!(
                back.text(wave64),
                text,
                "case {case}: {:08x?} -> {again:08x?}",
                inst.words
            );
        }
        // Most printed instructions are in forms the assembler takes.
        assert!(
            assembled * 4 > tried * 3,
            "only {assembled} of {tried} printed instructions assembled"
        );
    }

    #[test]
    fn bad_text_names_the_line() {
        for (text, needle) in [
            ("s_mov_b32 s0", "line 1: `s_mov_b32` takes 2 operands"),
            ("s_endpgm\nv_frobnicate v0", "line 2: unknown opcode"),
            ("v_add_f32 v0, v1, v2 glc", "unexpected modifier `glc`"),
            ("s_branch nowhere", "unknown label"),
            ("v_add_f32 v0, lit(1), lit(2)", "two different literals"),
            ("s_mov_b32 s0, v1", "cannot be a source"),
        ] {
            let e = assemble(text).unwrap_err();
            assert!(e.contains(needle), "{text:?}: {e}");
        }
    }
}
