//! Fail the build when a wasm module reaches a std facility its platform
//! cannot serve.
//!
//! # Why this exists
//!
//! `wasm32-unknown-unknown` has no OS behind it, so std links
//! `sys/pal/unsupported/*` for anything that needs one. Reaching those paths
//! compiles clean and fails later, in a browser, in two different ways:
//!
//! - `SystemTime::now()` panics (`time not implemented on this platform`).
//!   frustrate's panic-attribution shim does carry the message across, so this
//!   one is at least *reportable* — but it is a runtime failure for a fact that
//!   was knowable when the module was built.
//! - `println!` is worse: `sys::stdio::unsupported::Stdout::write` returns
//!   `Ok(len)` without writing, so the call succeeds, produces nothing, and
//!   nothing anywhere reports it. Measured: no output, no trap, no error.
//!
//! Both have a fix that already works — build for `//bazel:wasm32_wasi`, where
//! the facilities are genuinely served. This
//! tool is what points at it, at build time, without the author having to know
//! the failure mode first.
//!
//! # How it decides
//!
//! Per facility: **the module's name section contains the std symbol, and its
//! import section lacks the wasi call that would serve it.**
//!
//! Keying on the module's own imports rather than on a platform label is what
//! keeps this rule free of a platform table. `frustrate_wasm_module` documents
//! that any `platform()` works, including one a consumer declares; a check that
//! compared against `//bazel:wasm32_wasi` by name would quietly not apply to
//! those. A module that imports `clock_time_get` has a clock behind
//! `SystemTime::now` whatever platform produced it, and one that does not, does
//! not.
//!
//! # What it does not claim
//!
//! This is a tripwire, not a proof. A facility reached only through a path
//! rustc inlines away leaves no symbol, and a facility whose symbol survives
//! but is never called is a false positive — which is why the covered set is
//! exactly the two measured to discriminate (see `FACILITIES`) rather than
//! everything std marks unsupported. Both decay modes are made loud rather than
//! silent: a missing name section is a hard error (see `SENTINEL`), and
//! inlining turns `//tests/bazel_rules:wasm_std_check_test` red.
//!
//! The wasm parsing here duplicates `tests/bazel_rules/wasm_imports_test.dart`
//! by about sixty lines. That is deliberate: the Dart parser cannot be shared
//! into a Rust exec tool, and the two sections this reads have been frozen
//! since the MVP, so there is no format drift for a shared implementation to
//! protect against.

use std::collections::BTreeSet;
use std::process::ExitCode;

/// A std facility, the mangled-symbol segments that identify a *call* to it,
/// and the wasi import that would mean the platform serves it.
///
/// `segments` are matched conjunctively **within a single symbol name**, and
/// each carries its Rust-mangling length prefix (`4time`, `10SystemTime`). The
/// prefixes are what make this precise: `SystemTime` alone appears in any
/// module that merely names the type in a `Debug` impl, whereas the sequence
/// `4time` + `10SystemTime` + `3now` appears only where the call is.
///
/// Legacy (`_ZN…`) and v0 (`_R…`) manglings both carry these identifier
/// segments, matched by [`contains_segment`], which knows the one way they
/// differ here.
struct Facility {
    /// What the author wrote, for the message.
    name: &'static str,
    /// Conjunctive segments, all required in one symbol.
    segments: &'static [&'static str],
    /// The host imports that mean this facility is genuinely served. More than
    /// one because there are two ways to get a host: the wasip1 platform, where
    /// std asks through `wasi_snapshot_preview1`, and a std built by
    /// toolchain/custom_std, where it asks frustrate directly. Either counts.
    served_by: &'static [&'static str],
    /// What happens today when this ships, for the message.
    consequence: &'static str,
    /// `toolchain/custom_std/tool/build.dart --facilities=` spells facilities
    /// with its own short names, and the message has to hand back a command
    /// that runs. Several entries share one (both stdio macros are `stdio`).
    builder: &'static str,
}

