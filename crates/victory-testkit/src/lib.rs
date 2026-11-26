//! Synthetic PE32+ x64 images for tests, built in memory.
//!
//! Loader tests need images with exactly the parts a case is about (an
//! import table, exports, TLS callbacks, unwind data, an entry point), and
//! nothing of any game. [`PeBuilder`] lays them out as a linker would:
//!
//! | section | holds |
//! |---------|-------|
//! | `.text` | the code given, each piece 16-byte aligned, from RVA 0x1000 |
//! | `.rdata` | import descriptors, lookup and address tables, names; the export directory; unwind info |
//! | `.data` | the data given; the TLS template, index, directory and callback array |
//! | extra | each [`PeBuilder::section`], in order |
//! | `.pdata` | one function entry per [`PeBuilder::function`] |
//! | `.reloc` | a `DIR64` relocation for every address the builder wrote |
//!
//! Sections without content are left out. The images load with
//! `LoadLibraryExW` at any base.
//!
//! ```
//! use victory_testkit::{PeBuilder, Unwind};
//! let mut pe = PeBuilder::dll("probe.dll");
//! // mov eax, 0x1234; ret
//! let f = pe.code(&[0xB8, 0x34, 0x12, 0, 0, 0xC3]);
//! pe.export("probe", f);
//! pe.function(f, 6, Unwind::leaf());
//! let image = pe.build();
//! assert_eq!(&image.bytes[..2], b"MZ");
//! ```

/// Where the first section starts; [`Code`] RVAs are relative to it.
pub const TEXT_RVA: u32 = 0x1000;
const SECTION_ALIGNMENT: u32 = 0x1000;
const FILE_ALIGNMENT: u32 = 0x200;

/// A piece of code in `.text`, by its offset there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Code(u32);

impl Code {
    pub fn rva(self) -> u32 {
        TEXT_RVA + self.0
    }

    /// `bytes` further into the same piece.
    pub fn offset(self, bytes: u32) -> Code {
        Code(self.0 + bytes)
    }
}

/// One imported function, by its place in the import list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Import(usize);

/// Data in `.data`, by its offset there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Data(u32);

/// What a rip-relative displacement in code points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// The import's address-table slot (`call [rip+disp32]` calls it).
    Import(Import),
    Data(Data),
    Code(Code),
}

/// The unwind info of one function: `UNWIND_INFO` version 1, no handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unwind {
    pub prolog: u8,
    /// Unwind codes, last prolog instruction first, as the format wants.
    pub codes: Vec<u16>,
}

impl Unwind {
    /// A function that touches neither the stack nor non-volatile registers.
    pub fn leaf() -> Unwind {
        Unwind {
            prolog: 0,
            codes: Vec::new(),
        }
    }

    /// A prolog that is one `sub rsp, size` (8 to 128, a multiple of 8)
    /// ending `prolog` bytes into the function.
    pub fn alloc_small(prolog: u8, size: u8) -> Unwind {
        assert!(
            (8..=128).contains(&size) && size.is_multiple_of(8),
            "UWOP_ALLOC_SMALL covers 8 to 128 bytes"
        );
        // UWOP_ALLOC_SMALL (2), operation info (size - 8) / 8.
        let code = u16::from(prolog) | (2 << 8) | (u16::from((size - 8) / 8) << 12);
        Unwind {
            prolog,
            codes: vec![code],
        }
    }
}

struct Piece {
    at: u32,
    /// (offset of a disp32 in the piece, target); the displacement is
    /// relative to the end of those four bytes.
    fixups: Vec<(u32, Target)>,
}

/// An image under construction. See the crate documentation.
pub struct PeBuilder {
    name: String,
    dll: bool,
    image_base: u64,
    subsystem: u16,
    stack_reserve: u64,
    text: Vec<u8>,
    pieces: Vec<Piece>,
    data: Vec<u8>,
    imports: Vec<(String, String)>,
    exports: Vec<(String, Code)>,
    entry: Option<Code>,
    tls: Option<(Vec<u8>, Vec<Code>)>,
    functions: Vec<(Code, u32, Unwind)>,
    extra: Vec<(String, Vec<u8>, u32)>,
}

