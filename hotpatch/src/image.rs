//! The running library, as read from the file it was loaded from.

use object::{Object, ObjectSection, ObjectSymbol, SymbolKind, SymbolScope};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    MachO,
    Elf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    Aarch64,
    X86_64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymKind {
    Text,
    Data,
    /// A Mach-O thread-local variable descriptor.
    Tls,
}

#[derive(Debug, Clone, Copy)]
pub struct Symbol {
    /// The address in the file, before the loader's slide.
    pub address: u64,
    pub kind: SymKind,
    /// More than one definition carries this name. Any copy of a function is
    /// as good as another; a data symbol is ambiguous.
    pub duplicated: bool,
}

pub struct Image {
    pub format: Format,
    pub arch: Arch,
    /// `LC_UUID` on Mach-O, the GNU build-id on ELF.
    pub identity: Option<Vec<u8>>,
    /// Defined symbols by name, without Mach-O's leading underscore, so they
    /// match LLVM IR names.
    pub symbols: HashMap<String, Symbol>,
    /// Defined symbols the dynamic linker exports.
    pub exported: HashSet<String>,
    /// Symbols the library imports from other images.
    pub imported: HashSet<String>,
}

impl Image {
    pub fn read(bytes: &[u8]) -> Result<Image, String> {
        let file = object::File::parse(bytes).map_err(|e| format!("not a library object file: {e}"))?;
        let format = match file.format() {
            object::BinaryFormat::MachO => Format::MachO,
            object::BinaryFormat::Elf => Format::Elf,
            other => return Err(format!("{other:?} libraries cannot be patched")),
        };
        let arch = match file.architecture() {
            object::Architecture::Aarch64 => Arch::Aarch64,
            object::Architecture::X86_64 => Arch::X86_64,
            other => return Err(format!("{other:?} libraries cannot be patched")),
        };
        let identity = match format {
            Format::MachO => file.mach_uuid().ok().flatten().map(|u| u.to_vec()),
            Format::Elf => file.build_id().ok().flatten().map(|b| b.to_vec()),
        };
        let tls_sections: HashSet<object::SectionIndex> = file
            .sections()
            .filter(|s| s.name().is_ok_and(|n| n == "__thread_vars"))
            .map(|s| s.index())
            .collect();

        let mut symbols: HashMap<String, Symbol> = HashMap::new();
        let mut exported = HashSet::new();
        let mut imported = HashSet::new();
        for sym in file.symbols().chain(file.dynamic_symbols()) {
            let Ok(raw) = sym.name() else { continue };
            if raw.is_empty() {
                continue;
            }
            let name = match format {
                Format::MachO => raw.strip_prefix('_').unwrap_or(raw),
                Format::Elf => raw,
            };
            if sym.is_undefined() {
                imported.insert(name.to_string());
                continue;
            }
            let kind = if sym.section_index().is_some_and(|i| tls_sections.contains(&i)) {
                SymKind::Tls
            } else {
                match sym.kind() {
                    SymbolKind::Text => SymKind::Text,
                    SymbolKind::Data | SymbolKind::Unknown => SymKind::Data,
                    SymbolKind::Tls => SymKind::Tls,
                    _ => continue,
                }
            };
            if sym.scope() == SymbolScope::Dynamic {
                exported.insert(name.to_string());
            }
            let address = sym.address();
            symbols
                .entry(name.to_string())
                .and_modify(|s| {
                    // The dynamic symbol table repeats the static one on ELF.
                    if s.address != address {
                        s.duplicated = true;
                    }
                })
                .or_insert(Symbol {
                    address,
                    kind,
                    duplicated: false,
                });
        }
        // A name the library both defines and lists as undefined is defined.
        imported.retain(|n| !symbols.contains_key(n));
        Ok(Image {
            format,
            arch,
            identity,
            symbols,
            exported,
            imported,
        })
    }
}

#[cfg(test)]
impl Image {
    /// An image described directly, for tests that need no linker. `text` and
    /// `data` are defined symbol names; `exported` is the subset the dynamic
    /// linker offers.
    pub fn for_test(text: &[&str], data: &[&str], exported: &[&str]) -> Image {
        let mut symbols = HashMap::new();
        for (i, name) in text.iter().enumerate() {
            symbols.insert(
                name.to_string(),
                Symbol { address: 0x1000 + i as u64 * 0x100, kind: SymKind::Text, duplicated: false },
            );
        }
        for (i, name) in data.iter().enumerate() {
            symbols.insert(
                name.to_string(),
                Symbol { address: 0x9000 + i as u64 * 0x8, kind: SymKind::Data, duplicated: false },
            );
        }
        Image {
            format: Format::MachO,
            arch: Arch::Aarch64,
            identity: Some(vec![7; 16]),
            symbols,
            exported: exported.iter().map(|s| s.to_string()).collect(),
            imported: HashSet::new(),
        }
    }
}