/// The covered set.
///
/// Deliberately small. Each entry was measured on a module that reaches it and
/// a module that does not, at `-c fastbuild` and `-c opt`, and admitted only on
/// a clean 0-then-1. Two facilities that did *not* survive that bar:
///
/// - **`Instant::now`** — present in a module that never calls it, because
///   std's `mpmc` channel links a timeout path that nothing here takes. It
///   would fire on every module, including a clean one.
/// - **`getrandom`** — already a legible compile error naming its own feature
///   flags. It is the shape this tool is imitating, not a gap.
const FACILITIES: &[Facility] = &[
    Facility {
        name: "std::time::SystemTime::now",
        segments: &["4time", "10SystemTime", "3now"],
        served_by: &["clock_time_get", "now_wall_ns"],
        consequence: "std links `panic!(\"time not implemented on this platform\")` \
                      on this platform, so the call compiles and then traps at run time",
        builder: "clock",
    },
    Facility {
        name: "println! / print! (std::io::stdio::_print)",
        segments: &["3std", "2io", "5stdio", "6_print"],
        served_by: &["fd_write", "write_stdio"],
        consequence: "stdout on this platform accepts every write and discards it, \
                      so the call succeeds and produces nothing — no output, no trap, \
                      no error",
        builder: "stdio",
    },
    Facility {
        name: "eprintln! / eprint! (std::io::stdio::_eprint)",
        segments: &["3std", "2io", "5stdio", "7_eprint"],
        served_by: &["fd_write", "write_stdio"],
        consequence: "stderr on this platform accepts every write and discards it, \
                      so the call succeeds and produces nothing — no output, no trap, \
                      no error",
        builder: "stdio",
    },
];

/// Proof that the name section is present *and* carries mangled Rust symbols.
///
/// Every frustrate module links a panic path — the panic-attribution shim in
/// `frustrate_web_init` guarantees one exists — so `core::panicking` is in any
/// module this tool is pointed at. Its absence means the names were stripped or
/// rewritten, and a scan over what remains would find nothing and report
/// success. That is the failure mode this whole tool exists to remove, so it is
/// an error rather than a pass.
const SENTINEL: &str = "9panicking";

const WASI: &str = "wasi_snapshot_preview1";
/// frustrate's own import namespace, which a custom_std asks through.
const FRUSTRATE: &str = "frustrate";

/// Does `name` contain `segment` under either mangling scheme?
///
/// The schemes agree on `<length><identifier>` except when the identifier
/// itself begins with `_` or a digit: v0 then writes a separator `_` between
/// the two so the boundary stays unambiguous, and legacy does not. Measured on
/// this repo's toolchain — `_underscore_leading_name` (24 bytes) is
/// `24__underscore_leading_name` under `-Csymbol-mangling-version=v0`.
///
/// It matters for exactly the two stdio facilities, whose std names are
/// `_print` and `_eprint`. Rather than spell both forms in every table entry —
/// where the wrong one would be a *silent* miss, the one failure mode this
/// tool must not have — a segment whose identifier starts with `_` is matched
/// against both spellings here.
///
/// Not currently load-bearing: the facilities are all in `std`, which ships
/// precompiled, so their mangling is a property of the Rust distribution and
/// not of the consumer's flags — measured, a module built with `-C
/// symbol-mangling-version=v0` still carries legacy-mangled std symbols. It is
/// here because that is a fact about today's distribution rather than a
/// guarantee, and the cost of being wrong about it is silence.
fn contains_segment(name: &str, segment: &str) -> bool {
    if name.contains(segment) {
        return true;
    }
    let digits = segment.len() - segment.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    let ident = &segment[digits..];
    digits > 0
        && ident.starts_with('_')
        && name.contains(&format!("{}_{}", &segment[..digits], ident))
}

fn uleb(d: &[u8], i: &mut usize) -> Result<u64, String> {
    let (mut r, mut s) = (0u64, 0u32);
    loop {
        let b = *d.get(*i).ok_or("truncated LEB128")?;
        *i += 1;
        r |= ((b & 0x7f) as u64) << s;
        if b & 0x80 == 0 {
            return Ok(r);
        }
        s += 7;
        if s > 63 {
            return Err("LEB128 too long".into());
        }
    }
}

