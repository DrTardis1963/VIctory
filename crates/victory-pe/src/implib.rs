//! COFF import libraries (`.lib`).
//!
//! An umbrella import library tells us which DLL provides each API that a
//! console title may link against. That is the map the runtime needs to
//! decide which of our implementations answers which import.

use object::read::archive::ArchiveFile;
use object::read::coff::{ImportFile, ImportName, ImportType};

use crate::{ImportSymbol, PeError};

/// One `dll!export` pair described by an import library.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LibImport {
    pub dll: String,
    pub export: ImportSymbol,
    pub kind: LibImportKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LibImportKind {
    Code,
    Data,
    Const,
}

/// Result of reading an import library.
#[derive(Debug, Default)]
pub struct ImportLibrary {
    pub imports: Vec<LibImport>,
    /// Members that are not short-format import objects (regular objects,
    /// long-format import descriptors). Reported so nothing is silently lost.
    pub other_members: usize,
}

impl ImportLibrary {
    pub fn parse(data: &[u8]) -> Result<Self, PeError> {
        let archive = ArchiveFile::parse(data)?;
        let mut lib = ImportLibrary::default();
        for member in archive.members() {
            let member = member?;
            let bytes = member.data(data)?;
            match ImportFile::parse(bytes) {
                Ok(import) => lib.imports.push(LibImport {
                    dll: String::from_utf8_lossy(import.dll()).into_owned(),
                    export: match import.import() {
                        ImportName::Name(name) => ImportSymbol::Name {
                            hint: 0,
                            name: String::from_utf8_lossy(name).into_owned(),
                        },
                        ImportName::Ordinal(o) => ImportSymbol::Ordinal(o),
                    },
                    kind: match import.import_type() {
                        ImportType::Code => LibImportKind::Code,
                        ImportType::Data => LibImportKind::Data,
                        ImportType::Const => LibImportKind::Const,
                    },
                }),
                Err(_) => lib.other_members += 1,
            }
        }
        lib.imports.sort();
        lib.imports.dedup();
        Ok(lib)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = b"!<arch>\n".to_vec();
        for (name, body) in members {
            let header = format!(
                "{name:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                0,
                0,
                0,
                100644,
                body.len()
            );
            assert_eq!(header.len(), 60);
            out.extend_from_slice(header.as_bytes());
            out.extend_from_slice(body);
            if body.len() % 2 == 1 {
                out.push(b'\n');
            }
        }
        out
    }

    fn short_import(
        symbol: &str,
        dll: &str,
        import_type: u16,
        name_type: u16,
        ordinal_or_hint: u16,
    ) -> Vec<u8> {
        let size_of_data = (symbol.len() + 1 + dll.len() + 1) as u32;
        let flags = (import_type & 0x3) | ((name_type & 0x7) << 2);
        let mut out = Vec::with_capacity(20 + size_of_data as usize);
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0xFFFFu16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&crate::MACHINE_AMD64.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&size_of_data.to_le_bytes());
        out.extend_from_slice(&ordinal_or_hint.to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(symbol.as_bytes());
        out.push(0);
        out.extend_from_slice(dll.as_bytes());
        out.push(0);
        out
    }

    #[test]
    fn parses_short_imports_sorts_dedups_and_counts_other_members() {
        let code = short_import("CreateThing", "sample.dll", 0, 1, 7);
        let data = short_import("_g_flag", "sample.dll", 1, 2, 0);
        let ordinal = short_import("ignored", "other.dll", 2, 0, 42);
        let obj = b"regular object stub";
        let lib = archive(&[
            ("m1/", &code),
            ("m2/", &code),
            ("m3/", &data),
            ("m4/", &ordinal),
            ("m5/", obj),
        ]);

        let parsed = ImportLibrary::parse(&lib).unwrap();
        assert_eq!(parsed.other_members, 1);
        assert_eq!(
            parsed.imports,
            vec![
                LibImport {
                    dll: "other.dll".into(),
                    export: ImportSymbol::Ordinal(42),
                    kind: LibImportKind::Const,
                },
                LibImport {
                    dll: "sample.dll".into(),
                    export: ImportSymbol::Name {
                        hint: 0,
                        name: "CreateThing".into(),
                    },
                    kind: LibImportKind::Code,
                },
                LibImport {
                    dll: "sample.dll".into(),
                    export: ImportSymbol::Name {
                        hint: 0,
                        name: "g_flag".into(),
                    },
                    kind: LibImportKind::Data,
                },
            ]
        );
    }

    #[test]
    fn rejects_non_archives_and_truncated_members() {
        assert!(matches!(ImportLibrary::parse(b""), Err(PeError::Malformed(_))));
        assert!(matches!(
            ImportLibrary::parse(b"not an archive"),
            Err(PeError::Malformed(_))
        ));
        assert!(matches!(
            ImportLibrary::parse(b"!<arch>\nshort"),
            Err(PeError::Malformed(_))
        ));

        let mut truncated = archive(&[("m1/", b"abcd")]);
        // Claim size 999 in the 60-byte header starting at byte 8 (size at 56..66).
        truncated[56..66].copy_from_slice(b"999       ");
        assert!(matches!(
            ImportLibrary::parse(&truncated),
            Err(PeError::Malformed(_))
        ));
    }
}
