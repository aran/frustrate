//! The check configuration's linker: `rust-lld` with the export list cut back
//! to the check roots.
//!
//! Reached only through `-Clinker=`, in the `--cfg frustrate_block_check`
//! configuration: `tools/check_block.dart` sets it for the cargo flow and
//! `frustrate_block_check`'s transition for the Bazel one. Nothing that ships is
//! linked through it.
//!
//! The check artifact's premise is **exports == roots**: the module's only
//! entry points are one `frustrate_check_block_*` root per claimed member, so
//! `--gc-sections` deletes what the claims cannot reach and a surviving
//! `memory.atomic.wait` means a claimed body reaches a wait. `--cfg` can
//! suppress frustrate's own `#[no_mangle]`s, but not a `#[wasm_bindgen]` export
//! inside a dependency rlib. rustc passes wasm-ld one explicit `--export`
//! argument per exported symbol, those included, so dropping the argument here
//! removes the export and lets the GC delete the body.
//!
//! # What this can and cannot break
//!
//! It only ever *removes* `--export` arguments, so an export it fails to remove
//! survives into the module, where `wasm_block_check` refuses it by name
//! ("every function export must be a check root"). This can fail to help; it
//! cannot make a scan mean less than it says.
//!
//! Dropping a *root* is the direction that would be dangerous, and the
//! fail-closed check in `main` guards it: a check module is linked only when
//! some claim wants a root, so zero roots kept means rustc no longer emits one
//! `--export` per symbol, and the link stops rather than producing a module
//! scanned over nothing.
//!
//! # The arguments
//!
//! Both spellings are handled: `--export <sym>` is rustc's form for a collected
//! symbol, `--export=<sym>` its form for `__heap_base`/`__data_end` and what
//! `-Clink-arg` produces. Nothing else is touched: `--export-if-defined`,
//! `--export-all` and `--export-table` pass through, and if one of them ever
//! creates an export the scanner names it.
//!
//! `@response-file` arguments are expanded before filtering, and re-spilled
//! after, so a filter that walked argv without expanding cannot pass every
//! export through in silence on a platform with a small argument limit.
//!
//! # Where it runs
//!
//! Bazel puts it in the custom-std toolchains' sysroot, at
//! `lib/rustlib/<host>/bin/frustrate-link-filter`, built for each execution
//! host; rustc prepends that directory to the linker's PATH, which is how the
//! bare `-Clinker=frustrate-link-filter` finds it and how this finds
//! `rust-lld`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// Codegen's check roots. Kept, always. Spelled the same as `ROOT_PREFIX` in
/// main.rs.
const ROOT_PREFIX: &str = "frustrate_check_block_";

/// Exports that are not codegen's and are kept anyway.
///
/// `__heap_base` and `__data_end` are rustc's own, on every wasm link.
/// `__stack_pointer`, `__wasm_init_tls`, `__tls_base`, `__tls_size` and
/// `__tls_align` come from the +atomics toolchain's link arguments under Bazel.
/// Only `__wasm_init_tls` is a *function* export, and the scanner exempts
/// exactly it (`LINKER_EXPORTS` in main.rs); the other four are globals, which
/// its export walk skips.
const KEEP_EXPORTS: &[&str] = &[
    "__heap_base",
    "__data_end",
    "__stack_pointer",
    "__wasm_init_tls",
    "__tls_base",
    "__tls_size",
    "__tls_align",
];

/// The linker actually run, resolved on PATH: rustc prepends its own
/// toolchain's `lib/rustlib/<host>/bin` to the child's PATH in both flows, so
/// this finds that toolchain's `rust-lld`.
const LINKER: &str = "rust-lld";

/// How deep `@file` nesting may go before this calls it a cycle.
const MAX_RESPONSE_DEPTH: usize = 8;

const PROG: &str = "wasm_block_check link filter";

/// `args` with every `@file` replaced by the arguments it holds. `read` is
/// injected so the tests need no disk.
fn expand_response_files(
    args: Vec<String>,
    read: &dyn Fn(&str) -> Result<String, String>,
    depth: usize,
) -> Result<Vec<String>, String> {
    if depth > MAX_RESPONSE_DEPTH {
        return Err(format!(
            "@response-file nesting is more than {MAX_RESPONSE_DEPTH} deep, which is a cycle."
        ));
    }
    let mut out = Vec::new();
    for a in args {
        match a.strip_prefix('@') {
            Some(name) if !name.is_empty() => {
                let text =
                    read(name).map_err(|e| format!("cannot read the response file {name}: {e}"))?;
                out.extend(expand_response_files(tokenize(&text), read, depth + 1)?);
            }
            _ => out.push(a),
        }
    }
    Ok(out)
}