/// The image built, with where its parts landed.
pub struct Image {
    pub bytes: Vec<u8>,
    /// The IAT slot RVA of each [`Import`], in import order.
    pub iat_slots: Vec<u32>,
    /// `.data`'s RVA (0 if the image has no `.data`).
    pub data_rva: u32,
    /// Each extra section's name and RVA.
    pub sections: Vec<(String, u32)>,
}

impl Image {
    pub fn iat_slot(&self, import: Import) -> u32 {
        self.iat_slots[import.0]
    }

    pub fn data_rva(&self, data: Data) -> u32 {
        self.data_rva + data.0
    }
}

impl PeBuilder {
    /// An executable (`IMAGE_FILE_EXECUTABLE_IMAGE`) at 0x1_4000_0000.
    pub fn exe(name: &str) -> PeBuilder {
        PeBuilder::new(name, false, 0x1_4000_0000)
    }

    /// A DLL at 0x1_8000_0000.
    pub fn dll(name: &str) -> PeBuilder {
        PeBuilder::new(name, true, 0x1_8000_0000)
    }

    fn new(name: &str, dll: bool, image_base: u64) -> PeBuilder {
        PeBuilder {
            name: name.into(),
            dll,
            image_base,
            subsystem: 3, // console
            stack_reserve: 0x10_0000,
            text: Vec::new(),
            pieces: Vec::new(),
            data: Vec::new(),
            imports: Vec::new(),
            exports: Vec::new(),
            entry: None,
            tls: None,
            functions: Vec::new(),
            extra: Vec::new(),
        }
    }

    pub fn image_base(&mut self, base: u64) -> &mut Self {
        self.image_base = base;
        self
    }

    pub fn subsystem(&mut self, subsystem: u16) -> &mut Self {
        self.subsystem = subsystem;
        self
    }

    pub fn stack_reserve(&mut self, bytes: u64) -> &mut Self {
        self.stack_reserve = bytes;
        self
    }

    /// Appends code to `.text`.
    pub fn code(&mut self, bytes: &[u8]) -> Code {
        self.code_with(bytes, &[])
    }

    /// Appends code whose rip-relative displacements (`fixups`: the offset
    /// of a disp32 in `bytes`, and what it points at) are filled in when the
    /// image is laid out. Each must be the last four bytes of its
    /// instruction, as in `call [rip+disp32]` (`FF 15`) or
    /// `mov rax, [rip+disp32]` (`48 8B 05`).
    pub fn code_with(&mut self, bytes: &[u8], fixups: &[(u32, Target)]) -> Code {
        while !self.text.len().is_multiple_of(16) {
            self.text.push(0xCC);
        }
        let at = self.text.len() as u32;
        self.text.extend_from_slice(bytes);
        self.pieces.push(Piece {
            at,
            fixups: fixups.to_vec(),
        });
        Code(at)
    }

    /// `jmp [rip+disp32]` through `import`'s slot: a call of it, as an
    /// import thunk.
    pub fn jump_to(&mut self, import: Import) -> Code {
        self.code_with(&[0xFF, 0x25, 0, 0, 0, 0], &[(2, Target::Import(import))])
    }

    /// Appends data to `.data` (8-byte aligned).
    pub fn data(&mut self, bytes: &[u8]) -> Data {
        while !self.data.len().is_multiple_of(8) {
            self.data.push(0);
        }
        let at = self.data.len() as u32;
        self.data.extend_from_slice(bytes);
        Data(at)
    }

    /// Imports `dll!name`; a name `#n` imports ordinal `n`.
    pub fn import(&mut self, dll: &str, name: &str) -> Import {
        self.imports.push((dll.into(), name.into()));
        Import(self.imports.len() - 1)
    }

    pub fn export(&mut self, name: &str, code: Code) -> &mut Self {
        self.exports.push((name.into(), code));
        self
    }

    pub fn entry(&mut self, code: Code) -> &mut Self {
        self.entry = Some(code);
        self
    }

    /// A TLS directory with `template` as the initial TLS data (at least
    /// one byte is kept) and `callbacks` called in order.
    pub fn tls(&mut self, template: &[u8], callbacks: &[Code]) -> &mut Self {
        self.tls = Some((template.to_vec(), callbacks.to_vec()));
        self
    }

    /// Unwind data for the `len` bytes of code at `code`.
    pub fn function(&mut self, code: Code, len: u32, unwind: Unwind) -> &mut Self {
        self.functions.push((code, len, unwind));
        self
    }

