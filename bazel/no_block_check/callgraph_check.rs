//! The IR reader's direct calls, checked against LLVM's own call graph.
//!
//! [`super::parse`] reads rustc's IR as text. This runs `opt
//! -passes=print-callgraph`, from the llvm-tools of the Rust release that
//! wrote the module, over the same check-build IR the native analyses scan,
//! and compares the two per function. LLVM builds its graph from the parsed
//! module: an edge for each call site whose callee is a function (a `call`,
//! `invoke` or `callbr` of a named function), and none for a call through a
//! value. So the comparison is over direct calls only; the address-takes and
//! indirect calls that [`super::provenance`] follows are outside it.
//!
//! It fails when, for a function the module defines,
//!
//! - LLVM has a direct callee the reader does not (an edge the analysis
//!   would never follow), or
//! - the reader has a direct callee LLVM does not (it took something else for
//!   a call), or
//! - LLVM has a node with calls for a function the reader did not see defined.
//!
//! What the reader does on purpose is mapped before comparing, never
//! tolerated after: it drops `llvm.*` intrinsics, names a Linux `syscall`
//! after the kernel call (`syscall::<name>`, still a call to `syscall`), and
//! records inline asm with instructions as a call to an external named
//! `asm "..."` (in LLVM a call through an asm value, which has no callee).
//!
//! It cannot compare, and fails saying so, when `opt` is not from the rustc
//! that wrote the module (the module's `rustc version` and `opt --version`
//! disagree) or when it cannot read `opt`'s output.

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

/// `opt`'s call graph: each function it has a node for, with the functions
/// its call sites call.
fn llvm_callgraph(opt: &str, ll: &str) -> Result<BTreeMap<String, BTreeSet<String>>, String> {
    let out = Command::new(opt)
        .args(["-passes=print-callgraph", "-disable-output", ll])
        .output()
        .map_err(|e| format!("running {opt}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{opt} on {ll}: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let text = String::from_utf8_lossy(&out.stderr);
    let mut graph = BTreeMap::new();
    // `None` inside the external node's block, whose edges are callers.
    let mut current: Option<String> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Call graph node for function: '") {
            let Some(end) = rest.rfind("'<<0x") else {
                return Err(format!("unreadable node line: `{line}`"));
            };
            let name = rest[..end].to_string();
            graph.entry(name.clone()).or_insert_with(BTreeSet::new);
            current = Some(name);
        } else if line.starts_with("Call graph node <<null function>>") {
            current = None;
        } else if let Some(rest) = line.trim_start().strip_prefix("CS<") {
            let Some((_, edge)) = rest.split_once("> ") else {
                return Err(format!("unreadable edge line: `{line}`"));
            };
            let Some(f) = &current else { continue };
            if edge == "calls external node" {
                continue;
            }
            let Some(callee) = edge
                .strip_prefix("calls function '")
                .and_then(|c| c.strip_suffix('\''))
            else {
                return Err(format!("unreadable edge line: `{line}`"));
            };
            graph.get_mut(f).unwrap().insert(callee.to_string());
        } else if !line.trim().is_empty() {
            return Err(format!("unreadable line in opt's call graph: `{line}`"));
        }
    }
    if graph.is_empty() {
        return Err(format!("{opt} printed no call graph for {ll}"));
    }
    Ok(graph)
}

/// The Rust release `opt --version` was built for, as in
/// `LLVM version 23.1.1-rust-1.99.1-stable`.
fn opt_release(opt: &str) -> Result<String, String> {
    let out = Command::new(opt)
        .arg("--version")
        .output()
        .map_err(|e| format!("running {opt}: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace()
        .find_map(|w| {
            w.split_once("-rust-")
                .map(|(_, r)| r.split('-').next().unwrap_or("").to_string())
        })
        .ok_or_else(|| format!("`{opt} --version` names no Rust release: {text}"))
}

/// The Rust release that wrote the module, from its `rustc version` ident.
fn module_release(text: &str) -> Option<&str> {
    let at = text.rfind("!{!\"rustc version ")? + "!{!\"rustc version ".len();
    text[at..].split_whitespace().next()
}

/// One module's disagreements between the reader and LLVM, each a line.
fn compare(opt: &str, ll: &str) -> Result<Vec<String>, String> {
    let text = std::fs::read_to_string(ll).map_err(|e| format!("{ll}: {e}"))?;
    let release = opt_release(opt)?;
    match module_release(&text) {
        Some(r) if r == release => {}
        r => {
            return Err(format!(
                "{ll} was written by rustc {r:?} but opt is from Rust {release}: \
                 their call graphs are not comparable"
            ))
        }
    }
    let ir = super::parse(&text)?;
    // A crate whose module defines no function (one of types and constants
    // alone) has no call graph to compare, and `opt` prints none.
    if !ir
        .defined
        .iter()
        .enumerate()
        .any(|(i, d)| *d && text.contains(&format!("@{}(", ir.names[i])))
    {
        return Ok(Vec::new());
    }
    let llvm = llvm_callgraph(opt, ll)?;
    let mut out = Vec::new();
    for (f, callees) in llvm.iter().filter(|(f, _)| !f.starts_with("llvm.")) {
        let llvm_calls: BTreeSet<&str> = callees
            .iter()
            .map(String::as_str)
            .filter(|c| !c.starts_with("llvm."))
            .collect();
        let Some(&id) = ir.index.get(f) else {
            out.push(format!("LLVM has `{f}`, which the reader never saw"));
            continue;
        };
        if !ir.defined[id as usize] {
            if !llvm_calls.is_empty() {
                out.push(format!(
                    "LLVM has calls from `{f}`, which the reader did not see defined"
                ));
            }
            continue;
        }
        let reader_calls: BTreeSet<&str> = ir.calls[id as usize]
            .iter()
            .map(|&c| ir.names[c as usize].as_str())
            .filter(|c| !c.starts_with("asm \""))
            .map(|c| {
                if c.starts_with("syscall::") {
                    "syscall"
                } else {
                    c
                }
            })
            .collect();
        for c in llvm_calls.difference(&reader_calls) {
            out.push(format!("`{f}` calls `{c}` and the reader misses it"));
        }
        for c in reader_calls.difference(&llvm_calls) {
            out.push(format!(
                "the reader has `{f}` call `{c}`, which LLVM does not"
            ));
        }
    }
    Ok(out)
}

#[test]
fn the_readers_direct_calls_are_llvms() {
    let opt = std::env::var("OPT").expect("OPT: the `opt` to run");
    let modules: Vec<String> = std::env::var("MODULES")
        .expect("MODULES: the IR to compare")
        .split_whitespace()
        .filter(|p| p.ends_with(".module.ll"))
        .map(str::to_string)
        .collect();
    assert!(!modules.is_empty(), "MODULES names no `.module.ll`");
    let mut failures = Vec::new();
    for ll in &modules {
        match compare(&opt, ll) {
            Ok(diffs) => failures.extend(diffs.into_iter().map(|d| format!("{ll}: {d}"))),
            Err(e) => failures.push(e),
        }
    }
    assert!(
        failures.is_empty(),
        "{} disagreements, the first {}:\n{}",
        failures.len(),
        failures.len().min(50),
        failures[..failures.len().min(50)].join("\n")
    );
}
