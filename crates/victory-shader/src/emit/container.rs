//! The PC container around emitted DXIL: feature flags (`SFI0`), the
//! signatures (`ISG1`, `OSG1`), pipeline state validation (`PSV0`) and the
//! program (`DXIL`), laid out as validator version 1.6 lays them out, plus
//! the container hash that D3D12 checks.

/// One signature element: one row (D3D12 links stages by semantic).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SigElement {
    pub name: String,
    pub index: u32,
    /// DXIL semantic kind (0 arbitrary, 1 vertex id, 3 position, 16 target...).
    pub kind: u8,
    /// DXIL component type (4 i32, 5 u32, 9 f32, 1 i1).
    pub comp: u8,
    /// DXIL interpolation mode (0 for none).
    pub interp: u8,
    /// Start row, if the element has a register.
    pub row: Option<u32>,
    pub col: u8,
    pub cols: u8,
    /// Components read (inputs) or written (outputs), relative to `col`.
    pub usage: u8,
    /// 16-bit on the console (declared 32-bit here).
    pub half: bool,
}

impl SigElement {
    fn system_value(&self) -> u32 {
        match self.kind {
            1 => 6,  // vertex id
            2 => 8,  // instance id
            3 => 1,  // position
            4 => 4,  // render target array index
            5 => 5,  // viewport array index
            6 => 2,  // clip distance
            7 => 3,  // cull distance
            10 => 7, // primitive id
            12 => 10,
            13 => 9, // front face
            14 => 66,
            15 => 70,
            16 => 64, // target
            17 => 65, // depth
            18 => 68,
            19 => 67,
            20 => 69,
            30 => 25, // cull primitive
            _ => 0,
        }
    }

    fn sig_comp(&self) -> u32 {
        match self.comp {
            1 | 5 => 1, // bool and u32
            4 => 2,
            9 => 3,
            _ => 0,
        }
    }

    fn mask(&self) -> u8 {
        ((1u8 << self.cols) - 1) << self.col
    }
}

/// `ISG1`/`OSG1`: 32-byte rows, then the names.
pub(crate) fn signature(elements: &[SigElement], input: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend((elements.len() as u32).to_le_bytes());
    out.extend(8u32.to_le_bytes());
    // Validator 1.6 writes one name per element in element order, repeats
    // included, except that system values share theirs (it keys names by
    // the semantic's string, which is one static string per system value);
    // the rows sorted by register (unallocated last).
    let mut offsets: Vec<u32> = Vec::new();
    let mut next = 8 + 32 * elements.len() as u32;
    let mut names = Vec::new();
    for (i, e) in elements.iter().enumerate() {
        let shared = (e.kind != 0)
            .then(|| (0..i).find(|&j| elements[j].kind == e.kind))
            .flatten();
        match shared {
            Some(j) => offsets.push(offsets[j]),
            None => {
                offsets.push(next);
                next += e.name.len() as u32 + 1;
                names.push(&e.name);
            }
        }
    }
    let mut order: Vec<usize> = (0..elements.len()).collect();
    order.sort_by_key(|&i| (elements[i].row.unwrap_or(u32::MAX), elements[i].col));
    for i in order {
        let (e, offset) = (&elements[i], offsets[i]);
        let used = e.usage << e.col;
        let rw = if input { used } else { !used & 0xf };
        for v in [
            0,
            offset,
            e.index,
            e.system_value(),
            e.sig_comp(),
            e.row.unwrap_or(u32::MAX),
        ] {
            out.extend(v.to_le_bytes());
        }
        out.extend([e.mask(), rw, 0, 0]);
        out.extend(0u32.to_le_bytes());
    }
    for name in names {
        out.extend(name.as_bytes());
        out.push(0);
    }
    out
}

/// A resource bound by register, as `PSV0` lists it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Binding {
    /// PSV resource type: 1 sampler, 2 CBV.
    pub kind: u32,
    pub space: u32,
    pub register: u32,
    /// DXIL resource kind (13 cbuffer, 14 sampler).
    pub dxil_kind: u32,
}

