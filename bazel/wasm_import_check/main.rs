//! Fail the build when a wasm module is one frustrate's web runtime cannot
//! instantiate.
//!
//! One rule over the module that ships: every import namespace is one the
//! runtime supplies itself, one the target declares in `host_imports` (`--host`),
//! or — when the wasm-bindgen post-pass ran — the namespace it wrote
//! (`--bindgen`, `./<stem>_bg.js`). A post-passed module must also have the
//! shape the CLI's thread transform produces, which [`run_post_pass`] checks.
//!
//! # Why this exists
//!
//! A bridge crate whose dependency graph contains wasm-bindgen — every crate
//! that reaches a browser API from Rust — compiles to a module carrying its
//! calls as imports in `__wbindgen_placeholder__`. Only the wasm-bindgen CLI
//! can rewrite those into the one real namespace its generated JS supplies, and
//! that pass runs only when `frustrate_wasm_module` is given the `bindgen`
//! attribute. Omit it and the module builds, links and passes every other
//! check: the unsatisfiable imports are as legal a wasm module as any
//! other, and nothing about the build says otherwise. The same holds for a
//! namespace of the app's own JS that the target forgot to declare.
//!
//! The failure lands at `WebAssembly.instantiate`, in a browser, as a
//! LinkError, for someone who opened the page. The fact was decidable the
//! moment rustc emitted the module.
//!
//! # How it decides
//!
//! **Every import namespace must be one the runtime will supply**: its own
//! (see [`SELF_SUPPLIED`]), a declared host namespace, which the app supplies
//! from its own JS module at load, or the post-pass's, which wasm-bindgen's
//! generated JS supplies. A namespace is wasm-bindgen's exactly when it has the
//! form the post-pass writes ([`is_bindgen_namespace`]), so `host_imports` may
//! not declare one. The check reads the module after every pass, so the
//! module it examines *is* the module that ships. There is no "which module was
//! examined" caveat of the kind `bazel/wasm_std_check` carries.
//!
//! Import-keyed for the same reason that check is: `frustrate_wasm_module`
//! documents that any `platform()` works, so a rule that compared platform
//! labels would silently not apply to a consumer's own. It is also the rule the
//! runtime itself applies — `hostImports` in
//! `runtime/dart/lib/src/js/frustrate.js` decides the same question at
//! instantiation.
//!
//! # What it claims
//!
//! Unlike the std-facility check, this is a decision and not a tripwire. The
//! namespace string is written literally into the import section by rustc from
//! the crate's own `#[link(wasm_import_module = …)]`; there is no optimization
//! that can inline it away and no mangling scheme that can spell it
//! differently, which is why hand-built fixtures in this file's tests are
//! adequate coverage where the std check needs a real rustc module to prove its
//! symbols still appear. For the same reason the section cannot be stripped:
//! deleting an import changes what the module *is*, so there is no
//! missing-metadata case to refuse to judge — only an unparseable one, which is
//! an error.
//!
//! There is deliberately no way to switch this off. A module that trips it
//! cannot be instantiated by frustrate's runtime at all, so no valid target
//! needs the exemption — the same stance `frustrate_no_block_check` takes, and for
//! the same reason.

use std::collections::BTreeMap;
use std::process::ExitCode;

use wasmparser::{ExternalKind, Parser, Payload, TypeRef};

/// The import namespaces frustrate's web runtime supplies itself.
///
/// The authority is the runtime, `selfSupplied` in
/// `runtime/dart/lib/src/js/frustrate.js`. `--print-self-supplied` writes this
/// list, and `host_imports_web_test.dart` requires the runtime to claim every
/// namespace on it.
///
/// `frustrate` is the bridge ABI plus the browser std's host calls, `env` is the
/// shared memory and `__stack_chk_fail`, `wasi_snapshot_preview1` is the
/// preview1 host the wasi platform's std calls.
const SELF_SUPPLIED: &[&str] = &["frustrate", "env", "wasi_snapshot_preview1"];

/// What the runtime puts in `env`, which unlike the other two namespaces is a
/// *closed* set — and the reason this is checked by field where they are not.
///
/// `env` is where the linker parks a symbol nothing defined. A crate whose only
/// item is a `#[no_mangle]` definition is the way that happens by accident:
/// rustc links an `--extern` crate only when something uses it, so a dependency
/// edge alone can leave the definition out, and the caller's `extern` block
/// becomes `env.<symbol>` — an import no runtime supplies, in a namespace the
/// namespace check waves through.
///
/// The other two namespaces cannot be checked this way and should not be: a
/// module's `frustrate` imports are whichever subset of the ABI and the
/// browser std's host calls its std happens to need, and `wasi_snapshot_preview1`
/// is preview1's whole surface. `env` is two names, both from the runtime —
/// `memory` for a threaded module, `__stack_chk_fail` from toolchains_llvm's
/// unremovable `-fstack-protector`. The authority is `runtime_web.dart` and
/// `frustrate.js`, at the three instantiation sites.
const ENV_SUPPLIED: &[&str] = &["memory", "__stack_chk_fail"];

/// wasm-bindgen names its pre-pass namespaces `__wbindgen_placeholder__` and
/// `__wbindgen_externref_xform__`. Matching the shared prefix rather than the
/// two exact strings keeps a future third one on the tailored message instead
/// of the generic one; nothing else in a bridge module's import list starts
/// this way.
const BINDGEN_PREFIX: &str = "__wbindgen";