/// Splits a response file the way LLVM's GNU tokenizer does: whitespace
/// separates, a backslash escapes the next character outside single quotes,
/// and quotes group.
fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_token = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if in_token {
                    out.push(std::mem::take(&mut cur));
                    in_token = false;
                }
            }
            '\\' => {
                in_token = true;
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            '\'' | '"' => {
                in_token = true;
                while let Some(n) = chars.next() {
                    if n == c {
                        break;
                    }
                    if n == '\\' && c == '"' {
                        if let Some(e) = chars.next() {
                            cur.push(e);
                        }
                        continue;
                    }
                    cur.push(n);
                }
            }
            c => {
                in_token = true;
                cur.push(c);
            }
        }
    }
    if in_token {
        out.push(cur);
    }
    out
}

/// `args` as the body of a response file [`tokenize`] (and lld) read back to
/// `args`: one single-quoted argument per line.
fn quote_response(args: &[String]) -> String {
    args.iter()
        .map(|a| format!("'{}'\n", a.replace('\'', "'\\''")))
        .collect()
}

fn keep(sym: &str) -> bool {
    sym.starts_with(ROOT_PREFIX) || KEEP_EXPORTS.contains(&sym)
}

struct Filtered {
    args: Vec<String>,
    roots: Vec<String>,
    dropped: usize,
}

/// `args` with every `--export` of a symbol this module has no use for gone.
/// Order is otherwise preserved exactly.
fn filter_exports(args: Vec<String>) -> Result<Filtered, String> {
    let mut f = Filtered {
        args: Vec::new(),
        roots: Vec::new(),
        dropped: 0,
    };
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let (sym, spelled) = if a == "--export" {
            let Some(sym) = it.next() else {
                return Err("`--export` is the last argument, with no symbol after it. \
                            This is not a command line rustc emits."
                    .into());
            };
            (sym.clone(), vec![a, sym])
        } else if let Some(sym) = a.strip_prefix("--export=") {
            (sym.to_string(), vec![a.clone()])
        } else {
            f.args.push(a);
            continue;
        };
        if keep(&sym) {
            if sym.starts_with(ROOT_PREFIX) {
                f.roots.push(sym);
            }
            f.args.extend(spelled);
        } else {
            f.dropped += 1;
        }
    }
    Ok(f)
}

/// Refuses a link that is not the wasm one: removing `--export` arguments is
/// right for the check module and wrong everywhere else. `-flavor` absent is
/// not an error; it only means rustc stopped spelling the invocation that way.
fn check_flavor(args: &[String]) -> Result<(), String> {
    for w in args.windows(2) {
        if w[0] == "-flavor" && w[1] != "wasm" {
            return Err(format!(
                "this is a `-flavor {}` link, not a wasm one. Nothing but the block \
                 check's throwaway module should be linked through this.",
                w[1]
            ));
        }
    }
    Ok(())
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// Makes `rust-lld` loadable on macOS. It links `libLLVM.dylib` through an
/// `LC_RPATH` of `@loader_path/../lib`, which resolves to the sysroot's
/// `lib/rustlib/<host>/lib`, while the toolchain ships the dylib at its root
/// `lib/`; run as-is it dies with `Library not loaded: @rpath/libLLVM.dylib`.
/// rustc never runs it that way for this target, so only this filter needs
/// it. The directory is derived from the resolved linker:
/// `<toolchain>/lib/rustlib/<host>/bin/rust-lld` -> `<toolchain>/lib`. A no-op
/// off macOS, or when the dylib is not there.
fn darwin_library_path(linker: &Path) -> Option<OsString> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let lib = std::fs::canonicalize(linker)
        .ok()?
        .parent()?
        .join("../../..");
    let lib = std::fs::canonicalize(lib).ok()?;
    if !lib.join("libLLVM.dylib").exists() {
        return None;
    }
    let mut v = OsString::from(lib);
    if let Some(existing) = std::env::var_os("DYLD_LIBRARY_PATH") {
        v.push(":");
        v.push(existing);
    }
    Some(v)
}