fn name(d: &[u8], i: &mut usize) -> Result<String, String> {
    let len = uleb(d, i)? as usize;
    let end = i.checked_add(len).ok_or("name length overflow")?;
    let s = d.get(*i..end).ok_or("truncated name")?;
    *i = end;
    Ok(String::from_utf8_lossy(s).into_owned())
}

/// `(section id, body)` for each section, in order.
fn sections(d: &[u8]) -> Result<Vec<(u8, &[u8])>, String> {
    if d.len() < 8 || &d[..4] != b"\0asm" {
        return Err("not a wasm module".into());
    }
    let mut i = 8;
    let mut out = Vec::new();
    while i < d.len() {
        let id = d[i];
        i += 1;
        let size = uleb(d, &mut i)? as usize;
        let end = i.checked_add(size).ok_or("section overruns file")?;
        out.push((id, d.get(i..end).ok_or("truncated section")?));
        i = end;
    }
    Ok(out)
}

/// Every `(module, name)` in the import section (id 2).
fn imports(d: &[u8]) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for (id, body) in sections(d)? {
        if id != 2 {
            continue;
        }
        let mut i = 0;
        let count = uleb(body, &mut i)?;
        for _ in 0..count {
            let m = name(body, &mut i)?;
            let n = name(body, &mut i)?;
            let kind = *body.get(i).ok_or("truncated import kind")?;
            i += 1;
            match kind {
                0 => {
                    uleb(body, &mut i)?;
                }
                1 => {
                    i += 1; // reftype
                    let flags = *body.get(i).ok_or("truncated table limits")?;
                    i += 1;
                    uleb(body, &mut i)?;
                    if flags & 0x01 != 0 {
                        uleb(body, &mut i)?;
                    }
                }
                2 => {
                    let flags = *body.get(i).ok_or("truncated memory limits")?;
                    i += 1;
                    uleb(body, &mut i)?;
                    if flags & 0x01 != 0 {
                        uleb(body, &mut i)?;
                    }
                }
                3 => i += 2, // valtype + mutability
                k => return Err(format!("unknown import kind {k} in {m}.{n}")),
            }
            out.push((m, n));
        }
    }
    Ok(out)
}

/// Function names from the `name` custom section's function subsection (1).
fn function_names(d: &[u8]) -> Result<Vec<String>, String> {
    for (id, body) in sections(d)? {
        if id != 0 {
            continue;
        }
        let mut i = 0;
        if name(body, &mut i)? != "name" {
            continue;
        }
        let mut out = Vec::new();
        while i < body.len() {
            let sub = body[i];
            i += 1;
            let size = uleb(body, &mut i)? as usize;
            let end = i.checked_add(size).ok_or("name subsection overruns")?;
            if sub == 1 {
                let mut k = i;
                let count = uleb(body, &mut k)?;
                for _ in 0..count {
                    uleb(body, &mut k)?; // function index
                    out.push(name(body, &mut k)?);
                }
            }
            i = end;
        }
        return Ok(out);
    }
    Ok(Vec::new())
}