    /// An extra section, laid out after `.data`.
    pub fn section(&mut self, name: &str, bytes: &[u8], characteristics: u32) -> &mut Self {
        assert!(name.len() <= 8, "section names have at most 8 bytes");
        self.extra
            .push((name.into(), bytes.to_vec(), characteristics));
        self
    }

    pub fn build(&self) -> Image {
        let mut layout = Layout::default();
        let mut text = self.text.clone();
        let text_rva = layout.place(".text", text.len(), 0x6000_0020);
        debug_assert_eq!(text_rva, TEXT_RVA);

        // .rdata: imports, exports, unwind info; all its RVAs follow from its own.
        let rdata_rva = layout.next_rva;
        let mut rdata = Vec::new();
        let mut directories = [(0u32, 0u32); 16];
        let mut iat_slots = vec![0; self.imports.len()];
        if !self.imports.is_empty() {
            directories[1] =
                self.import_table(&mut rdata, rdata_rva, &mut iat_slots, &mut directories[12]);
        }
        if !self.exports.is_empty() {
            directories[0] = self.export_table(&mut rdata, rdata_rva);
        }
        let mut unwind_rvas = Vec::new();
        for (_, _, unwind) in &self.functions {
            align(&mut rdata, 4);
            unwind_rvas.push(rdata_rva + rdata.len() as u32);
            let mut codes = unwind.codes.clone();
            rdata.extend_from_slice(&[1, unwind.prolog, codes.len() as u8, 0]);
            if codes.len() % 2 == 1 {
                codes.push(0);
            }
            for c in codes {
                rdata.extend_from_slice(&c.to_le_bytes());
            }
        }
        layout.place(".rdata", rdata.len(), 0x4000_0040);

        // .data: the data given, then TLS.
        let data_rva = layout.next_rva;
        let mut data = self.data.clone();
        let mut relocations: Vec<u32> = Vec::new();
        if let Some((template, callbacks)) = &self.tls {
            align(&mut data, 8);
            let start = data_rva + data.len() as u32;
            data.extend_from_slice(template);
            if template.is_empty() {
                data.push(0);
            }
            let end = data_rva + data.len() as u32;
            align(&mut data, 8);
            let index = data_rva + data.len() as u32;
            data.extend_from_slice(&[0; 8]);
            let array = data_rva + data.len() as u32;
            for callback in callbacks {
                relocations.push(data_rva + data.len() as u32);
                push_u64(&mut data, self.image_base + u64::from(callback.rva()));
            }
            push_u64(&mut data, 0);
            let directory = data_rva + data.len() as u32;
            for va in [start, end, index, array] {
                relocations.push(data_rva + data.len() as u32);
                push_u64(&mut data, self.image_base + u64::from(va));
            }
            data.extend_from_slice(&[0; 8]); // SizeOfZeroFill, Characteristics
            directories[9] = (directory, 40);
        }
        layout.place(".data", data.len(), 0xC000_0040);
        let data_rva = if data.is_empty() { 0 } else { data_rva };

        let mut sections = Vec::new();
        for (name, bytes, characteristics) in &self.extra {
            sections.push((
                name.clone(),
                layout.place(name, bytes.len(), *characteristics),
            ));
        }

        let mut pdata = Vec::new();
        for ((code, len, _), unwind) in self.functions.iter().zip(&unwind_rvas) {
            for v in [code.rva(), code.rva() + len, *unwind] {
                pdata.extend_from_slice(&v.to_le_bytes());
            }
        }
        let pdata_rva = layout.place(".pdata", pdata.len(), 0x4000_0040);
        if !pdata.is_empty() {
            directories[3] = (pdata_rva, pdata.len() as u32);
        }

        let reloc = relocation_blocks(&relocations);
        let reloc_rva = layout.place(".reloc", reloc.len(), 0x4200_0040);
        if !reloc.is_empty() {
            directories[5] = (reloc_rva, reloc.len() as u32);
        }

        // Code fixups, now that everything has its place.
        for piece in &self.pieces {
            for &(offset, target) in &piece.fixups {
                let at = piece.at + offset;
                let to = match target {
                    Target::Import(i) => iat_slots[i.0],
                    Target::Data(d) => data_rva + d.0,
                    Target::Code(c) => c.rva(),
                };
                let disp = to as i64 - (TEXT_RVA + at + 4) as i64;
                let at = at as usize;
                text[at..at + 4].copy_from_slice(&(disp as i32).to_le_bytes());
            }
        }

        let contents: Vec<&[u8]> = [&text[..], &rdata[..], &data[..]]
            .into_iter()
            .chain(self.extra.iter().map(|(_, b, _)| &b[..]))
            .chain([&pdata[..], &reloc[..]])
            .filter(|c| !c.is_empty())
            .collect();
        let bytes = self.headers_and_sections(
            &layout,
            &contents,
            &directories,
            text.len(),
            data.len() + rdata.len(),
        );
        Image {
            bytes,
            iat_slots,
            data_rva,
            sections,
        }
    }

