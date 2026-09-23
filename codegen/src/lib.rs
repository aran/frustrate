//! frustrate-codegen: interface extraction, model checking, code emission.
//!
//! Usable as a library (e.g. from a build.rs for cargo-driven development) and
//! via the thin CLI in main.rs (for Bazel actions). Explicit inputs and
//! outputs only: sources in, generated Rust + Dart out.

pub mod check;
pub mod emit_dart;
pub mod emit_dart_fake;
pub mod emit_rust;
pub mod hash;
pub mod ir;
pub mod parse;

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// One bridge source file plus the module path through which its items are
/// reachable from the crate root (e.g. "crate::api").
#[derive(Debug, Clone)]
pub struct SourceSpec {
    pub path: PathBuf,
    pub module_path: String,
}

#[derive(Debug, Clone)]
pub struct GenerateConfig {
    pub crate_name: String,
    pub sources: Vec<SourceSpec>,
    /// Where to write the generated Rust glue (compiled into the bridge
    /// crate). One file for every platform: native-only members are
    /// cfg-gated rather than variant-split.
    pub rust_out: PathBuf,
    /// Where to write the public Dart bindings entry (a conditional export).
    /// The per-platform variants land next to it under `src/`:
    /// `src/<entry stem>.native.dart` and `src/<entry stem>.web.dart`.
    pub dart_out: PathBuf,
    /// Optional: where to write the finalized interface IR as JSON.
    pub ir_out: Option<PathBuf>,
    /// Optional: where to write the `#[bridge(no_block)]` claim census
    /// ([`emit_rust::claim_census`]) — what the two `no_block` drivers read to
    /// tell "nothing to prove" from "the proof is missing".
    ///
    /// Separate from [`GenerateConfig::ir_out`] because the IR JSON is the wire
    /// fingerprint and cannot carry a claim; see `claim_census`.
    pub claims_out: Option<PathBuf>,
}

/// Parse, check, and emit. Returns the finalized interface.
pub fn generate(config: &GenerateConfig) -> Result<ir::Interface> {
    declare_check_cfg();

    let mut parts = vec![];
    let mut warnings = vec![];
    for src in &config.sources {
        let text = std::fs::read_to_string(&src.path)
            .with_context(|| format!("reading {}", src.path.display()))?;
        let (part, found) = parse::parse_source_reporting(&text, &src.module_path)
            .with_context(|| format!("parsing {}", src.path.display()))?;
        // The parser is handed text; only here is the path known.
        warnings.extend(found.into_iter().map(|w| check::Warning {
            file: src.path.display().to_string(),
            ..w
        }));
        parts.push(part);
    }
    // Before `check`, deliberately. A source can carry both a forgotten
    // `#[bridge]` and a real check error, and warnings that evaporate whenever
    // an error fires are worth much less than warnings that do not.
    report(&warnings);

    let iface = parse::merge(&config.crate_name, parts);
    let iface = check::check(iface).map_err(|diags| {
        let rendered: Vec<String> = diags.iter().map(|d| d.to_string()).collect();
        anyhow::anyhow!("interface check failed:\n{}", rendered.join("\n"))
    })?;

    write_atomic(&config.rust_out, &emit_rust::emit(&iface))?;

    let entry_name = config
        .dart_out
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("dart_out has no file name: {}", config.dart_out.display()))?;
    let entry_stem = entry_name
        .strip_suffix(".dart")
        .with_context(|| format!("dart_out must end in .dart: {entry_name}"))?;
    let dart = emit_dart::emit(&iface, entry_stem);
    let src_dir = match config.dart_out.parent() {
        Some(p) => p.join("src"),
        None => PathBuf::from("src"),
    };
    write_atomic(&config.dart_out, &dart.entry)?;
    write_atomic(&src_dir.join(format!("{entry_stem}.native.dart")), &dart.native)?;
    write_atomic(&src_dir.join(format!("{entry_stem}.web.dart")), &dart.web)?;

    if let Some(ir_out) = &config.ir_out {
        write_atomic(ir_out, &serde_json::to_string_pretty(&iface)?)?;
    }
    if let Some(claims_out) = &config.claims_out {
        write_atomic(claims_out, &emit_rust::claim_census(&iface))?;
    }
    Ok(iface)
}

