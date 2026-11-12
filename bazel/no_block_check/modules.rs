//! The native check's program: the LLVM IR of every crate in the library's
//! graph, each written by its own compilation, and std's code beside them.
//!
//! A crate's module holds its own functions and the generic instances it
//! uses; what it calls in another crate is a declaration there and a
//! definition in that crate's module. [`link`] reads the modules as one
//! program, the way a linker resolves names: the first definition of a name
//! is the one kept, so a generic instance that several crates instantiated
//! (the same code, from the same MIR) is one function.
//!
//! std is not compiled with the program, so its code comes from the bitcode
//! its rlibs carry. [`std_keep`] writes a crate that names every Rust symbol
//! the modules use and none of them define; rustc's fat LTO of that crate as
//! a staticlib merges what those symbols reach out of std's bitcode, and its
//! IR is the last module. A symbol std does not define either stays declared,
//! and the scan refuses the program for it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use super::ir::symbols;

/// The modules as one program: each name's first definition, in the order
/// the modules are given (the crates first, std's closure last, so a
/// crate's unoptimised copy of an instance is kept over std's), and a
/// declaration only of a name no module defines. A declaration says the
/// contents come from outside the program, which for a static the
/// provenance analysis reads as contents it cannot know.
pub(crate) fn link(modules: &[String]) -> Result<String, String> {
    let mut out = String::new();
    let mut defined: BTreeSet<String> = BTreeSet::new();
    let mut declared: BTreeMap<String, String> = BTreeMap::new();
    let mut triple: Option<String> = None;
    for (k, path) in modules.iter().enumerate() {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        let local = locals(&text);
        let mut skipping = false;
        for line in text.lines() {
            let renamed;
            let line = if local.is_empty() || !line.contains('@') {
                line
            } else {
                renamed = rename(line, &local, k);
                renamed.as_str()
            };
            if skipping {
                if line.starts_with('}') {
                    skipping = false;
                }
                continue;
            }
            if let Some(t) = line.strip_prefix("target triple = ") {
                match &triple {
                    None => triple = Some(t.to_string()),
                    Some(first) if unversioned(first) == unversioned(t) => continue,
                    Some(first) => {
                        return Err(format!(
                            "{path} targets {t}, and an earlier module {first}: \
                             these are not one program"
                        ))
                    }
                }
            } else if line.starts_with("define ") {
                if let Some((name, _)) = symbols(line).into_iter().next() {
                    if !defined.insert(name) {
                        skipping = line.trim_end().ends_with('{');
                        continue;
                    }
                }
            } else if line.starts_with("declare ") || (line.starts_with('@') && is_external(line)) {
                if let Some((name, _)) = symbols(line).into_iter().next() {
                    declared.entry(name).or_insert_with(|| line.to_string());
                }
                continue;
            } else if line.starts_with('@') {
                if let Some((name, _)) = symbols(line).into_iter().next() {
                    if !defined.insert(name) {
                        continue;
                    }
                }
            }
            out.push_str(line);
            out.push('\n');
        }
        if skipping {
            return Err(format!("{path} ends inside a function body"));
        }
    }
    for (name, line) in &declared {
        if !defined.contains(name) {
            out.push_str(line);
            out.push('\n');
        }
    }
    Ok(out)
}

/// The names a module defines with local linkage that are not Rust symbols:
/// rustc's `vtable.0`, `alloc_<hash>`, `anon.<hash>.0` and the like, which
/// another module may define for something else under the same name. A
/// local Rust symbol carries its crate in its name, so two modules that
/// define one define the same function.
fn locals(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in text.lines() {
        let local = if let Some(rest) = line.strip_prefix("define ") {
            let head = &rest[..rest.find('@').unwrap_or(rest.len())];
            head.split_whitespace()
                .any(|w| w == "internal" || w == "private")
        } else if line.starts_with('@') {
            line.split_once(" = ")
                .is_some_and(|(_, r)| r.starts_with("internal ") || r.starts_with("private "))
        } else {
            false
        };
        if !local {
            continue;
        }
        if let Some((name, _)) = symbols(line).into_iter().next() {
            if !is_rust(&name) {
                out.insert(name);
            }
        }
    }
    out
}