    /// Writes the import table into `rdata`; returns the import directory
    /// and sets `iat` to the address-table directory.
    fn import_table(
        &self,
        rdata: &mut Vec<u8>,
        base: u32,
        slots: &mut [u32],
        iat: &mut (u32, u32),
    ) -> (u32, u32) {
        let mut dlls: Vec<&str> = Vec::new();
        for (dll, _) in &self.imports {
            if !dlls.iter().any(|d| d.eq_ignore_ascii_case(dll)) {
                dlls.push(dll);
            }
        }
        let of = |dll: &str| -> Vec<usize> {
            (0..self.imports.len())
                .filter(|&i| self.imports[i].0.eq_ignore_ascii_case(dll))
                .collect()
        };
        // Sizes first: descriptors, then per DLL its lookup table and its
        // address table, then hint/name entries and DLL names.
        let descriptors = base;
        // The tables hold 8-byte slots, which the loader writes aligned.
        let mut at = align_up(descriptors + 20 * (dlls.len() as u32 + 1), 8);
        let mut tables = Vec::new();
        for dll in &dlls {
            let n = of(dll).len() as u32 + 1;
            tables.push((at, at + 8 * n));
            at += 16 * n;
        }
        let iat_start = tables.first().map_or(0, |t| t.1);
        let mut names = Vec::new();
        let mut name_rvas = vec![0u64; self.imports.len()];
        for (i, (_, name)) in self.imports.iter().enumerate() {
            match name.strip_prefix('#').and_then(|n| n.parse::<u16>().ok()) {
                Some(ordinal) => name_rvas[i] = (1 << 63) | u64::from(ordinal),
                None => {
                    name_rvas[i] = u64::from(at + names.len() as u32);
                    names.extend_from_slice(&0u16.to_le_bytes());
                    names.extend_from_slice(name.as_bytes());
                    names.push(0);
                    align(&mut names, 2);
                }
            }
        }
        let mut dll_names = Vec::new();
        for dll in &dlls {
            dll_names.push(at + names.len() as u32);
            names.extend_from_slice(dll.as_bytes());
            names.push(0);
            align(&mut names, 2);
        }

        for (d, (lookup, address)) in tables.iter().enumerate() {
            for v in [*lookup, 0, 0, dll_names[d], *address] {
                rdata.extend_from_slice(&v.to_le_bytes());
            }
        }
        rdata.extend_from_slice(&[0; 20]);
        align(rdata, 8);
        let mut iat_end = iat_start;
        for (d, dll) in dlls.iter().enumerate() {
            let entries = of(dll);
            for table in 0..2 {
                for (k, &i) in entries.iter().enumerate() {
                    if table == 1 {
                        slots[i] = tables[d].1 + 8 * k as u32;
                    }
                    push_u64(rdata, name_rvas[i]);
                }
                push_u64(rdata, 0);
            }
            iat_end = tables[d].1 + 8 * (entries.len() as u32 + 1);
        }
        rdata.extend_from_slice(&names);
        // The address tables are interleaved with lookup tables; the IAT
        // directory spans from the first to the end of the last.
        *iat = (iat_start, iat_end - iat_start);
        (descriptors, 20 * (dlls.len() as u32 + 1))
    }