/// Whether `ns` is wasm-bindgen's namespace: `--target bundler` writes the one
/// namespace its generated JS supplies as `./<out-name>_bg.js`. The runtime's
/// `isBindgenNamespace` in `runtime/dart/lib/src/js/frustrate.js` is the same
/// predicate, and gives every such namespace to `bindgenGlueUrl`.
fn is_bindgen_namespace(ns: &str) -> bool {
    ns.starts_with("./") && ns.ends_with("_bg.js")
}

/// The sections of a core module, each item decoded. A module that does not
/// decode completely is an error: a partial answer would be a pass for a
/// module this never finished examining.
fn payloads(d: &[u8]) -> Result<Vec<Payload<'_>>, String> {
    if !Parser::is_core_wasm(d) {
        return Err("not a wasm module".into());
    }
    Parser::new(0)
        .parse_all(d)
        .collect::<wasmparser::Result<_>>()
        .map_err(|e| e.to_string())
}

/// One entry of the import section: its namespace, and — when it is a memory —
/// the limits it declares.
struct Import {
    module: String,
    field: String,
    /// `(minimum pages, maximum pages, shared)` iff this import is a memory.
    memory: Option<(u64, Option<u64>, bool)>,
}

/// Every entry of the import section, in order.
fn imports(d: &[u8]) -> Result<Vec<Import>, String> {
    let mut out = Vec::new();
    for p in payloads(d)? {
        if let Payload::ImportSection(r) = p {
            for i in r.into_imports() {
                let i = i.map_err(|e| e.to_string())?;
                out.push(Import {
                    module: i.module.to_string(),
                    field: i.name.to_string(),
                    memory: match i.ty {
                        TypeRef::Memory(m) => Some((m.initial, m.maximum, m.shared)),
                        _ => None,
                    },
                });
            }
        }
    }
    Ok(out)
}

/// `(name, kind)` for every entry of the export section, in order.
fn exports(d: &[u8]) -> Result<Vec<(String, ExternalKind)>, String> {
    let mut out = Vec::new();
    for p in payloads(d)? {
        if let Payload::ExportSection(r) = p {
            for e in r {
                let e = e.map_err(|e| e.to_string())?;
                out.push((e.name.to_string(), e.kind));
            }
        }
    }
    Ok(out)
}

/// Whether the module has a start function.
fn has_start(d: &[u8]) -> Result<bool, String> {
    Ok(payloads(d)?
        .iter()
        .any(|p| matches!(p, Payload::StartSection { .. })))
}

/// A module importing something from `env` that the runtime does not put there.
///
/// Its own error rather than a line folded into the namespace one, because the
/// cause and the fix are different: the namespace check catches a *dependency
/// the runtime cannot serve*, and this catches a *definition that did not make
/// it into the link*. Same outcome in a browser — a LinkError before any of the
/// module's code runs — and the same reason to catch it here instead.
fn stray_env_error(label: &str, stray: &[String]) -> Result<(), String> {
    if stray.is_empty() {
        return Ok(());
    }
    let mut msg = format!(
        "frustrate_wasm_module: {label} imports symbols from `env` that nothing \
         will supply:\n\n"
    );
    for field in stray {
        msg.push_str(&format!("  env.{field}\n"));
    }
    msg.push_str(&format!(
        "\nThe runtime puts {} in `env` and nothing else, so these are \
         undefined symbols the linker parked there rather than imports anyone \
         declared. `WebAssembly.instantiate` rejects the module with a \
         LinkError, in the browser, before any of its code runs.\n\n\
         The usual cause is a crate whose only item is a `#[no_mangle]` \
         definition: rustc links an `--extern` crate only when something uses \
         it, so depending on it is not enough and the definition is dropped. \
         Name it once in the crate that needs it —\n\n\
         \x20   use the_crate as _;\n\n\
         `frustrate-getrandom` (//ext/getrandom) is the worked example, and \
         says so in its own docs. Otherwise: something declared an `extern` \
         block for a symbol no crate in this module defines.",
        ENV_SUPPLIED
            .iter()
            .map(|s| format!("`{s}`"))
            .collect::<Vec<_>>()
            .join(" and ")
    ));
    Err(msg)
}

/// The one rule: every import namespace is the runtime's own, one the target
/// declares in `host_imports` (`hosts`), or — when the wasm-bindgen post-pass
/// ran — the namespace it wrote (`bindgen`), whose module must then also have
/// the shape [`run_post_pass`] checks.
fn run(path: &str, label: &str, hosts: &[String], bindgen: Option<&str>) -> Result<(), String> {
    check_namespaces(path, label, hosts, bindgen)?;
    match bindgen {
        Some(_) => run_post_pass(path, label),
        None => Ok(()),
    }
}