/// `line` with each `@name` in `local` made the module's own, as
/// `@name.m<module>`. Quoted text that is not a name, and a comment, are
/// left alone, as [`symbols`] reads them.
fn rename(line: &str, local: &BTreeSet<String>, module: usize) -> String {
    let b = line.as_bytes();
    let mut out = String::with_capacity(line.len() + 8);
    let mut i = 0;
    let mut copied = 0;
    while i < b.len() {
        match b[i] {
            b';' => break,
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += 1;
                }
                i += 1;
            }
            b'@' => {
                i += 1;
                let (start, end, quoted) = if b.get(i) == Some(&b'"') {
                    let start = i + 1;
                    let mut j = start;
                    while j < b.len() && b[j] != b'"' {
                        j += 1;
                    }
                    i = (j + 1).min(b.len());
                    (start, j.min(b.len()), true)
                } else {
                    let start = i;
                    while i < b.len()
                        && (b[i].is_ascii_alphanumeric()
                            || matches!(b[i], b'-' | b'$' | b'.' | b'_'))
                    {
                        i += 1;
                    }
                    (start, i, false)
                };
                // Escaped names are compared as written; rustc's local
                // names need no escapes.
                if local.contains(&line[start..end]) {
                    out.push_str(&line[copied..end]);
                    let _ = write!(out, ".m{module}");
                    copied = end;
                    let _ = quoted;
                }
            }
            _ => i += 1,
        }
    }
    out.push_str(&line[copied..]);
    out
}

/// A triple without its OS's version: crates of one program can be compiled
/// for different minimum OS versions (`arm64-apple-macosx11.0.0` and
/// `arm64-apple-macosx27.0.0`).
fn unversioned(triple: &str) -> String {
    triple
        .split('-')
        .map(|c| c.trim_end_matches(|ch: char| ch.is_ascii_digit() || ch == '.' || ch == '"'))
        .collect::<Vec<_>>()
        .join("-")
}

/// Whether a global's line declares it rather than defining it.
fn is_external(line: &str) -> bool {
    line.split_once(" = ")
        .is_some_and(|(_, rest)| rest.starts_with("external ") || rest.starts_with("extern_weak "))
}

/// A Rust symbol, in either mangling.
fn is_rust(name: &str) -> bool {
    name.starts_with("_R") || name.starts_with("_ZN")
}

/// The allocator functions a `#[global_allocator]` defines, by their unmangled
/// names: rustc's allocator shim calls these when the program has one.
const GLOBAL_ALLOCATOR: &[&str] = &[
    "__rg_alloc",
    "__rg_dealloc",
    "__rg_realloc",
    "__rg_alloc_zeroed",
];

/// The Rust symbols one module defines (`d`), and declares as functions
/// (`f`) or statics (`s`): what [`std_keep`] reads, written once per crate
/// so that a change to one crate rereads only its module.
pub(crate) fn summary(module: &str) -> Result<String, String> {
    let text = std::fs::read_to_string(module).map_err(|e| format!("{module}: {e}"))?;
    let mut out = BTreeSet::new();
    for line in text.lines() {
        let kind = if line.starts_with("define ") {
            "d"
        } else if line.starts_with("declare ") {
            "f"
        } else if line.starts_with('@') {
            if is_external(line) {
                "s"
            } else {
                "d"
            }
        } else {
            continue;
        };
        if let Some((name, _)) = symbols(line).into_iter().next() {
            if is_rust(&name) {
                out.insert(format!("{kind} {name}"));
            }
        }
    }
    Ok(out.into_iter().map(|l| l + "\n").collect())
}