fn run(path: &str, label: &str, allowed: &[String]) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let names = function_names(&bytes)?;
    // Both namespaces, because both can carry a host. The names are disjoint
    // from the imports frustrate always supplies (post, panic, schedule_drain,
    // spawn_worker), so collecting the whole `frustrate` namespace cannot make
    // a stock-std module look served.
    let served: BTreeSet<String> = imports(&bytes)?
        .into_iter()
        .filter(|(m, _)| m == WASI || m == FRUSTRATE)
        .map(|(_, n)| n)
        .collect();

    if !names.iter().any(|n| n.contains(SENTINEL)) {
        return Err(format!(
            "frustrate_wasm_module: cannot verify std facility use for {label}.\n\
             \n\
             This module's wasm name section is missing, empty, or carries no \
             mangled Rust symbols (built with `-C strip=symbols`, or run through a \
             post-pass that rewrote the names?). The check reads function names to \
             find std facilities the target platform cannot serve, and a scan of \
             names that are not there would report success for a module it never \
             examined — so it errors instead.\n\
             \n\
             Keep the name section, or set `std_check = \"off\"` on this target and \
             say in a comment why the module is exempt."
        ));
    }

    let unknown: Vec<&String> = allowed
        .iter()
        .filter(|a| !FACILITIES.iter().any(|f| f.name == a.as_str()))
        .collect();
    if !unknown.is_empty() {
        // A waiver for something this tool never checks is a mistake, and a
        // silent one: it reads as coverage the author does not have. Same
        // treatment every contract in this repo gets when it cannot bite.
        return Err(format!(
            "frustrate_wasm_module: {label}: `std_check_allow` names {} , which \
             this check does not cover, so the entry does nothing. Covered: {}",
            unknown
                .iter()
                .map(|u| format!("`{u}`"))
                .collect::<Vec<_>>()
                .join(", "),
            FACILITIES
                .iter()
                .map(|f| format!("`{}`", f.name))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    let hit: Vec<&Facility> = FACILITIES
        .iter()
        .filter(|f| {
            !allowed.iter().any(|a| a == f.name)
                && !f.served_by.iter().any(|i| served.contains(*i))
                && names
                    .iter()
                    .any(|n| f.segments.iter().all(|s| contains_segment(n, s)))
        })
        .collect();

    if hit.is_empty() {
        return Ok(());
    }

    let mut msg = format!(
        "frustrate_wasm_module: {label} reaches std facilities its wasm platform \
         does not serve:\n\n"
    );
    for f in &hit {
        msg.push_str(&format!("  {} — {}.\n\n", f.name, f.consequence));
    }
    // The exact list for this module, so the command can be run rather than
    // adapted. Deduped because both stdio macros are one facility.
    let mut needed: Vec<&str> = hit.iter().map(|f| f.builder).collect();
    needed.sort_unstable();
    needed.dedup();
    let facilities = needed.join(",");
    msg.push_str(&format!(
        "Nothing in this module answers these calls -- it imports neither a wasi \
         host nor the frustrate facility that would serve them. Two ways to get \
         one, and they are different trades:\n\
         \n\
         1. Switch target to the wasi platform, whose std serves them outright:\n\
         \n\
         \x20   frustrate_wasm_module(\n\
         \x20       name = \"...\",\n\
         \x20       crate = \"...\",\n\
         \x20       platform = \"@frustrate//bazel:wasm32_wasi\",\n\
         \x20   )\n\
         \n\
         \x20  It costs module size and rules out web-sys, which is why it is not \
         the default.\n\
         \n\
         2. Keep this target and build a std that serves the facility:\n\
         \n\
         \x20   bazel run //toolchain/custom_std:build -- --facilities={facilities}\n\
         \n\
         \x20  That list REPLACES a flavour's facilities rather than adding to \
         them, so it has to name every one this build already relies on. A \
         threaded target (//bazel:wasm32_threads) needs `atomics` in it too, or \
         the std it produces is not the one that target loads.\n\
         \n\
         \x20  It keeps web-sys and the module size, and costs a local build step \
         plus the pinned nightly.\n\
         \n\
         If the call is in a dependency, on a path this module never takes, waive \
         that one facility with `std_check_allow` and say in a comment which call \
         site you looked at — the others stay guarded. `std_check = \"off\"` waives \
         all of them and is the blunter instrument.",
    ));
    Err(msg)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!(
            "usage: wasm_std_check <module.wasm> <label> <marker-out> [allowed-facility...]"
        );
        return ExitCode::FAILURE;
    }
    match run(&args[1], &args[2], &args[4..]) {
        Ok(()) => {
            // The marker is the action's declared output; writing it is what
            // lets Bazel cache a pass. Nothing ever reads its contents.
            if let Err(e) = std::fs::write(&args[3], b"ok\n") {
                eprintln!("wasm_std_check: writing {}: {e}", args[3]);
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Err(msg) => {
            eprintln!("{msg}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal module: magic + version, one custom `name` section carrying a
    /// function subsection with the given names, and one import section.
    fn module(names: &[&str], imports: &[(&str, &str)]) -> Vec<u8> {
        fn uleb(v: usize, out: &mut Vec<u8>) {
            let mut v = v;
            loop {
                let b = (v & 0x7f) as u8;
                v >>= 7;
                if v == 0 {
                    out.push(b);
                    return;
                }
                out.push(b | 0x80);
            }
        }
        fn s(text: &str, out: &mut Vec<u8>) {
            uleb(text.len(), out);
            out.extend_from_slice(text.as_bytes());
        }

        let mut m = b"\0asm\x01\0\0\0".to_vec();

        let mut imp = Vec::new();
        uleb(imports.len(), &mut imp);
        for (module_name, field) in imports {
            s(module_name, &mut imp);
            s(field, &mut imp);
            imp.push(0); // func
            uleb(0, &mut imp); // typeidx
        }
        m.push(2);
        uleb(imp.len(), &mut m);
        m.extend_from_slice(&imp);

        let mut sub = Vec::new();
        uleb(names.len(), &mut sub);
        for (i, n) in names.iter().enumerate() {
            uleb(i, &mut sub);
            s(n, &mut sub);
        }
        let mut custom = Vec::new();
        s("name", &mut custom);
        custom.push(1);
        uleb(sub.len(), &mut custom);
        custom.extend_from_slice(&sub);
        m.push(0);
        uleb(custom.len(), &mut m);
        m.extend_from_slice(&custom);
        m
    }

    /// The real symbol shapes, copied from modules built by this repo's rules.
    const PANIC: &str = "_ZN4core9panicking9panic_fmt17h0b1b2c3d4e5f6071E";
    const SYSTEM_TIME_NOW: &str = "_ZN3std4time10SystemTime3now17hc957c6f32bad0aaaE";
    const PRINT: &str = "_ZN3std2io5stdio6_print17hb87c02292a7dc51cE";
    const INSTANT_NOW: &str = "_ZN3std4time7Instant3now17h8cc72c4d8ca5c88fE";

    fn check(names: &[&str], imports: &[(&str, &str)]) -> Result<(), String> {
        check_allowing(names, imports, &[])
    }

    /// Build the module, write it to a path **no other call can name**, and
    /// check it.
    ///
    /// A fresh counter value per call, not a hash of the inputs. These tests
    /// run in parallel in one process, and content-addressing is actively
    /// wrong here: two different tests legitimately build the *same* module —
    /// `PRINT` and the legacy spelling in
    /// `a_print_symbol_is_found_under_either_mangling_scheme` are the same
    /// symbol — so they would share a path, and `fs::write` truncates before
    /// it writes. One test then reads the other's half-written file. That is
    /// what the intermittent failure was: an empty read, an "unknown format"
    /// error, and an assertion about `println!` that never got its message.
    ///
    /// Two earlier versions keyed on `names.len()` and on the waiver list's
    /// length. The lesson is not "hash harder" — it is that the path must be
    /// unique per *call*, because identical inputs are a thing tests do.
    fn check_allowing(
        names: &[&str],
        imports: &[(&str, &str)],
        allowed: &[&str],
    ) -> Result<(), String> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let bytes = module(names, imports);
        let dir = std::env::temp_dir().join(format!("wasm_std_check_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wasm = dir.join(format!("m{}.wasm", NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::write(&wasm, bytes).unwrap();
        let owned: Vec<String> = allowed.iter().map(|s| s.to_string()).collect();
        run(wasm.to_str().unwrap(), "//pkg:target.wasm", &owned)
    }

    #[test]
    fn a_module_reaching_nothing_unsupported_passes() {
        assert!(check(&[PANIC, "add"], &[]).is_ok());
    }

    #[test]
    fn system_time_without_a_clock_names_the_platform_that_has_one() {
        let e = check(&[PANIC, SYSTEM_TIME_NOW], &[]).unwrap_err();
        assert!(e.contains("std::time::SystemTime::now"), "{e}");
        assert!(e.contains("traps at run time"), "{e}");
        assert!(e.contains("//bazel:wasm32_wasi"), "{e}");
        assert!(e.contains("//pkg:target.wasm"), "{e}");
    }

    #[test]
    fn system_time_with_a_clock_behind_it_passes() {
        // The wasi platform: same symbol, but the module imports a clock. This
        // is the half that keeps //bazel:wasm32_wasi green without the rule
        // holding a platform table.
        assert!(check(
            &[PANIC, SYSTEM_TIME_NOW],
            &[("wasi_snapshot_preview1", "clock_time_get")]
        )
        .is_ok());
    }

    #[test]
    fn print_says_the_output_is_discarded_rather_than_lost() {
        let e = check(&[PANIC, PRINT], &[]).unwrap_err();
        assert!(e.contains("println!"), "{e}");
        assert!(e.contains("no output, no trap, no error"), "{e}");
    }

    #[test]
    fn print_with_fd_write_behind_it_passes() {
        assert!(check(&[PANIC, PRINT], &[("wasi_snapshot_preview1", "fd_write")]).is_ok());
    }

    /// The custom_std branch: the same facility, served by frustrate's own
    /// import rather than by a wasi one. A module built against a std with
    /// `--facilities=clock` imports `frustrate.now_wall_ns` and has a real
    /// clock behind `SystemTime::now`, so keying only on the wasi name would
    /// fail a module that is fine (toolchain/custom_std).
    #[test]
    fn a_custom_std_clock_serves_system_time() {
        assert!(check(
            &[PANIC, SYSTEM_TIME_NOW],
            &[("frustrate", "now_wall_ns")]
        )
        .is_ok());
    }

    /// The recipe has to be runnable, not illustrative.
    ///
    /// It used to be a fixed `--facilities=clock,stdio` whatever the module
    /// needed, which is wrong twice over: it names facilities this build does
    /// not want, and — because the list REPLACES a flavour's rather than adding
    /// to it — following it on a threaded target silently drops `atomics` and
    /// produces a std that target does not load. So the list is computed, and
    /// the replacement trap is stated.
    #[test]
    fn the_rebuild_recipe_names_this_modules_facilities_and_the_replace_trap() {
        let err = check(&[PANIC, PRINT], &[]).unwrap_err();
        assert!(err.contains("--facilities=stdio"), "{err}");
        assert!(!err.contains("clock"), "clock is not what this module needs: {err}");
        assert!(err.contains("REPLACES"), "{err}");
        assert!(err.contains("atomics"), "{err}");

        // Two facilities, deduped and sorted, when the module reaches both.
        let both = check(&[PANIC, PRINT, SYSTEM_TIME_NOW], &[]).unwrap_err();
        assert!(both.contains("--facilities=clock,stdio"), "{both}");
    }

    #[test]
    fn a_custom_std_stdio_serves_print_and_eprint() {
        assert!(check(&[PANIC, PRINT], &[("frustrate", "write_stdio")]).is_ok());
        let eprint = "_ZN3std2io5stdio7_eprint17hb87c02292a7dc51cE";
        assert!(check(&[PANIC, eprint], &[("frustrate", "write_stdio")]).is_ok());
    }

    /// The imports frustrate always supplies must not be mistaken for a
    /// facility server: a stock-std module imports `frustrate.post` and still
    /// has no clock.
    #[test]
    fn frustrates_own_imports_do_not_serve_a_facility() {
        let e = check(
            &[PANIC, SYSTEM_TIME_NOW],
            &[("frustrate", "post"), ("frustrate", "panic")],
        )
        .unwrap_err();
        assert!(e.contains("SystemTime::now"), "{e}");
    }

    /// Both remedies are named, because they are different trades: the wasi
    /// platform swaps the whole target, a facility keeps it and adds a step.
    #[test]
    fn the_message_names_both_ways_to_get_a_clock() {
        let e = check(&[PANIC, SYSTEM_TIME_NOW], &[]).unwrap_err();
        assert!(e.contains("//bazel:wasm32_wasi"), "{e}");
        assert!(e.contains("--facilities"), "{e}");
    }

    /// The measured false positive. std's mpmc channel links a timeout path
    /// into modules that never take it, so a module carrying this symbol is not
    /// evidence of anything and must not fail the build.
    #[test]
    fn instant_is_not_covered_because_a_clean_module_carries_it() {
        assert!(check(&[PANIC, INSTANT_NOW], &[]).is_ok());
    }

    /// Naming the type is not calling it: the length-prefixed segments are what
    /// separate `SystemTime` in a Debug impl from `SystemTime::now`.
    #[test]
    fn merely_naming_the_type_is_not_a_hit() {
        let debug_impl =
            "_ZN54_$LT$std..time..SystemTime$u20$as$u20$core..fmt..Debug$GT$3fmt17habcE";
        assert!(check(&[PANIC, debug_impl], &[]).is_ok());
    }

    /// The failure this tool exists to remove, applied to the tool itself: with
    /// no names to read, every facility scan comes back empty, which is
    /// indistinguishable from a clean module.
    #[test]
    fn a_stripped_name_section_is_an_error_not_a_pass() {
        let e = check(&[], &[]).unwrap_err();
        assert!(e.contains("cannot verify"), "{e}");
        assert!(e.contains("strip=symbols"), "{e}");
        assert!(e.contains("std_check = \"off\""), "{e}");
    }

    /// A name section that exists but carries demangled or rewritten names —
    /// what a post-pass leaves behind — reads as "cannot verify" for the same
    /// reason, rather than as a clean module.
    #[test]
    fn names_without_mangled_rust_symbols_are_an_error_too() {
        let e = check(&["core::panicking::panic_fmt", "add"], &[]).unwrap_err();
        assert!(e.contains("cannot verify"), "{e}");
    }

    /// A waiver is per facility, so the others keep biting. This is what makes
    /// the escape hatch usable for a real dependency graph without turning the
    /// whole check off for the code the author actually wrote.
    #[test]
    fn a_waiver_covers_one_facility_and_not_the_others() {
        assert!(check_allowing(
            &[PANIC, SYSTEM_TIME_NOW],
            &[],
            &["std::time::SystemTime::now"]
        )
        .is_ok());

        let e = check_allowing(
            &[PANIC, SYSTEM_TIME_NOW, PRINT],
            &[],
            &["std::time::SystemTime::now"],
        )
        .unwrap_err();
        assert!(e.contains("println!"), "{e}");
        assert!(!e.contains("SystemTime::now — "), "waived facility still named: {e}");
    }

    /// A waiver for something never checked reads as coverage the author does
    /// not have, so it is a mistake rather than a no-op — the same treatment
    /// FR0030 gives an opt-in that cannot bite.
    #[test]
    fn a_waiver_naming_an_uncovered_facility_is_an_error() {
        let e = check_allowing(&[PANIC], &[], &["std::thread::sleep"]).unwrap_err();
        assert!(e.contains("does not cover"), "{e}");
        assert!(e.contains("std::time::SystemTime::now"), "lists what is covered: {e}");
    }

    /// v0 writes a separator `_` when the identifier itself starts with `_`,
    /// so std's `_print` is `6__print` there and `6_print` here. A miss would
    /// be silent, so both spellings are pinned.
    #[test]
    fn a_print_symbol_is_found_under_either_mangling_scheme() {
        // Measured shapes: legacy from a module this repo builds, v0 from the
        // same crate built with -Csymbol-mangling-version=v0.
        let legacy = "_ZN3std2io5stdio6_print17hb87c02292a7dc51cE";
        let v0 = "_RNvNtNtCsefCTQm38txr_3std2io5stdio6__print";
        for sym in [legacy, v0] {
            let e = check(&[PANIC, sym], &[]).unwrap_err();
            assert!(e.contains("println!"), "missed in {sym}: {e}");
        }
    }

    /// The separator only appears where it is needed, so a segment that does
    /// not start with `_` must not match a doubled spelling that never occurs.
    #[test]
    fn the_separator_tolerance_does_not_loosen_other_segments() {
        assert!(contains_segment("_ZN3std2io5stdio6_print17hE", "6_print"));
        assert!(contains_segment("_RNvNtNtCs_3std2io5stdio6__print", "6_print"));
        assert!(!contains_segment("_ZN3std4time10SystemTime3nowE", "3__now"));
        assert!(!contains_segment("_ZN3std4time7Instant3nowE", "10SystemTime"));
    }

    #[test]
    fn a_file_that_is_not_wasm_is_rejected() {
        let dir = std::env::temp_dir().join(format!("wasm_std_check_junk_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("j.wasm");
        std::fs::write(&p, b"not a wasm file at all").unwrap();
        assert!(run(p.to_str().unwrap(), "//pkg:x", &[])
            .unwrap_err()
            .contains("not a wasm module"));
    }
}