/// The one build-mode cfg the generated Rust names, declared to cargo so that
/// no consumer has to.
///
/// Every generated `#[no_mangle]` export is gated on `frustrate_block_check`
/// (`emit_rust`'s `BLOCK_CHECK_SUPPRESS`), and the check roots of a `no_block`
/// claim are gated on its negation. Nothing that ships sets it. Cargo passes
/// `--check-cfg` by default, so absent this line every gate is an
/// `unexpected_cfgs` warning — 25 of them in `tests/test_api` — in a file the
/// user cannot edit, curable only by a `[lints.rust]` stanza naming an internal
/// cfg they have never heard of. A build script speaks for exactly the crate
/// the generated file is compiled into, which is precisely the scope wanted.
///
/// `cargo::` (double colon) is the right prefix, not the older `cargo:`: cargo
/// has honoured `rustc-check-cfg` only since 1.80 and has understood `cargo::`
/// since 1.77, so no cargo reads one form and not the other.
const CHECK_CFG_DIRECTIVE: &str = "cargo::rustc-check-cfg=cfg(frustrate_block_check, frustrate_hot_patch)";

fn declare_check_cfg() {
    if in_build_script() {
        println!("{CHECK_CFG_DIRECTIVE}");
    }
}

/// Whether `generate` is running inside a cargo build script, as opposed to
/// inside the Bazel codegen action (or a test, or `cargo run`).
///
/// `CARGO_CFG_TARGET_ARCH` is the signal: cargo sets it for build scripts and
/// for nothing else. It decides which lines may go to stdout, where cargo reads
/// the build-script protocol and Bazel reads nothing at all.
fn in_build_script() -> bool {
    std::env::var_os("CARGO_CFG_TARGET_ARCH").is_some()
}

/// Surface non-fatal findings on the channel the caller's build system reads.
///
/// Errors need no such care — they come back through `Result` and every flow
/// prints a failure. A warning only exists if someone sees it, and the two
/// supported flows read different streams:
///
///   - **cargo.** `generate` runs inside a build script, whose stderr cargo
///     discards unless the script *fails* (or `-vv`). The one line cargo
///     displays is `cargo:warning=…` on stdout, emitted when
///     [`in_build_script`] says someone is listening.
///   - **Bazel.** The codegen CLI is a plain action; Bazel echoes an action's
///     stderr verbatim ("INFO: From …") and reads nothing on its stdout.
///
/// So: stderr always, plus the cargo line when we are inside a build script. A
/// diagnostic only half the users can see is the defect this warning exists to
/// fix, one level up.
///
/// One caveat inherent to both build systems: a cached action or an up-to-date
/// build script does not re-run, so the warning is not replayed — exactly like
/// a C compiler warning on an unchanged object file.
fn report(warnings: &[check::Warning]) {
    let in_build_script = in_build_script();
    for w in warnings {
        let rendered = w.to_string();
        eprintln!("{rendered}");
        if in_build_script {
            // The build-script protocol is line-oriented: an embedded newline
            // would make cargo read the remainder as a fresh directive.
            println!("cargo:warning={}", rendered.replace('\n', " "));
        }
    }
}

fn write_atomic(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content).with_context(|| format!("writing {}", path.display()))
}

/// Replace every dispatch id in generated code with `#`, for tests that assert
/// on a shape rather than on an id.
///
/// Ids are derived from each member's own wire facts (`hash::member_ids`), so a
/// test cannot spell one without restating the hash — and pinning the hash in
/// fifty places would make every one of them a test of `member_ids` by
/// accident. The ids themselves are tested where they are decided (`hash.rs`)
/// and where they reach Dart (`emit_dart`'s member-name table); here they are
/// noise.
///
/// Only the id position is masked. The numbers beside it — a request size hint,
/// an enum tag, a field count — are the substance of many of these assertions,
/// so a blanket digit substitution would hide exactly what they check.
#[cfg(test)]
pub(crate) fn mask_ids(code: &str) -> String {
    const PREFIXES: &[&str] = &[
        "call_",
        "spawn_",
        "actor_",
        "frustrate_check_block_",
        "callSync(",
        "callAsync(",
        "actorCall(",
        // An actor method dispatches through its host, which takes the id
        // first: `await _host.call(<id>, <hint>, (w) {`.
        "_host.call(",
        "case ",
        "fn_id: ",
    ];
    let mut out = String::with_capacity(code.len());
    let mut rest = code;
    'outer: while !rest.is_empty() {
        for p in PREFIXES {
            if let Some(tail) = rest.strip_prefix(p) {
                let digits = tail.len() - tail.trim_start_matches(|c: char| c.is_ascii_digit()).len();
                // A prefix not followed by digits is some other identifier
                // (`call_sync`, `case Foo`), which must pass through whole.
                if digits > 0 {
                    out.push_str(p);
                    out.push('#');
                    rest = &tail[digits..];
                    continue 'outer;
                }
            }
        }
        // Also mask a match arm's scrutinee: `<id> => call_#_name(...)`.
        if let Some(arrow) = arm_id(rest) {
            out.push('#');
            rest = &rest[arrow..];
            continue;
        }
        let mut chars = rest.chars();
        let c = chars.next().expect("non-empty");
        out.push(c);
        rest = chars.as_str();
    }
    out
}