/// The source of the crate whose fat-LTO IR is std's part of the program:
/// every Rust symbol the modules use and do not define, kept by one export.
///
/// A program with a `#[global_allocator]` gets one here too, whose methods do
/// nothing: rustc then writes the allocator shim to call the global
/// allocator's functions, and [`link`] keeps the program's definitions of
/// those, which come first.
pub(crate) fn std_keep(summaries: &[String]) -> Result<String, String> {
    let mut defined: BTreeSet<String> = BTreeSet::new();
    let mut functions: BTreeSet<String> = BTreeSet::new();
    let mut statics: BTreeSet<String> = BTreeSet::new();
    for path in summaries {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        for line in text.lines() {
            let (set, name) = match line.split_once(' ') {
                Some(("d", n)) => (&mut defined, n),
                Some(("f", n)) => (&mut functions, n),
                Some(("s", n)) => (&mut statics, n),
                _ => return Err(format!("{path}: not a module summary: `{line}`")),
            };
            set.insert(name.to_string());
        }
    }
    let global_allocator = defined.iter().any(|n| {
        is_rust(n)
            && GLOBAL_ALLOCATOR
                .iter()
                .any(|g| super::own_path(n).last().is_some_and(|l| l == g))
    });
    let mut out = String::from(
        "// @generated by no_block_check --std-keep: std's code that the program uses.\n\
         #![no_std]\n\
         #![allow(clashing_extern_declarations, improper_ctypes)]\n\
         extern crate std;\n\
         unsafe extern \"Rust\" {\n",
    );
    let wanted = |set: &BTreeSet<String>| -> Vec<String> {
        set.iter()
            .filter(|n| is_rust(n) && !defined.contains(*n))
            .cloned()
            .collect()
    };
    let (functions, statics) = (wanted(&functions), wanted(&statics));
    for (i, f) in functions.iter().enumerate() {
        let _ = writeln!(out, "    #[link_name = \"{f}\"] safe fn f{i}();");
    }
    for (i, s) in statics.iter().enumerate() {
        let _ = writeln!(out, "    #[link_name = \"{s}\"] safe static s{i}: u8;");
    }
    out.push_str(
        "}\n\
         #[repr(transparent)]\n\
         pub struct Kept(*const ());\n\
         unsafe impl Sync for Kept {}\n",
    );
    let _ = write!(
        out,
        "#[unsafe(no_mangle)]\n#[used]\npub static FRUSTRATE_NO_BLOCK_STD: [Kept; {}] = [\n",
        functions.len() + statics.len()
    );
    for i in 0..functions.len() {
        let _ = writeln!(out, "    Kept(f{i} as *const ()),");
    }
    for i in 0..statics.len() {
        let _ = writeln!(out, "    Kept(&raw const s{i} as *const ()),");
    }
    out.push_str("];\n");
    if global_allocator {
        out.push_str(
            "struct Program;\n\
             unsafe impl core::alloc::GlobalAlloc for Program {\n\
             \x20   unsafe fn alloc(&self, _: core::alloc::Layout) -> *mut u8 { core::ptr::null_mut() }\n\
             \x20   unsafe fn dealloc(&self, _: *mut u8, _: core::alloc::Layout) {}\n\
             }\n\
             #[global_allocator]\n\
             static PROGRAM: Program = Program;\n",
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(texts: &[&str]) -> Vec<String> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!("modules_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        texts
            .iter()
            .map(|t| {
                let p = dir.join(format!("m{}.ll", NEXT.fetch_add(1, Ordering::Relaxed)));
                std::fs::write(&p, t).unwrap();
                p.to_str().unwrap().to_string()
            })
            .collect()
    }

    fn summarised(texts: &[&str]) -> Vec<String> {
        let sums: Vec<String> = files(texts).iter().map(|m| summary(m).unwrap()).collect();
        let refs: Vec<&str> = sums.iter().map(String::as_str).collect();
        files(&refs)
    }

    const TRIPLE: &str = "target triple = \"arm64-apple-macosx11.0.0\"\n";

    #[test]
    fn the_first_definition_of_a_name_is_kept() {
        let a = format!("{TRIPLE}define void @_RNvCs1_1a1f() {{\n  call void @_RNvCs1_1b1g()\n  ret void\n}}\ndeclare void @_RNvCs1_1b1g()\n@_RNvCs1_1a1S = global i8 0\n");
        let b = format!("{TRIPLE}define void @_RNvCs1_1b1g() {{\n  ret void\n}}\ndefine void @_RNvCs1_1a1f() {{\n  call void @nanosleep()\n  ret void\n}}\n@_RNvCs1_1a1S = global i8 1\n@_RNvCs1_1b1T = external global i8\n");
        let linked = link(&files(&[&a, &b])).unwrap();
        assert!(!linked.contains("nanosleep"), "{linked}");
        assert!(!linked.contains("global i8 1"), "{linked}");
        assert!(linked.contains("define void @_RNvCs1_1b1g()"), "{linked}");
        assert!(
            linked.contains("@_RNvCs1_1b1T = external global i8"),
            "{linked}"
        );
        assert!(!linked.contains("declare void @_RNvCs1_1b1g()"), "{linked}");
        assert_eq!(linked.matches("target triple").count(), 1, "{linked}");
    }

    #[test]
    fn a_local_name_in_two_modules_is_two_globals() {
        let a = format!(
            "{TRIPLE}@vtable.0 = private constant [1 x ptr] [ptr @_RNvCs1_1a1f]\n\
             define void @_RNvCs1_1a1f() {{\n  ret void\n}}\n\
             define void @_RNvCs1_1a1g() {{\n  call void @use(ptr @vtable.0) ; @vtable.0\n  ret void\n}}\n"
        );
        let b = format!(
            "{TRIPLE}@vtable.0 = private constant [1 x ptr] [ptr @_RNvCs1_1b1h]\n\
             define void @_RNvCs1_1b1h() {{\n  call void @nanosleep()\n  ret void\n}}\n"
        );
        let linked = link(&files(&[&a, &b])).unwrap();
        assert!(
            linked.contains("@vtable.0.m0 = private constant [1 x ptr] [ptr @_RNvCs1_1a1f]"),
            "{linked}"
        );
        assert!(
            linked.contains("@vtable.0.m1 = private constant [1 x ptr] [ptr @_RNvCs1_1b1h]"),
            "{linked}"
        );
        assert!(
            linked.contains("call void @use(ptr @vtable.0.m0) ; @vtable.0\n"),
            "{linked}"
        );
    }

    #[test]
    fn modules_for_two_targets_are_refused() {
        let a = TRIPLE.to_string();
        let b = "target triple = \"x86_64-unknown-linux-gnu\"\n".to_string();
        let c = "target triple = \"arm64-apple-macosx27.0.0\"\n".to_string();
        assert!(link(&files(&[&a, &c])).is_ok());
        assert!(link(&files(&[&a, &b]))
            .unwrap_err()
            .contains("not one program"));
    }

    #[test]
    fn std_keep_names_the_rust_symbols_nothing_defines() {
        let a = format!(
            "{TRIPLE}define void @_RNvCs1_1a1f() {{\n  ret void\n}}\n\
             declare void @_RNvCs1_1a1f()\n\
             declare void @_RNvNtCs2_3std2io6_print()\n\
             declare i32 @pthread_mutex_lock(ptr)\n\
             @_RNvCs2_3std5COUNT = external global i64\n"
        );
        let keep = std_keep(&summarised(&[&a])).unwrap();
        assert!(
            keep.contains("#[link_name = \"_RNvNtCs2_3std2io6_print\"] safe fn f0();"),
            "{keep}"
        );
        assert!(
            keep.contains("#[link_name = \"_RNvCs2_3std5COUNT\"] safe static s0: u8;"),
            "{keep}"
        );
        assert!(!keep.contains("pthread_mutex_lock"), "{keep}");
        assert!(!keep.contains("\"_RNvCs1_1a1f\""), "{keep}");
        assert!(keep.contains("[Kept; 2]"), "{keep}");
        assert!(!keep.contains("global_allocator"), "{keep}");
    }

    #[test]
    fn a_program_with_a_global_allocator_gets_its_shim() {
        let a = format!("{TRIPLE}define ptr @_RNvCs5fGHjzWXRav_7___rustc10___rg_alloc(i64 %0, i64 %1) {{\n  ret ptr null\n}}\n");
        assert!(std_keep(&summarised(&[&a]))
            .unwrap()
            .contains("#[global_allocator]"));
    }
}