    /// Writes the export directory into `rdata`; returns its directory.
    fn export_table(&self, rdata: &mut Vec<u8>, base: u32) -> (u32, u32) {
        align(rdata, 4);
        let start = base + rdata.len() as u32;
        // Names sorted, as `GetProcAddress` searches them by halves.
        let mut exports = self.exports.clone();
        exports.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        let n = exports.len() as u32;
        let functions = start + 40;
        let names = functions + 4 * n;
        let ordinals = names + 4 * n;
        let mut strings = ordinals + 2 * n;
        let dll_name = strings;
        strings += self.name.len() as u32 + 1;
        let mut table = Vec::new();
        for v in [0, 0, 0, dll_name, 1, n, n, functions, names, ordinals] {
            // Characteristics, TimeDateStamp, Major+MinorVersion (one u32),
            // Name, Base, NumberOfFunctions, NumberOfNames, the three tables.
            table.extend_from_slice(&u32::to_le_bytes(v));
        }
        for (_, code) in &exports {
            table.extend_from_slice(&code.rva().to_le_bytes());
        }
        let mut text = Vec::new();
        text.extend_from_slice(self.name.as_bytes());
        text.push(0);
        for (name, _) in &exports {
            table.extend_from_slice(
                &(strings + text.len() as u32 - (self.name.len() as u32 + 1)).to_le_bytes(),
            );
            text.extend_from_slice(name.as_bytes());
            text.push(0);
        }
        for i in 0..n as u16 {
            table.extend_from_slice(&i.to_le_bytes());
        }
        table.extend_from_slice(&text);
        rdata.extend_from_slice(&table);
        (start, table.len() as u32)
    }