/// The length of a leading `<digits>` that is a *dispatch* arm's id: digits,
/// ` => `, then a call into one of the per-member glue functions.
///
/// The arm body has to be checked. `<digits> => ` alone also spells a trait
/// carrier's tag and an enum's discriminant, which are wire values in their own
/// right and are exactly what several of these tests assert on.
#[cfg(test)]
fn arm_id(s: &str) -> Option<usize> {
    let digits = s.len() - s.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return None;
    }
    let body = s[digits..].strip_prefix(" => ")?;
    // The glue call may be wrapped — `Some(envelope::run(|| actor_7_poke(..)))`
    // — so look anywhere in the arm, but only within this line: the next arm's
    // tag must not vouch for this one.
    let arm = body.split('\n').next().unwrap_or(body);
    ["call_", "spawn_", "actor_"]
        .iter()
        .any(|p| {
            arm.match_indices(p).any(|(i, _)| {
                arm[i + p.len()..].starts_with(|c: char| c.is_ascii_digit())
            })
        })
        .then_some(digits)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    /// Names rustc knows on its own, so cargo's `--check-cfg` never questions
    /// them and no directive has to declare them.
    const WELL_KNOWN: &[&str] = &["feature", "target_family", "test"];

    fn emit_src(src: &str) -> String {
        let mut iface =
            crate::check::check(crate::parse::parse_source(src, "crate::api").unwrap()).unwrap();
        iface.crate_name = "test_api".into();
        crate::emit_rust::emit(&iface)
    }

    /// The cfg names inside every `<opener>…)` in `text`. Deliberately naive —
    /// the shapes it has to read are `#[cfg(name)]`, `#[cfg(not(name))]` and
    /// `#[cfg(name = "value")]`, nothing deeper.
    fn cfg_names_in(text: &str, opener: &str) -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        let mut rest = text;
        while let Some(i) = rest.find(opener) {
            rest = &rest[i + opener.len()..];
            let end = rest.find(')').unwrap_or(rest.len());
            let mut in_quotes = false;
            let unquoted: String = rest[..end]
                .chars()
                .map(|c| {
                    if c == '"' {
                        in_quotes = !in_quotes;
                    }
                    if in_quotes || c == '"' {
                        ' '
                    } else {
                        c
                    }
                })
                .collect();
            names.extend(
                unquoted
                    .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .filter(|t| !t.is_empty() && !matches!(*t, "not" | "any" | "all"))
                    .map(str::to_string),
            );
        }
        names
    }

    /// The directive has to declare every cfg the emitter invents, or cargo's
    /// default `--check-cfg` goes back to reporting each gate as unexpected in
    /// a file the consumer cannot edit. Adding a gate on a *new* private cfg
    /// without extending the directive fails here rather than downstream.
    #[test]
    fn the_check_cfg_directive_declares_every_private_cfg_the_emitter_gates_on() {
        // Both directions of the gate: a `no_block` claim emits check roots
        // under the cfg, and every export is emitted under its negation.
        let code = emit_src(
            r#"
            #[bridge(sync, no_block)] pub fn kept(x: i64) -> i64 { x }
            #[bridge] pub fn plain() -> String { String::new() }
            "#,
        );
        let private: BTreeSet<String> = cfg_names_in(&code, "#[cfg(")
            .into_iter()
            .filter(|n| !WELL_KNOWN.contains(&n.as_str()))
            .collect();
        assert_eq!(
            private,
            BTreeSet::from(["frustrate_block_check".to_string(), "frustrate_hot_patch".to_string()]),
            "{code}"
        );
        assert_eq!(
            private,
            cfg_names_in(super::CHECK_CFG_DIRECTIVE, "cfg("),
            "generated gates and the build-script declaration have drifted"
        );
    }
}