fn run() -> Result<i32, String> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let spilled = argv.iter().any(|a| a.len() > 1 && a.starts_with('@'));
    let args = expand_response_files(
        argv,
        &|n| std::fs::read_to_string(n).map_err(|e| e.to_string()),
        0,
    )?;
    check_flavor(&args)?;
    let kept = filter_exports(args)?;
    if kept.roots.is_empty() {
        return Err(format!(
            "this link exports no `{ROOT_PREFIX}*` symbol, so there is nothing to check.\n\n\
             A check module is linked only when some `#[bridge(no_block)]` claim wants \
             artifact\nevidence, and codegen emits one root per such claim, so every link \
             that reaches this\nfilter carries at least one. Zero means the mechanism this \
             rests on changed: rustc no\nlonger passing one `--export` argument per exported \
             symbol is the way that happens.\nStopping, because the alternative is a module \
             scanned over none of its claims that\nstill reports clean. Re-measure with \
             `rustc --print link-args`."
        ));
    }

    let linker = which(LINKER).ok_or_else(|| {
        format!(
            "`{LINKER}` is not on PATH.\n\nrustc puts its own toolchain's \
             lib/rustlib/<host>/bin on the linker's PATH, which is\nwhere this expects \
             to find it. A PATH without it means the invocation did not come\nfrom rustc, \
             or the toolchain is incomplete."
        )
    })?;
    let mut cmd = Command::new(&linker);
    if let Some(p) = darwin_library_path(&linker) {
        cmd.env("DYLD_LIBRARY_PATH", p);
    }

    // Re-spill only if rustc spilled: the filtered line is never longer than
    // what arrived. In TMPDIR, since the original may sit in an output tree a
    // sandbox need not let anyone write to.
    let status = if spilled {
        let path = std::env::temp_dir().join(format!("frustrate-link-{}.rsp", std::process::id()));
        std::fs::write(&path, quote_response(&kept.args))
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        let mut at = OsString::from("@");
        at.push(&path);
        let status = cmd.arg(at).status();
        let _ = std::fs::remove_file(&path);
        status
    } else {
        cmd.args(&kept.args).status()
    }
    .map_err(|e| format!("cannot run {}: {e}", linker.display()))?;
    Ok(status.code().unwrap_or(1))
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("{PROG}: {e}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn filters_both_spellings_and_keeps_everything_else_in_order() {
        let f = filter_exports(s(&[
            "-flavor",
            "wasm",
            "--export",
            "frustrate_check_block_0_add",
            "--export",
            "some_wasm_bindgen_export",
            "--export=__heap_base",
            "--export=__data_end",
            "--export=__wbindgen_malloc",
            "--gc-sections",
            "-o",
            "out.wasm",
            "--export=__wasm_init_tls",
            "--export=__tls_base",
        ]))
        .unwrap();
        assert_eq!(
            f.args,
            s(&[
                "-flavor",
                "wasm",
                "--export",
                "frustrate_check_block_0_add",
                "--export=__heap_base",
                "--export=__data_end",
                "--gc-sections",
                "-o",
                "out.wasm",
                "--export=__wasm_init_tls",
                "--export=__tls_base",
            ])
        );
        assert_eq!(f.roots, s(&["frustrate_check_block_0_add"]));
        assert_eq!(f.dropped, 2);
    }

    #[test]
    fn lookalike_flags_are_not_exports() {
        let args = s(&["--export-if-defined", "x", "--export-all", "--export-table"]);
        let f = filter_exports(args.clone()).unwrap();
        assert_eq!(f.args, args);
        assert!(f.roots.is_empty());
    }

    #[test]
    fn no_root_left_is_reported_as_zero_roots() {
        assert!(filter_exports(s(&["--export", "unrelated"]))
            .unwrap()
            .roots
            .is_empty());
    }

    #[test]
    fn a_trailing_export_is_refused() {
        assert!(filter_exports(s(&["--export"])).is_err());
    }

    #[test]
    fn a_native_link_is_refused() {
        assert!(check_flavor(&s(&["-flavor", "ld64"])).is_err());
        assert!(check_flavor(&s(&["-flavor", "wasm"])).is_ok());
        assert!(check_flavor(&s(&["-o", "x"])).is_ok());
    }

    #[test]
    fn response_files_are_expanded_before_filtering() {
        let read = |n: &str| match n {
            "args.rsp" => {
                Ok("--export frustrate_check_block_1_b\n--export stray\n@more.rsp\n".into())
            }
            "more.rsp" => Ok("'--export=__heap_base'\n".into()),
            _ => Err("absent".into()),
        };
        let expanded =
            expand_response_files(s(&["-flavor", "wasm", "@args.rsp"]), &read, 0).unwrap();
        assert_eq!(
            expanded,
            s(&[
                "-flavor",
                "wasm",
                "--export",
                "frustrate_check_block_1_b",
                "--export",
                "stray",
                "--export=__heap_base",
            ])
        );
        let f = filter_exports(expanded).unwrap();
        assert_eq!(f.roots, s(&["frustrate_check_block_1_b"]));
        assert_eq!(f.dropped, 1);
    }

    #[test]
    fn a_response_file_cycle_is_refused() {
        let read = |_: &str| Ok("@loop.rsp".to_string());
        assert!(expand_response_files(s(&["@loop.rsp"]), &read, 0).is_err());
    }

    #[test]
    fn rustc_escapes_are_read_back() {
        // rustc spills with backslash-escaped spaces and backslashes.
        assert_eq!(
            tokenize("-o\n/a\\ b/out.wasm\nC:\\\\x\n"),
            s(&["-o", "/a b/out.wasm", "C:\\x"])
        );
        assert_eq!(tokenize("\"a \\\"b\\\"\" ''"), s(&["a \"b\"", ""]));
    }

    #[test]
    fn what_this_writes_is_read_back_unchanged() {
        let awkward = s(&[
            "-o",
            "/a b/out.wasm",
            "--export",
            "it's",
            "--say=\"hi\"",
            "@x",
            "back\\slash",
            "",
        ]);
        assert_eq!(tokenize(&quote_response(&awkward)), awkward);
    }
}