fn check_namespaces(
    path: &str,
    label: &str,
    hosts: &[String],
    bindgen: Option<&str>,
) -> Result<(), String> {
    if let Some(ns) = hosts.iter().find(|ns| SELF_SUPPLIED.contains(&ns.as_str())) {
        return Err(format!(
            "frustrate_wasm_module: {label} declares host_imports = [\"{ns}\"], \
             a namespace frustrate's web runtime supplies itself. Drop it."
        ));
    }
    if let Some(ns) = hosts.iter().find(|ns| is_bindgen_namespace(ns)) {
        return Err(format!(
            "frustrate_wasm_module: {label} declares host_imports = [\"{ns}\"], \
             which has the form of wasm-bindgen's namespace (`./<name>_bg.js`). \
             Only wasm-bindgen's generated JS supplies such a namespace: run its \
             post-pass with `bindgen = ...`, and name the JS in \
             `FrustrateWeb.init(bindgenGlueUrl:)`."
        ));
    }
    if let Some(ns) = bindgen.filter(|ns| !is_bindgen_namespace(ns)) {
        return Err(format!(
            "wasm_import_check: --bindgen {ns} is not of the form `./<name>_bg.js`"
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let entries = imports(&bytes).map_err(|e| {
        format!(
            "frustrate_wasm_module: cannot read the import section of {label}: {e}.\n\
             \n\
             The check that every import namespace is one the runtime supplies \
             cannot run on a module it could not decode, and a partial answer would \
             be a pass for a module it never finished examining — so it errors \
             instead."
        )
    })?;

    let mut foreign: BTreeMap<String, usize> = BTreeMap::new();
    let mut stray_env: Vec<String> = Vec::new();
    for entry in entries {
        if hosts.contains(&entry.module) || bindgen == Some(entry.module.as_str()) {
            continue;
        }
        if !SELF_SUPPLIED.contains(&entry.module.as_str()) {
            *foreign.entry(entry.module).or_insert(0) += 1;
        } else if entry.module == "env" && !ENV_SUPPLIED.contains(&entry.field.as_str()) {
            stray_env.push(entry.field);
        }
    }
    if foreign.is_empty() {
        // Reported only when the namespaces are otherwise clean: a module with
        // a foreign namespace has a bigger problem, and the message below would
        // be a second diagnosis of the same missing dependency.
        return stray_env_error(label, &stray_env);
    }

    let mut msg =
        format!("frustrate_wasm_module: {label} imports namespaces nothing will supply:\n\n");
    for (ns, n) in &foreign {
        let plural = if *n == 1 { "import" } else { "imports" };
        msg.push_str(&format!("  {ns} ({n} {plural})\n"));
    }
    msg.push_str(
        "\nA module whose imports are not all satisfied does not instantiate: \
         `WebAssembly.instantiate` rejects it with a LinkError, in the browser, \
         before any of its code runs.\n\n",
    );

    if foreign.keys().any(|ns| ns.starts_with(BINDGEN_PREFIX)) {
        msg.push_str(
            "Those are wasm-bindgen's placeholders. rustc leaves every \
             `#[wasm_bindgen]` call as an import in them, and only the wasm-bindgen \
             CLI rewrites them into the one real namespace its generated JS \
             supplies. Run that post-pass by naming the CLI:\n\
             \n\
             \x20   frustrate_wasm_module(\n\
             \x20       name = \"...\",\n\
             \x20       crate = \"...\",\n\
             \x20       bindgen = \"@your_crates//:wasm-bindgen-cli__wasm-bindgen\",\n\
             \x20   )\n\
             \n\
             frustrate ships no CLI, because the version must equal the \
             wasm-bindgen crate version in *your* lockfile exactly — it is a \
             property of your crate graph, not of frustrate. Build it from source \
             with a crate_universe hub declaring \
             `gen_binaries = [\"wasm-bindgen\"]`; e2e/iroh_demo is the worked \
             example, hub and all.\n\
             \n\
             The rule then exposes the generated JS in the `bindgen_js` output \
             group. Serve it beside the .wasm and name its URL in \
             `FrustrateWeb.init(bindgenGlueUrl:)`, or the module still will not \
             instantiate — the runtime says so at that point.",
        );
    } else {
        msg.push_str(&format!(
            "frustrate's web runtime supplies {}. Any other namespace comes from \
             a JS module the app serves: if one of yours implements it, declare \
             it on this target —\n\
             \n\
             \x20   host_imports = [{}],\n\
             \n\
             — and name that module's URL for it in \
             `FrustrateWeb.init(hostImports: …)`. Otherwise nothing can satisfy \
             it: drop whatever declares it.",
            SELF_SUPPLIED
                .iter()
                .map(|s| format!("`{s}`"))
                .collect::<Vec<_>>()
                .join(", "),
            foreign
                .keys()
                .map(|ns| format!("\"{ns}\""))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        if !hosts.is_empty() {
            msg.push_str(&format!(
                "\n\nThis target declares host_imports = [{}].",
                hosts
                    .iter()
                    .map(|ns| format!("\"{ns}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if let Some(ns) = bindgen {
            msg.push_str(&format!(
                "\n\nThe wasm-bindgen post-pass wrote `{ns}`, which its generated \
                 JS supplies. Any other namespace it leaves (a `snippets/` module \
                 from `#[wasm_bindgen(module = …)]` or `inline_js`) is not served \
                 by frustrate."
            ));
        }
    }
    Err(msg)
}

/// Pages of the shared memory frustrate's web runtime creates for a threaded
/// module. `_sharedMemoryInitialPages` and `_sharedMemoryMaxPages` in
/// `runtime/dart/lib/src/runtime_web.dart` are the authority; these must equal
/// them, or this check passes a module the runtime then refuses.
const HOST_MEMORY_INITIAL_PAGES: u64 = 256;
const HOST_MEMORY_MAX_PAGES: u64 = 16384;

/// The wasm-bindgen the shape below was read off. Named in every failure, since
/// a failure here is almost always a CLI whose thread transform changed — and
/// the version is the app's own pin, not frustrate's.
const MEASURED_BINDGEN: &str = "0.2.126";

/// The post-pass contract: what a *threaded* module must look like after the
/// wasm-bindgen CLI has rewritten it, for frustrate's runtime to drive it.
///
/// # Why this exists at all
///
/// `check_namespaces` asks whether a module imports only namespaces the
/// runtime supplies. This is the rest of the same question — can frustrate's
/// runtime instantiate this module — for a module the post-pass rewrote, where
/// the answer is also about shape.
///
/// The CLI has a thread transform that fires, with no way to switch it off,
/// whenever the module's memory is shared — which is exactly what `+atomics`
/// emits. It rewrites the module deeply: the memory import moves out of `env`
/// and into the generated sidecar's namespace, `__wasm_init_tls`, `__tls_size`
/// and `__tls_align` are deleted, thread bootstrap becomes an exported
/// `__wbindgen_start`, and the start section is unstarted into it. frustrate
/// cedes the bootstrap and keeps memory identity: it supplies its own shared
/// memory under the sidecar's namespace, which works *because* the module
/// re-exports the memory it imports and every generated shim reads
/// `wasm.memory.buffer` off the instance.
///
/// Every clause below is one of the facts that argument rests on, and the CLI's
/// version is the *app's* pin rather than frustrate's — so the shape can move
/// under a build frustrate never chose. Checked here, the drift is a build
/// failure naming the invariant; unchecked, it is a browser LinkError, a
/// half-bootstrapped thread, or (worst) two Workers on two memories that share
/// nothing.
///
/// # What it does not check
///
/// Nothing about a single-threaded module, which imports no memory at all.
/// Import-keyed, like every other gate here, so a consumer's own `platform()`
/// is covered without this rule knowing any platform label.
fn run_post_pass(path: &str, label: &str) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let cannot_read = |what: &str, e: String| {
        format!(
            "frustrate_wasm_module: cannot read the {what} of {label}: {e}.\n\
             \n\
             This is the post-pass output the app will serve. A partial answer \
             would be a pass for a module this never finished examining, so it \
             errors instead."
        )
    };
    let imports = imports(&bytes).map_err(|e| cannot_read("import section", e))?;
    let memories: Vec<&Import> = imports.iter().filter(|i| i.memory.is_some()).collect();
    if memories.is_empty() {
        return Ok(()); // Single-threaded: it exports its own memory. Not this check's question.
    }
    let has_start = has_start(&bytes).map_err(|e| cannot_read("section list", e))?;
    let exports = exports(&bytes).map_err(|e| cannot_read("export section", e))?;
    let exported = |n: &str, kind: ExternalKind| exports.iter().any(|(e, k)| e == n && *k == kind);

    let fail = |invariant: &str, detail: String| -> Result<(), String> {
        Err(format!(
            "frustrate_wasm_module: {label} is a threaded module whose \
             wasm-bindgen post-pass did not leave the shape frustrate's web \
             runtime drives.\n\
             \n\
             {invariant}\n\
             {detail}\n\
             \n\
             frustrate cedes per-thread bootstrap to wasm-bindgen's thread \
             transform and keeps memory identity for itself. The shape above \
             was read off wasm-bindgen {MEASURED_BINDGEN}; the CLI version is your \
             lockfile's pin, not frustrate's, so a newer transform is the \
             usual reason this fires."
        ))
    };

    if memories.len() > 1 {
        return fail(
            "It imports more than one memory, and the runtime supplies one.",
            memories
                .iter()
                .map(|i| format!("  {}.{}", i.module, i.field))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    let m = memories[0];
    let (min, max, shared) = m.memory.unwrap();
    if !shared {
        return fail(
            "Its imported memory is not shared, so the transform never ran \
             over it and its threads would not share anything.",
            format!("  {}.{} declares minimum {min} pages", m.module, m.field),
        );
    }
    if min > HOST_MEMORY_INITIAL_PAGES || max.is_none_or(|x| x < HOST_MEMORY_MAX_PAGES) {
        return fail(
            "The shared memory frustrate creates does not satisfy its declared \
             limits, so instantiation would fail with a LinkError in the browser.",
            format!(
                "  {}.{} declares minimum {min} pages, maximum {}\n  \
                 the runtime creates initial {HOST_MEMORY_INITIAL_PAGES}, \
                 maximum {HOST_MEMORY_MAX_PAGES} (a shared memory must be at \
                 least the declared minimum and no wider than the declared \
                 maximum)\n  \
                 the maximum comes from `--max-memory` in \
                 toolchain/custom_std/rustflags.txt",
                m.module,
                m.field,
                max.map_or("none".to_string(), |x| x.to_string()),
            ),
        );
    }
    if !exported("memory", ExternalKind::Memory) {
        return fail(
            "It does not re-export the memory it imports — the single fact the \
             whole design rests on.",
            "  Every generated shim reads `wasm.memory.buffer` off the \
             instance, which is how supplying frustrate's memory under the \
             sidecar's namespace reaches them at all. Without the re-export \
             the shims would read the sidecar's own per-realm memory and each \
             Worker would silently address a different one."
                .to_string(),
        );
    }
    if !exported("__wbindgen_start", ExternalKind::Func) {
        return fail(
            "It exports no `__wbindgen_start`, which is the transform's whole \
             per-thread bootstrap.",
            "  A pool worker calls it to get its stack and its TLS block. \
             Without it the worker has neither and runs on the linked stack \
             pointer, which belongs to the main thread."
                .to_string(),
        );
    }
    for n in ["__wasm_init_tls", "__tls_size", "__tls_align"] {
        if exports.iter().any(|(e, _)| e == n) {
            return fail(
                "It exports both bootstraps, so nothing decides which one runs.",
                format!(
                    "  `{n}` survived the transform, which deletes it. The \
                     pool pump reads exactly that export to tell a post-passed \
                     module from a hand-bootstrapped one, and would take the \
                     hand path on a module that has already bootstrapped \
                     itself."
                ),
            );
        }
    }
    if has_start {
        return fail(
            "It still has a start section, which runs at instantiation — \
             before the sidecar has been handed the instance.",
            "  The transform unstarts the module into `__wbindgen_start` so \
             the bootstrap runs after `__wbg_set_wasm`. A start section left \
             in place would run wasm-bindgen shims against an undefined \
             instance."
                .to_string(),
        );
    }
    Ok(())
}

fn main() -> ExitCode {
    let usage = || {
        eprintln!(
            "usage: wasm_import_check [--host <namespace>]... [--bindgen <namespace>] \
             <module.wasm> <label> <marker-out>\n\
             \x20      wasm_import_check --print-self-supplied <json-out>"
        );
        ExitCode::FAILURE
    };
    if std::env::args().nth(1).as_deref() == Some("--print-self-supplied") {
        let rest: Vec<String> = std::env::args().skip(2).collect();
        let [out] = rest.as_slice() else {
            return usage();
        };
        let json = format!(
            "[{}]\n",
            SELF_SUPPLIED
                .iter()
                .map(|ns| format!("\"{ns}\""))
                .collect::<Vec<_>>()
                .join(", ")
        );
        return match std::fs::write(out, json) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("wasm_import_check: writing {out}: {e}");
                ExitCode::FAILURE
            }
        };
    }
    let mut bindgen: Option<String> = None;
    let mut hosts: Vec<String> = Vec::new();
    let mut positional: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bindgen" | "--host" => match args.next() {
                Some(ns) if arg == "--host" => hosts.push(ns),
                Some(ns) => bindgen = Some(ns),
                None => return usage(),
            },
            _ => positional.push(arg),
        }
    }
    let [path, label, marker] = positional.as_slice() else {
        return usage();
    };
    finish(run(path, label, &hosts, bindgen.as_deref()), marker)
}

fn finish(verdict: Result<(), String>, marker: &str) -> ExitCode {
    match verdict {
        Ok(()) => {
            // The marker is the action's declared output; writing it is what
            // lets Bazel cache a pass. Nothing ever reads its contents.
            if let Err(e) = std::fs::write(marker, b"ok\n") {
                eprintln!("wasm_import_check: writing {marker}: {e}");
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

    /// The four import kinds, so a fixture can put a namespace *after* one of
    /// each and prove the decoder still reaches it.
    enum Kind {
        Func,
        Table,
        Memory,
        Global,
    }

    fn uleb_out(v: usize, out: &mut Vec<u8>) {
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
        uleb_out(text.len(), out);
        out.extend_from_slice(text.as_bytes());
    }

    /// A module with one import section. `imports` is `(namespace, field,
    /// kind)`.
    fn module(imports: &[(&str, &str, Kind)]) -> Vec<u8> {
        let mut m = b"\0asm\x01\0\0\0".to_vec();
        let mut imp = Vec::new();
        uleb_out(imports.len(), &mut imp);
        for (ns, field, kind) in imports {
            s(ns, &mut imp);
            s(field, &mut imp);
            match kind {
                Kind::Func => {
                    imp.push(0);
                    uleb_out(0, &mut imp); // typeidx
                }
                Kind::Table => {
                    imp.push(1);
                    imp.push(0x70); // funcref
                    imp.push(0x01); // has max
                    uleb_out(1, &mut imp);
                    uleb_out(2, &mut imp);
                }
                Kind::Memory => {
                    imp.push(2);
                    imp.push(0x00); // no max
                    uleb_out(17, &mut imp);
                }
                Kind::Global => {
                    imp.push(3);
                    imp.push(0x7f); // i32
                    imp.push(0x00); // immutable
                }
            }
        }
        m.push(2);
        uleb_out(imp.len(), &mut m);
        m.extend_from_slice(&imp);
        m
    }

    /// Writes `bytes` to a temp file and runs the check over it, so the test
    /// exercises the same entry point the action does.
    fn check(bytes: &[u8]) -> Result<(), String> {
        check_with_hosts(bytes, &[])
    }

    /// [`check`] for a target declaring `host_imports = hosts`.
    fn check_with_hosts(bytes: &[u8], hosts: &[&str]) -> Result<(), String> {
        check_full(bytes, hosts, None)
    }

    /// [`check`] for a target declaring `host_imports = hosts`, whose
    /// wasm-bindgen post-pass, if any, wrote the namespace `bindgen`.
    fn check_full(bytes: &[u8], hosts: &[&str], bindgen: Option<&str>) -> Result<(), String> {
        let hosts: Vec<String> = hosts.iter().map(|s| s.to_string()).collect();
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "wasm_import_check_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("m.wasm");
        std::fs::write(&path, bytes).unwrap();
        let r = run(path.to_str().unwrap(), "//pkg:mod.wasm", &hosts, bindgen);
        std::fs::remove_dir_all(&dir).ok();
        r
    }

    #[test]
    fn a_module_with_no_import_section_passes() {
        assert_eq!(check(b"\0asm\x01\0\0\0"), Ok(()));
    }

    #[test]
    fn an_empty_import_section_passes() {
        assert_eq!(check(&module(&[])), Ok(()));
    }

    #[test]
    fn every_self_supplied_namespace_passes() {
        let m = module(&[
            ("frustrate", "post", Kind::Func),
            ("env", "memory", Kind::Memory),
            ("wasi_snapshot_preview1", "fd_write", Kind::Func),
        ]);
        assert_eq!(check(&m), Ok(()));
    }

    #[test]
    fn the_wasm_bindgen_placeholder_fails_with_the_bindgen_fix() {
        let m = module(&[
            ("frustrate", "post", Kind::Func),
            ("__wbindgen_placeholder__", "__wbg_new_1", Kind::Func),
            ("__wbindgen_placeholder__", "__wbg_new_2", Kind::Func),
            (
                "__wbindgen_externref_xform__",
                "__wbindgen_copy",
                Kind::Func,
            ),
        ]);
        let e = check(&m).unwrap_err();
        assert!(e.contains("//pkg:mod.wasm"), "{e}");
        assert!(e.contains("__wbindgen_placeholder__ (2 imports)"), "{e}");
        assert!(e.contains("__wbindgen_externref_xform__ (1 import)"), "{e}");
        assert!(e.contains("bindgen = "), "{e}");
        // The namespace it *can* satisfy is not reported as a problem.
        assert!(!e.contains("frustrate ("), "{e}");
    }

    #[test]
    fn an_unrecognised_namespace_fails_with_the_generic_message() {
        let m = module(&[("./demo_bg.js", "__wbg_new", Kind::Func)]);
        let e = check(&m).unwrap_err();
        assert!(e.contains("./demo_bg.js (1 import)"), "{e}");
        // Not the wasm-bindgen paragraph: this one names what the runtime does
        // supply instead.
        assert!(!e.contains("bindgen = "), "{e}");
        assert!(e.contains("`wasi_snapshot_preview1`"), "{e}");
    }

    #[test]
    fn a_declared_host_namespace_passes() {
        let m = module(&[
            ("frustrate", "post", Kind::Func),
            ("example_opfs", "read", Kind::Func),
            ("example_opfs", "write", Kind::Func),
        ]);
        assert_eq!(check_with_hosts(&m, &["example_opfs"]), Ok(()));
    }

    #[test]
    fn an_undeclared_namespace_fails_beside_a_declared_one() {
        let m = module(&[
            ("example_opfs", "read", Kind::Func),
            ("other_js", "f", Kind::Func),
        ]);
        let e = check_with_hosts(&m, &["example_opfs"]).unwrap_err();
        assert!(e.contains("other_js (1 import)"), "{e}");
        assert!(e.contains("host_imports = [\"other_js\"]"), "{e}");
        assert!(
            e.contains("declares host_imports = [\"example_opfs\"]"),
            "{e}"
        );
        assert!(!e.contains("example_opfs (1 import)"), "{e}");
    }

    #[test]
    fn declaring_a_host_namespace_does_not_admit_the_bindgen_placeholder() {
        let m = module(&[("__wbindgen_placeholder__", "__wbg_new", Kind::Func)]);
        let e = check_with_hosts(&m, &["example_opfs"]).unwrap_err();
        assert!(e.contains("bindgen = "), "{e}");
    }

    #[test]
    fn the_post_pass_namespace_passes_beside_a_declared_one() {
        let m = module(&[
            ("./demo_bg.js", "__wbg_new", Kind::Func),
            ("example_opfs", "read", Kind::Func),
        ]);
        assert_eq!(
            check_full(&m, &["example_opfs"], Some("./demo_bg.js")),
            Ok(())
        );
    }

    #[test]
    fn a_post_pass_snippet_namespace_fails() {
        let m = module(&[
            ("./demo_bg.js", "__wbg_new", Kind::Func),
            ("./snippets/x-1a2b/inline0.js", "f", Kind::Func),
        ]);
        let e = check_full(&m, &[], Some("./demo_bg.js")).unwrap_err();
        assert!(e.contains("./snippets/x-1a2b/inline0.js (1 import)"), "{e}");
        assert!(!e.contains("./demo_bg.js ("), "{e}");
    }

    #[test]
    fn declaring_a_bindgen_namespace_as_a_host_namespace_fails() {
        let m = module(&[("./demo_bg.js", "__wbg_new", Kind::Func)]);
        let e = check_with_hosts(&m, &["./demo_bg.js"]).unwrap_err();
        assert!(e.contains("host_imports = [\"./demo_bg.js\"]"), "{e}");
        assert!(e.contains("bindgenGlueUrl"), "{e}");
    }

    #[test]
    fn the_bindgen_namespace_form_is_the_post_pass_output_name() {
        assert!(is_bindgen_namespace("./demo_bg.js"));
        assert!(!is_bindgen_namespace("demo_bg.js"));
        assert!(!is_bindgen_namespace("./snippets/x-1a2b/inline0.js"));
        assert!(!is_bindgen_namespace("__wbindgen_placeholder__"));
    }

    #[test]
    fn declaring_a_namespace_the_runtime_supplies_fails() {
        let e = check_with_hosts(&module(&[]), &["env"]).unwrap_err();
        assert!(e.contains("host_imports = [\"env\"]"), "{e}");
    }

    #[test]
    fn a_stray_env_symbol_fails_and_names_the_use() {
        // The shape a dropped `#[no_mangle]` definition leaves behind: the
        // linker parks the undefined symbol in `env`, whose namespace the
        // check above waves through. Reached in practice by depending on a
        // crate whose only item is that definition without naming it (see
        // //ext/getrandom), where the outcome is otherwise a LinkError in a
        // browser rather than anything a build says.
        let m = module(&[
            ("env", "__stack_chk_fail", Kind::Func),
            ("env", "__getrandom_v03_custom", Kind::Func),
        ]);
        let e = check(&m).unwrap_err();
        assert!(e.contains("//pkg:mod.wasm"), "{e}");
        assert!(e.contains("env.__getrandom_v03_custom"), "{e}");
        assert!(e.contains("use the_crate as _;"), "{e}");
        // The one it *can* satisfy is not reported as a problem.
        assert!(!e.contains("env.__stack_chk_fail"), "{e}");
    }

    #[test]
    fn the_env_symbols_the_runtime_supplies_pass() {
        // The negative half. Without it, a field check that rejected
        // everything — or that never ran — would pass the test above.
        let m = module(&[
            ("env", "memory", Kind::Memory),
            ("env", "__stack_chk_fail", Kind::Func),
        ]);
        assert_eq!(check(&m), Ok(()));
    }

    #[test]
    fn a_stray_env_symbol_is_not_reported_beside_a_foreign_namespace() {
        // One diagnosis at a time: a module with a foreign namespace is missing
        // a dependency, and the stray symbol is that same absence seen twice.
        let m = module(&[
            ("__wbindgen_placeholder__", "__wbg_new_1", Kind::Func),
            ("env", "some_undefined_symbol", Kind::Func),
        ]);
        let e = check(&m).unwrap_err();
        assert!(e.contains("__wbindgen_placeholder__"), "{e}");
        assert!(!e.contains("env.some_undefined_symbol"), "{e}");
    }

    #[test]
    fn a_namespace_after_each_import_kind_is_still_seen() {
        // Each non-func kind has its own size encoding; getting one wrong
        // desynchronises the decoder and loses everything after it.
        for kind in [Kind::Table, Kind::Memory, Kind::Global] {
            let m = module(&[
                ("env", "first", kind),
                ("__wbindgen_placeholder__", "__wbg_new", Kind::Func),
            ]);
            let e = check(&m).unwrap_err();
            assert!(e.contains("__wbindgen_placeholder__ (1 import)"), "{e}");
        }
    }

    #[test]
    fn a_file_that_is_not_a_module_is_an_error_not_a_pass() {
        let e = check(b"not wasm at all").unwrap_err();
        assert!(e.contains("cannot read the import section"), "{e}");
        assert!(e.contains("not a wasm module"), "{e}");
    }

    #[test]
    fn a_truncated_import_section_is_an_error_not_a_pass() {
        let mut m = module(&[("__wbindgen_placeholder__", "__wbg_new", Kind::Func)]);
        m.truncate(m.len() - 4);
        let e = check(&m).unwrap_err();
        assert!(e.contains("cannot read the import section"), "{e}");
    }

    // ------------------------------------------------ the post-pass contract --

    /// A module shaped like wasm-bindgen's post-pass output: one imported
    /// memory with `limits`, plus an export section built from `exports`.
    ///
    /// `limits` is `(min, max, shared)`. Hand-built rather than taken from a
    /// real artifact for the reason the header gives: these sections are
    /// written literally by the CLI's own encoder, so there is no optimization
    /// that can spell them differently.
    fn post_pass_module(
        limits: Option<(u64, Option<u64>, bool)>,
        exports: &[(&str, u8)],
        start: bool,
    ) -> Vec<u8> {
        let mut m = b"\0asm\x01\0\0\0".to_vec();
        if let Some((min, max, shared)) = limits {
            let mut imp = Vec::new();
            uleb_out(1, &mut imp);
            s("./ir_bg.js", &mut imp);
            s("memory", &mut imp);
            imp.push(2);
            imp.push(if max.is_some() { 0x01 } else { 0 } | if shared { 0x02 } else { 0 });
            uleb_out(min as usize, &mut imp);
            if let Some(max) = max {
                uleb_out(max as usize, &mut imp);
            }
            m.push(2);
            uleb_out(imp.len(), &mut m);
            m.extend_from_slice(&imp);
        }
        let mut exp = Vec::new();
        uleb_out(exports.len(), &mut exp);
        for (n, kind) in exports {
            s(n, &mut exp);
            exp.push(*kind);
            uleb_out(0, &mut exp);
        }
        m.push(7);
        uleb_out(exp.len(), &mut m);
        m.extend_from_slice(&exp);
        if start {
            m.push(8);
            uleb_out(1, &mut m);
            uleb_out(0, &mut m);
        }
        m
    }

    /// The exports a conforming post-passed threaded module carries.
    const CONFORMING: &[(&str, u8)] = &[
        ("memory", 2),
        ("__wbindgen_start", 0),
        ("__tls_base", 3),
        ("__stack_alloc", 3),
        ("frustrate_worker_entry", 0),
    ];

    fn check_post_pass(bytes: &[u8]) -> Result<(), String> {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "wasm_post_pass_check_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("m.wasm");
        std::fs::write(&path, bytes).unwrap();
        let r = run_post_pass(path.to_str().unwrap(), "//pkg:mod.wasm");
        std::fs::remove_dir_all(&dir).ok();
        r
    }

    #[test]
    fn the_shape_wasm_bindgen_0_2_126_produces_passes() {
        let m = post_pass_module(
            Some((34, Some(HOST_MEMORY_MAX_PAGES), true)),
            CONFORMING,
            false,
        );
        assert_eq!(check_post_pass(&m), Ok(()));
    }

    #[test]
    fn a_module_importing_no_memory_is_not_this_checks_question() {
        // Single-threaded: it exports its own memory, and the transform never
        // ran. Everything else below would be false of it.
        let m = post_pass_module(None, &[("memory", 2), ("__wbindgen_start", 0)], true);
        assert_eq!(check_post_pass(&m), Ok(()));
    }

    #[test]
    fn an_unshared_imported_memory_fails() {
        let m = post_pass_module(
            Some((34, Some(HOST_MEMORY_MAX_PAGES), false)),
            CONFORMING,
            false,
        );
        let e = check_post_pass(&m).unwrap_err();
        assert!(e.contains("not shared"), "{e}");
    }

    #[test]
    fn limits_the_runtimes_own_memory_cannot_satisfy_fail() {
        // Declares more pages than the runtime's initial.
        let too_big = post_pass_module(
            Some((
                HOST_MEMORY_INITIAL_PAGES + 1,
                Some(HOST_MEMORY_MAX_PAGES),
                true,
            )),
            CONFORMING,
            false,
        );
        let e = check_post_pass(&too_big).unwrap_err();
        assert!(e.contains("does not satisfy its declared limits"), "{e}");

        // Declares a maximum narrower than the one the runtime creates.
        let narrow = post_pass_module(
            Some((34, Some(HOST_MEMORY_MAX_PAGES - 1), true)),
            CONFORMING,
            false,
        );
        assert!(check_post_pass(&narrow)
            .unwrap_err()
            .contains("does not satisfy its declared limits"));

        // A shared memory must declare one at all.
        let none = post_pass_module(Some((34, None, true)), CONFORMING, false);
        assert!(check_post_pass(&none)
            .unwrap_err()
            .contains("does not satisfy its declared limits"));
    }

    #[test]
    fn a_module_that_does_not_re_export_its_memory_fails() {
        let without: Vec<(&str, u8)> = CONFORMING
            .iter()
            .copied()
            .filter(|(n, _)| *n != "memory")
            .collect();
        let e = check_post_pass(&post_pass_module(
            Some((34, Some(HOST_MEMORY_MAX_PAGES), true)),
            &without,
            false,
        ))
        .unwrap_err();
        assert!(e.contains("does not re-export the memory"), "{e}");
    }

    #[test]
    fn a_module_without_the_transforms_bootstrap_fails() {
        let without: Vec<(&str, u8)> = CONFORMING
            .iter()
            .copied()
            .filter(|(n, _)| *n != "__wbindgen_start")
            .collect();
        let e = check_post_pass(&post_pass_module(
            Some((34, Some(HOST_MEMORY_MAX_PAGES), true)),
            &without,
            false,
        ))
        .unwrap_err();
        assert!(e.contains("no `__wbindgen_start`"), "{e}");
    }

    #[test]
    fn a_module_carrying_both_bootstraps_fails() {
        // The pump reads exactly these to decide which bootstrap to run, so a
        // module offering both leaves nothing to decide.
        for leftover in ["__wasm_init_tls", "__tls_size", "__tls_align"] {
            let mut both = CONFORMING.to_vec();
            both.push((leftover, 0));
            let e = check_post_pass(&post_pass_module(
                Some((34, Some(HOST_MEMORY_MAX_PAGES), true)),
                &both,
                false,
            ))
            .unwrap_err();
            assert!(e.contains("exports both bootstraps"), "{e}");
            assert!(e.contains(leftover), "{e}");
        }
    }

    #[test]
    fn a_start_section_that_survived_the_unstart_fails() {
        let e = check_post_pass(&post_pass_module(
            Some((34, Some(HOST_MEMORY_MAX_PAGES), true)),
            CONFORMING,
            true,
        ))
        .unwrap_err();
        assert!(e.contains("still has a start section"), "{e}");
    }

    #[test]
    fn two_imported_memories_fail() {
        // `wasm_conventions::get_memory` in the CLI refuses these too, so this
        // is unreachable through the post-pass — but the runtime supplies one
        // memory and the failure would otherwise be a browser LinkError.
        let mut imp = Vec::new();
        uleb_out(2, &mut imp);
        for ns in ["./ir_bg.js", "env"] {
            s(ns, &mut imp);
            s("memory", &mut imp);
            imp.push(2);
            imp.push(0x03);
            uleb_out(34, &mut imp);
            uleb_out(HOST_MEMORY_MAX_PAGES as usize, &mut imp);
        }
        let mut m = b"\0asm\x01\0\0\0".to_vec();
        m.push(2);
        uleb_out(imp.len(), &mut m);
        m.extend_from_slice(&imp);
        let e = check_post_pass(&m).unwrap_err();
        assert!(e.contains("more than one memory"), "{e}");
    }

    #[test]
    fn a_post_pass_module_that_cannot_be_decoded_is_an_error_not_a_pass() {
        let e = check_post_pass(b"not wasm at all").unwrap_err();
        assert!(e.contains("cannot read the import section"), "{e}");
    }

    /// A tag import (exception handling) is read like any other, and so is
    /// what follows it; a kind no proposal defines is an error, not a pass.
    #[test]
    fn a_tag_import_is_judged_and_an_unknown_kind_is_an_error() {
        let with_kind = |kind: &[u8]| {
            let mut imp = Vec::new();
            uleb_out(2, &mut imp);
            s("env", &mut imp);
            s("tag", &mut imp);
            imp.extend_from_slice(kind);
            s("__wbindgen_placeholder__", &mut imp);
            s("__wbg_new", &mut imp);
            imp.extend_from_slice(&[0, 0]); // func, typeidx 0
            let mut m = b"\0asm\x01\0\0\0".to_vec();
            m.push(2);
            uleb_out(imp.len(), &mut m);
            m.extend_from_slice(&imp);
            check(&m).unwrap_err()
        };
        let e = with_kind(&[4, 0, 0]); // tag: attribute 0, typeidx 0
        assert!(e.contains("__wbindgen_placeholder__ (1 import)"), "{e}");
        let e = with_kind(&[0x7f, 0]);
        assert!(e.contains("cannot read the import section"), "{e}");
    }
}