pub(crate) struct Psv<'a> {
    /// PSV shader kind: 0 pixel, 1 vertex, 5 compute.
    pub stage: u8,
    /// Vertex: writes `SV_Position`. Pixel: writes depth.
    pub flag: bool,
    pub inputs: &'a [SigElement],
    pub outputs: &'a [SigElement],
    /// Mesh: per-primitive outputs.
    pub prims: &'a [SigElement],
    /// Mesh: its state and the group-shared bytes it declares.
    pub mesh: Option<(super::shader::Mesh, u32)>,
    pub bindings: &'a [Binding],
    /// Compute: thread-group size and required wave size.
    pub threads: [u32; 3],
    pub wave_size: Option<u32>,
}

/// `PSV0` with runtime info version 2 (validator 1.6).
pub(crate) fn psv(p: &Psv) -> Vec<u8> {
    let vectors = |els: &[SigElement]| {
        els.iter()
            .filter_map(|e| e.row)
            .map(|r| r + 1)
            .max()
            .unwrap_or(0)
    };
    let (in_vectors, out_vectors) = (vectors(p.inputs), vectors(p.outputs));
    let mut out = Vec::new();
    let u32s = |out: &mut Vec<u8>, vs: &[u32]| {
        for v in vs {
            out.extend(v.to_le_bytes());
        }
    };
    u32s(&mut out, &[48]);
    let mut info = [0u8; 48];
    info[0] = u8::from(p.flag);
    if p.stage == 0 {
        // PSInfo.SampleFrequency: an input interpolated per sample, or the
        // sample index.
        info[1] = u8::from(
            p.inputs
                .iter()
                .any(|e| matches!(e.interp, 6 | 7) || e.kind == 12),
        );
    }
    let (min, max) = p.wave_size.map_or((0, u32::MAX), |n| (n, n));
    info[16..20].copy_from_slice(&min.to_le_bytes());
    info[20..24].copy_from_slice(&max.to_le_bytes());
    info[24] = p.stage;
    for (k, n) in p.threads.into_iter().enumerate() {
        info[36 + 4 * k..40 + 4 * k].copy_from_slice(&n.to_le_bytes());
    }
    info[28] = p.inputs.len() as u8;
    info[29] = p.outputs.len() as u8;
    info[31] = in_vectors as u8;
    info[32] = out_vectors as u8;
    if let Some((mesh, shared)) = p.mesh {
        info[0..4].copy_from_slice(&shared.to_le_bytes());
        info[12..14].copy_from_slice(&(mesh.max_vertices as u16).to_le_bytes());
        info[14..16].copy_from_slice(&(mesh.max_primitives as u16).to_le_bytes());
        info[26] = vectors(p.prims) as u8;
        info[27] = mesh.topology;
        info[30] = p.prims.len() as u8;
    }
    out.extend(info);
    u32s(&mut out, &[p.bindings.len() as u32]);
    if !p.bindings.is_empty() {
        u32s(&mut out, &[24]);
        for b in p.bindings {
            u32s(
                &mut out,
                &[b.kind, b.space, b.register, b.register, b.dxil_kind, 0],
            );
        }
    }
    // Names of arbitrary semantics (system values use ""), then indices.
    let mut strings = vec![0u8];
    let mut indices: Vec<u32> = Vec::new();
    let mut elements = Vec::new();
    for e in p.inputs.iter().chain(p.outputs).chain(p.prims) {
        let name = if e.kind == 0 {
            let needle: Vec<u8> = e.name.bytes().chain([0]).collect();
            match find(&strings, &needle) {
                Some(at) => at,
                None => {
                    strings.extend(&needle);
                    strings.len() - needle.len()
                }
            }
        } else {
            0
        };
        let index = match indices.iter().position(|&i| i == e.index) {
            Some(at) => at,
            None => {
                indices.push(e.index);
                indices.len() - 1
            }
        };
        let (start, alloc) = match e.row {
            Some(r) => (r as u8, 0x40 | (e.col << 4)),
            None => (0, 0),
        };
        elements.push((name as u32, index as u32, start, e.cols | alloc, e));
    }
    strings.resize(strings.len().next_multiple_of(4), 0);
    u32s(&mut out, &[strings.len() as u32]);
    out.extend(&strings);
    u32s(&mut out, &[indices.len() as u32]);
    u32s(&mut out, &indices);
    if !elements.is_empty() {
        u32s(&mut out, &[16]);
        for (name, index, start, cols, e) in elements {
            u32s(&mut out, &[name, index]);
            out.extend([1, start, cols, e.kind, e.sig_comp() as u8, e.interp, 0, 0]);
        }
    }
    // Input-to-output dependencies, as the view-id state declares them:
    // every output scalar on every input scalar.
    if in_vectors > 0 && out_vectors > 0 {
        let scalars = |els: &[SigElement]| {
            els.iter()
                .filter_map(|e| e.row.map(|r| r * 4 + u32::from(e.col + e.cols)))
                .max()
                .unwrap_or(0)
        };
        let (ni, no) = (scalars(p.inputs), scalars(p.outputs));
        let per = (out_vectors * 4).div_ceil(32);
        for i in 0..in_vectors * 4 {
            for k in 0..per {
                let bits = if i < ni {
                    no.saturating_sub(32 * k).min(32)
                } else {
                    0
                };
                let mask = if bits == 32 {
                    u32::MAX
                } else {
                    (1u32 << bits) - 1
                };
                u32s(&mut out, &[mask]);
            }
        }
    }
    out
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The container: parts in order, hashed.
pub(crate) fn build(parts: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
    let table = 0x20 + 4 * parts.len();
    let total = table + parts.iter().map(|(_, d)| 8 + d.len()).sum::<usize>();
    let mut out = Vec::with_capacity(total);
    out.extend(b"DXBC");
    out.extend([0; 16]);
    out.extend(1u16.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out.extend((total as u32).to_le_bytes());
    out.extend((parts.len() as u32).to_le_bytes());
    let mut offset = table;
    for (_, d) in parts {
        out.extend((offset as u32).to_le_bytes());
        offset += 8 + d.len();
    }
    for (fourcc, d) in parts {
        out.extend(*fourcc);
        out.extend((d.len() as u32).to_le_bytes());
        out.extend(*d);
    }
    let digest = hash(&out[20..]);
    out[4..20].copy_from_slice(&digest);
    out
}

/// The container hash D3D12 checks: MD5 whose last block carries the bit
/// count first and `2 × bytes + 1` last instead of standard padding.
pub(crate) fn hash(data: &[u8]) -> [u8; 16] {
    let len = data.len();
    let left = len % 64;
    let mut tail = Vec::with_capacity(128);
    if left < 56 {
        tail.extend(((len as u32) << 3).to_le_bytes());
        tail.extend(&data[len - left..]);
        tail.push(0x80);
        tail.resize(60, 0);
    } else {
        tail.extend(&data[len - left..]);
        tail.push(0x80);
        tail.resize(64, 0);
        tail.extend(((len as u32) << 3).to_le_bytes());
        tail.resize(124, 0);
    }
    tail.extend(((len as u32) << 1 | 1).to_le_bytes());
    let mut state = [0x6745_2301u32, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    for block in data[..len - left]
        .as_chunks::<64>()
        .0
        .iter()
        .chain(tail.as_chunks::<64>().0)
    {
        md5_block(&mut state, block);
    }
    let mut out = [0; 16];
    for (o, s) in out.as_chunks_mut::<4>().0.iter_mut().zip(state) {
        o.copy_from_slice(&s.to_le_bytes());
    }
    out
}

fn md5_block(state: &mut [u32; 4], block: &[u8; 64]) {
    const S: [u32; 16] = [7, 12, 17, 22, 5, 9, 14, 20, 4, 11, 16, 23, 6, 10, 15, 21];
    let x: Vec<u32> = block
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
        .collect();
    let [mut a, mut b, mut c, mut d] = *state;
    for i in 0..64 {
        let (f, g) = match i / 16 {
            0 => ((b & c) | (!b & d), i),
            1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
            2 => (b ^ c ^ d, (3 * i + 5) % 16),
            _ => (c ^ (b | !d), (7 * i) % 16),
        };
        let k = ((i as f64 + 1.0).sin().abs() * 4_294_967_296.0) as u32;
        let rotated = a
            .wrapping_add(f)
            .wrapping_add(k)
            .wrapping_add(x[g])
            .rotate_left(S[(i / 16) * 4 + i % 4]);
        (a, b, c, d) = (d, b.wrapping_add(rotated), b, c);
    }
    for (s, v) in state.iter_mut().zip([a, b, c, d]) {
        *s = s.wrapping_add(v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_core_matches_the_standard() {
        // Standard MD5 of "abc" with standard padding, through md5_block.
        let mut block = b"abc".to_vec();
        block.push(0x80);
        block.resize(56, 0);
        block.extend(24u64.to_le_bytes());
        let mut state = [0x6745_2301u32, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
        md5_block(&mut state, block.as_slice().try_into().unwrap());
        let hex: String = state
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(hex, "900150983cd24fb0d6963f7d28e17f72");
    }

    fn sig(
        name: &str,
        index: u32,
        kind: u8,
        row: Option<u32>,
        (col, cols, usage): (u8, u8, u8),
        interp: u8,
    ) -> SigElement {
        SigElement {
            name: name.into(),
            index,
            kind,
            comp: 9,
            interp,
            row,
            col,
            cols,
            usage,
            half: false,
        }
    }

    fn read_u32(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
    }

    fn read_cstr(bytes: &[u8], at: usize) -> &str {
        let end = bytes[at..].iter().position(|&b| b == 0).unwrap();
        std::str::from_utf8(&bytes[at..at + end]).unwrap()
    }

    #[test]
    fn build_and_hash_cover_both_padding_branches() {
        fn ref_hash(data: &[u8]) -> [u8; 16] {
            let len = data.len();
            let left = len % 64;
            let mut tail = Vec::new();
            if left < 56 {
                tail.extend(((len as u32) << 3).to_le_bytes());
                tail.extend(&data[len - left..]);
                tail.push(0x80);
                tail.resize(60, 0);
            } else {
                tail.extend(&data[len - left..]);
                tail.push(0x80);
                tail.resize(64, 0);
                tail.extend(((len as u32) << 3).to_le_bytes());
                tail.resize(124, 0);
            }
            tail.extend((((len as u32) << 1) | 1).to_le_bytes());
            let mut state = [0x6745_2301u32, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
            for chunk in data[..len - left]
                .as_chunks::<64>()
                .0
                .iter()
                .chain(tail.as_chunks::<64>().0)
            {
                md5_block(&mut state, chunk);
            }
            let mut out = [0u8; 16];
            for (o, s) in out.as_chunks_mut::<4>().0.iter_mut().zip(state) {
                o.copy_from_slice(&s.to_le_bytes());
            }
            out
        }

        // With 1 part, `out[20..]` has 24 header/part-table/part-header bytes before payload:
        // 31 -> 55 (left < 56), 32 -> 56 (left >= 56), 40 -> 64 (full block + left == 0).
        for payload_len in [31usize, 32, 40] {
            let payload: Vec<u8> = (0..payload_len).map(|i| (i * 7 + 3) as u8).collect();
            let built = build(&[(b"TEST", &payload)]);
            assert_eq!(built[20..].len(), 24 + payload_len);
            let parsed = crate::dxbc::Container::parse(&built).unwrap();
            assert_eq!(parsed.version, (1, 0));
            assert_eq!(parsed.digest, hash(&built[20..]));
            assert_eq!(parsed.digest, ref_hash(&built[20..]));
            assert_eq!(
                parsed.part(crate::dxbc::FourCC(*b"TEST")).unwrap().data,
                payload.as_slice()
            );

            let mut flipped = built[20..].to_vec();
            flipped[24] ^= 0x01;
            assert_ne!(hash(&flipped), parsed.digest);
        }
        assert_eq!(hash(&[0xAB; 55]), ref_hash(&[0xAB; 55]));
        assert_eq!(hash(&[0xCD; 56]), ref_hash(&[0xCD; 56]));
    }

    #[test]
    fn signature_sorts_rows_shares_system_names_and_encodes_masks() {
        let elements = [
            sig("SV_ClipDistance", 0, 6, Some(2), (0, 2, 0b11), 0),
            sig("SV_ClipDistance", 1, 6, Some(1), (2, 1, 0b01), 0),
            sig("SV_VertexID", 0, 1, None, (0, 1, 0b01), 0),
            sig("TEXCOORD", 0, 0, Some(0), (0, 4, 0b0101), 2),
            sig("TEXCOORD", 1, 0, Some(1), (0, 2, 0b10), 2),
        ];
        for (input, expected_rw) in [
            (true, [0b0101u8, 0b0010, 0b0100, 0b0011, 0b0001]),
            (false, [0b1010u8, 0b1101, 0b1011, 0b1100, 0b1110]),
        ] {
            let bytes = signature(&elements, input);
            assert_eq!(read_u32(&bytes, 0), 5);
            assert_eq!(read_u32(&bytes, 4), 8);

            let row_at = |idx: usize| 8 + idx * 32;
            let expected = [
                (0u32, 0u32, 0b1111u8, 0u32),       // row 0, col 0: TEXCOORD 0
                (1, 0, 0b0011, 1),                  // row 1, col 0: TEXCOORD 1
                (1, 2, 0b0100, 1),                  // row 1, col 2: SV_ClipDistance 1
                (2, 2, 0b0011, 0),                  // row 2, col 0: SV_ClipDistance 0
                (u32::MAX, 6, 0b0001, 0),           // unallocated: SV_VertexID 0
            ];
            let mut name_offsets = Vec::new();
            for (i, &(row, sys_val, mask, index)) in expected.iter().enumerate() {
                let r = row_at(i);
                let name_off = read_u32(&bytes, r + 4) as usize;
                name_offsets.push(name_off);
                assert_eq!(read_u32(&bytes, r + 8), index);
                assert_eq!(read_u32(&bytes, r + 12), sys_val);
                assert_eq!(read_u32(&bytes, r + 20), row);
                assert_eq!(bytes[r + 24], mask);
                assert_eq!(bytes[r + 25], expected_rw[i]);
            }
            // Shared string offset for repeated system values; distinct for repeated arbitrary semantics.
            assert_eq!(name_offsets[2], name_offsets[3]);
            assert_eq!(read_cstr(&bytes, name_offsets[2]), "SV_ClipDistance");
            assert_ne!(name_offsets[0], name_offsets[1]);
            assert_eq!(read_cstr(&bytes, name_offsets[0]), "TEXCOORD");
            assert_eq!(read_cstr(&bytes, name_offsets[1]), "TEXCOORD");
            assert_eq!(read_cstr(&bytes, name_offsets[4]), "SV_VertexID");
        }
    }

    #[test]
    fn psv_aligns_strings_and_writes_dependency_masks() {
        let inputs = [
            sig("SV_Position", 0, 3, Some(0), (0, 4, 0b1111), 4),
            sig("COLOR", 0, 0, Some(1), (0, 2, 0b0011), 6),
        ];
        let outputs = [sig("SV_Target", 0, 16, Some(0), (0, 4, 0b1111), 0)];
        let bindings = [Binding {
            kind: 2,
            space: 0,
            register: 3,
            dxil_kind: 13,
        }];
        let bytes = psv(&Psv {
            stage: 0,
            flag: true,
            inputs: &inputs,
            outputs: &outputs,
            prims: &[],
            mesh: None,
            bindings: &bindings,
            threads: [0, 0, 0],
            wave_size: Some(32),
        });

        assert_eq!(read_u32(&bytes, 0), 48);
        let info = &bytes[4..52];
        assert_eq!(info[0], 1); // flag
        assert_eq!(info[1], 1); // PSInfo.SampleFrequency (interp == 6)
        assert_eq!(read_u32(info, 16), 32); // wave_size min
        assert_eq!(read_u32(info, 20), 32); // wave_size max
        assert_eq!(info[24], 0); // stage: pixel
        assert_eq!(info[28], 2); // input element count
        assert_eq!(info[29], 1); // output element count
        assert_eq!(info[31], 2); // in_vectors
        assert_eq!(info[32], 1); // out_vectors

        let mut at = 52usize;
        assert_eq!(read_u32(&bytes, at), 1); // binding count
        assert_eq!(read_u32(&bytes, at + 4), 24); // binding stride
        assert_eq!(read_u32(&bytes, at + 8), 2); // kind
        assert_eq!(read_u32(&bytes, at + 16), 3); // register
        assert_eq!(read_u32(&bytes, at + 24), 13); // dxil_kind
        at += 8 + 24;

        let str_len = read_u32(&bytes, at) as usize;
        at += 4;
        assert_eq!(str_len % 4, 0);
        let strings = &bytes[at..at + str_len];
        assert_eq!(strings[0], 0);
        assert_eq!(read_cstr(strings, 1), "COLOR");
        at += str_len;

        let idx_count = read_u32(&bytes, at) as usize;
        at += 4;
        assert_eq!(idx_count, 1);
        assert_eq!(read_u32(&bytes, at), 0);
        at += 4 * idx_count;

        assert_eq!(read_u32(&bytes, at), 16); // signature element stride
        at += 4;
        // System value uses offset 0 (""); arbitrary COLOR points to offset 1.
        assert_eq!(read_u32(&bytes, at), 0);
        assert_eq!(read_u32(&bytes, at + 16), 1);
        at += 3 * 16;

        // 2 input vectors -> 8 input scalars (`ni = 6`, `no = 4` -> first 6 get 0xf, last 2 get 0).
        let deps: Vec<u32> = (0..8).map(|k| read_u32(&bytes, at + 4 * k)).collect();
        assert_eq!(deps, [0xF, 0xF, 0xF, 0xF, 0xF, 0xF, 0, 0]);
        assert_eq!(at + 8 * 4, bytes.len());

        // Plain interpolation leaves SampleFrequency == 0; repeated arbitrary names share string offset.
        let plain_inputs = [
            sig("TEXCOORD", 0, 0, Some(0), (0, 2, 0b11), 2),
            sig("TEXCOORD", 1, 0, Some(1), (0, 2, 0b11), 2),
        ];
        let plain = psv(&Psv {
            stage: 0,
            flag: false,
            inputs: &plain_inputs,
            outputs: &outputs,
            prims: &[],
            mesh: None,
            bindings: &[],
            threads: [0, 0, 0],
            wave_size: None,
        });
        assert_eq!(plain[5], 0);
        let plain_str_len = read_u32(&plain, 56) as usize;
        assert_eq!(plain_str_len % 4, 0);
        // "\0TEXCOORD\0" = 10 bytes, padded to 12; deduplicated (not emitted twice).
        assert_eq!(plain_str_len, 12);

        // Mesh metadata populates shared memory, max vertices/primitives, prims count & topology.
        let prims = [sig("SV_CullPrimitive", 0, 30, Some(0), (0, 1, 0b1), 0)];
        let mesh_psv = psv(&Psv {
            stage: 13,
            flag: false,
            inputs: &[],
            outputs: &outputs,
            prims: &prims,
            mesh: Some((
                crate::emit::shader::Mesh {
                    max_vertices: 64,
                    max_primitives: 126,
                    topology: 2,
                },
                4096,
            )),
            bindings: &[],
            threads: [32, 1, 1],
            wave_size: None,
        });
        let minfo = &mesh_psv[4..52];
        assert_eq!(read_u32(minfo, 0), 4096);
        assert_eq!(u16::from_le_bytes(minfo[12..14].try_into().unwrap()), 64);
        assert_eq!(u16::from_le_bytes(minfo[14..16].try_into().unwrap()), 126);
        assert_eq!(minfo[26], 1); // prim_vectors
        assert_eq!(minfo[27], 2); // topology
        assert_eq!(minfo[30], 1); // prims.len()
    }
}