    fn headers_and_sections(
        &self,
        layout: &Layout,
        contents: &[&[u8]],
        directories: &[(u32, u32); 16],
        code_size: usize,
        data_size: usize,
    ) -> Vec<u8> {
        let sections = &layout.sections;
        let headers_len = 0x40 + 4 + 20 + 240 + 40 * sections.len();
        let size_of_headers = align_up(headers_len as u32, FILE_ALIGNMENT);
        let mut out = vec![0u8; size_of_headers as usize];
        out[0..2].copy_from_slice(b"MZ");
        out[0x3c..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        let mut h = Vec::new();
        h.extend_from_slice(b"PE\0\0");
        // COFF file header.
        let mut flags: u16 = 0x0002 | 0x0020; // executable image, large address aware
        if self.dll {
            flags |= 0x2000;
        }
        h.extend_from_slice(&0x8664u16.to_le_bytes());
        h.extend_from_slice(&(sections.len() as u16).to_le_bytes());
        h.extend_from_slice(&[0; 12]); // timestamp, symbol table, symbol count
        h.extend_from_slice(&240u16.to_le_bytes());
        h.extend_from_slice(&flags.to_le_bytes());
        // Optional header (PE32+).
        h.extend_from_slice(&0x20Bu16.to_le_bytes());
        h.extend_from_slice(&[14, 0]); // linker version
        h.extend_from_slice(&align_up(code_size as u32, FILE_ALIGNMENT).to_le_bytes());
        h.extend_from_slice(&align_up(data_size as u32, FILE_ALIGNMENT).to_le_bytes());
        h.extend_from_slice(&0u32.to_le_bytes());
        h.extend_from_slice(&self.entry.map_or(0, Code::rva).to_le_bytes());
        h.extend_from_slice(&TEXT_RVA.to_le_bytes());
        h.extend_from_slice(&self.image_base.to_le_bytes());
        h.extend_from_slice(&SECTION_ALIGNMENT.to_le_bytes());
        h.extend_from_slice(&FILE_ALIGNMENT.to_le_bytes());
        for v in [6u16, 0, 0, 0, 6, 0] {
            // OS, image and subsystem versions
            h.extend_from_slice(&v.to_le_bytes());
        }
        h.extend_from_slice(&0u32.to_le_bytes()); // Win32VersionValue
        h.extend_from_slice(&layout.next_rva.to_le_bytes()); // SizeOfImage
        h.extend_from_slice(&size_of_headers.to_le_bytes());
        h.extend_from_slice(&0u32.to_le_bytes()); // CheckSum
        h.extend_from_slice(&self.subsystem.to_le_bytes());
        // High-entropy VA, dynamic base, NX compatible.
        h.extend_from_slice(&0x0160u16.to_le_bytes());
        for v in [self.stack_reserve, 0x1000, 0x10_0000, 0x1000] {
            h.extend_from_slice(&v.to_le_bytes());
        }
        h.extend_from_slice(&0u32.to_le_bytes()); // LoaderFlags
        h.extend_from_slice(&16u32.to_le_bytes());
        for (rva, size) in directories {
            h.extend_from_slice(&rva.to_le_bytes());
            h.extend_from_slice(&size.to_le_bytes());
        }
        let mut file_at = size_of_headers;
        let mut placed = Vec::new();
        for (section, content) in sections.iter().zip(contents) {
            let raw = align_up(content.len() as u32, FILE_ALIGNMENT);
            let mut name = [0u8; 8];
            name[..section.name.len()].copy_from_slice(section.name.as_bytes());
            h.extend_from_slice(&name);
            for v in [content.len() as u32, section.rva, raw, file_at, 0, 0] {
                h.extend_from_slice(&v.to_le_bytes());
            }
            h.extend_from_slice(&[0; 4]); // relocation and line-number counts
            h.extend_from_slice(&section.characteristics.to_le_bytes());
            placed.push((file_at, content));
            file_at += raw;
        }
        out[0x40..0x40 + h.len()].copy_from_slice(&h);
        out.resize(file_at as usize, 0);
        for (at, content) in placed {
            out[at as usize..at as usize + content.len()].copy_from_slice(content);
        }
        out
    }
}

#[derive(Default)]
struct Layout {
    next_rva: u32,
    sections: Vec<PlacedSection>,
}

struct PlacedSection {
    name: String,
    rva: u32,
    characteristics: u32,
}

impl Layout {
    /// The RVA a section of `len` bytes gets; empty ones take no room and
    /// are not listed.
    fn place(&mut self, name: &str, len: usize, characteristics: u32) -> u32 {
        if self.next_rva == 0 {
            self.next_rva = TEXT_RVA;
        }
        let rva = self.next_rva;
        if len > 0 {
            self.sections.push(PlacedSection {
                name: name.into(),
                rva,
                characteristics,
            });
            self.next_rva += align_up(len as u32, SECTION_ALIGNMENT);
        }
        rva
    }
}

/// `.reloc` blocks: one per 4 KiB page, `IMAGE_REL_BASED_DIR64` entries.
fn relocation_blocks(rvas: &[u32]) -> Vec<u8> {
    let mut rvas = rvas.to_vec();
    rvas.sort_unstable();
    let mut out = Vec::new();
    let mut i = 0;
    while i < rvas.len() {
        let page = rvas[i] & !0xfff;
        let entries: Vec<u16> = rvas[i..]
            .iter()
            .take_while(|&&r| r & !0xfff == page)
            .map(|&r| (10 << 12) | (r & 0xfff) as u16)
            .collect();
        i += entries.len();
        let mut entries = entries;
        if entries.len() % 2 == 1 {
            entries.push(0); // IMAGE_REL_BASED_ABSOLUTE padding
        }
        out.extend_from_slice(&page.to_le_bytes());
        out.extend_from_slice(&(8 + 2 * entries.len() as u32).to_le_bytes());
        for e in entries {
            out.extend_from_slice(&e.to_le_bytes());
        }
    }
    out
}

fn align(bytes: &mut Vec<u8>, to: usize) {
    while !bytes.len().is_multiple_of(to) {
        bytes.push(0);
    }
}

fn align_up(v: u32, to: u32) -> u32 {
    v.div_ceil(to) * to
}

fn push_u64(bytes: &mut Vec<u8>, v: u64) {
    bytes.extend_from_slice(&v.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use victory_pe::{ImageInfo, ImportSymbol};

    #[test]
    fn a_built_image_parses_back_with_every_part() {
        let mut pe = PeBuilder::exe("built.exe");
        let open = pe.import("kernel32.dll", "CreateFileW");
        let close = pe.import("KERNEL32.dll", "CloseHandle");
        let init = pe.import("xgameruntime.dll", "XGameRuntimeInitialize");
        let by_ordinal = pe.import("xaudio2_9.dll", "#1");
        let ret = pe.code(&[0xC3]);
        let thunk = pe.jump_to(init);
        // sub rsp, 0x28; call [rip+x]; add rsp, 0x28; ret
        let main = pe.code_with(
            &[
                0x48, 0x83, 0xEC, 0x28, 0xFF, 0x15, 0, 0, 0, 0, 0x48, 0x83, 0xC4, 0x28, 0xC3,
            ],
            &[(6, Target::Import(open))],
        );
        pe.entry(main)
            .export("zeta", ret)
            .export("alpha", thunk)
            .tls(&[1, 2, 3], &[ret, thunk])
            .function(main, 15, Unwind::alloc_small(4, 0x28))
            .function(ret, 1, Unwind::leaf())
            .section(".extra", b"hello", 0x4000_0040);
        pe.data(b"some data");
        let image = pe.build();
        let info = ImageInfo::parse(&image.bytes).unwrap();

        assert!(!info.is_dll());
        assert_eq!(info.entry_rva, main.rva());
        assert_eq!(info.image_base, 0x1_4000_0000);
        assert!(info.has_relocations && info.has_exception_table);
        let names: Vec<_> = info
            .sections
            .iter()
            .map(|s| s.name.trim_end_matches('\0'))
            .collect();
        assert_eq!(
            names,
            [".text", ".rdata", ".data", ".extra", ".pdata", ".reloc"]
        );
        // The two kernel32 imports share a descriptor whatever their case.
        let dlls: Vec<_> = info.imports.iter().map(|m| m.dll.as_str()).collect();
        assert_eq!(dlls, ["kernel32.dll", "xgameruntime.dll", "xaudio2_9.dll"]);
        let kernel32 = &info.imports[0].entries;
        assert_eq!(kernel32.len(), 2);
        assert_eq!(kernel32[0].iat_slot_rva, image.iat_slot(open));
        assert_eq!(kernel32[1].iat_slot_rva, image.iat_slot(close));
        let name = |e: &victory_pe::ImportEntry| e.symbol.to_string();
        assert_eq!(name(&kernel32[1]), "CloseHandle");
        let ordinal = &info.imports[2].entries[0];
        assert_eq!(ordinal.symbol, ImportSymbol::Ordinal(1));
        assert_eq!(ordinal.iat_slot_rva, image.iat_slot(by_ordinal));
        assert_eq!(info.import_count(), 4);
        assert!(
            image.iat_slots.iter().all(|s| s % 8 == 0),
            "{:x?}",
            image.iat_slots
        );
        // Exports sorted by name, at the code's RVAs.
        let exports: Vec<_> = info
            .exports
            .iter()
            .map(|e| (e.name.clone().unwrap(), e.rva.unwrap()))
            .collect();
        assert_eq!(
            exports,
            [("alpha".into(), thunk.rva()), ("zeta".into(), ret.rva())]
        );
        let tls = info.tls.as_ref().unwrap();
        assert_eq!(tls.callback_rvas, [ret.rva(), thunk.rva()]);
        // The thunk's displacement reaches the import's slot.
        let at = info.rva_to_file_offset(thunk.rva()).unwrap() as usize;
        let disp = i32::from_le_bytes(image.bytes[at + 2..at + 6].try_into().unwrap());
        assert_eq!(
            i64::from(thunk.rva()) + 6 + i64::from(disp),
            i64::from(image.iat_slot(init))
        );
        // After .text, .rdata and .data, a page each.
        assert_eq!(image.sections, [(".extra".to_string(), 0x4000)]);
    }

    #[test]
    fn a_bare_dll_has_only_its_code() {
        let mut pe = PeBuilder::dll("bare.dll");
        pe.code(&[0xC3]);
        let info = ImageInfo::parse(&pe.build().bytes).unwrap();
        assert!(info.is_dll());
        assert_eq!(info.sections.len(), 1);
        assert!(info.imports.is_empty() && info.exports.is_empty() && info.tls.is_none());
        assert!(!info.has_relocations);
        assert_eq!(info.size_of_image, 0x2000);
    }

    #[test]
    fn relocations_are_grouped_by_page() {
        let blocks = relocation_blocks(&[0x3010, 0x2008, 0x2000]);
        // Page 0x2000: two entries; page 0x3000: one, padded to two.
        assert_eq!(blocks.len(), 12 + 12);
        assert_eq!(&blocks[..8], &[0, 0x20, 0, 0, 12, 0, 0, 0]);
        assert_eq!(u16::from_le_bytes([blocks[8], blocks[9]]), 0xA000);
        assert_eq!(u16::from_le_bytes([blocks[22], blocks[23]]), 0);
    }

    #[test]
    fn unwind_codes_encode_a_small_allocation() {
        let u = Unwind::alloc_small(4, 0x28);
        assert_eq!(u.codes, [0x4204]);
    }
}
