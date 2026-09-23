//! frustrate's hot patch builder: the command a `*.hot_patch.json` manifest names.
//!
//! ```text
//! frustrate_hotpatch <build options> snapshot --state <dir>
//! frustrate_hotpatch <build options> patch --state <dir> --symbol flutter_hot_patch_apply=0x<addr> --out <dir>
//! ```
//!
//! Build options, written into the manifest by `frustrate_hot_patchable`:
//! `--library`, `--ir`, `--contract`, `--inputs` (a file listing the
//! compilation's other inputs), `--llc`, and repeated `--rustc-arg` and `--env`.
//! A repeated single-valued option takes its last value.
//!
//! Both commands print exactly one JSON line on stdout; stderr carries
//! diagnostics only.
//!
//! * `snapshot` records the launch build: `{"status":"ok"}`, or
//!   `{"status":"failed","message":…}` with a non-zero exit when the library
//!   cannot be patched at all.
//! * `patch` compares the current build with that record: `unchanged`;
//!   `patched` with the patch `file`, the changed `functions`, and — when the
//!   bridge's interface moved — the `interface` differences the patch carries;
//!   `restart` with `reasons`; or `failed` with a `message` (a compile or link
//!   error).
//!
//! An interface difference is not by itself a restart: a member added,
//! removed, or given a new signature is served by the patch, because dispatch
//! ids are derived from each member's own facts and the patch installs whole
//! new dispatch tables. A changed *type* still restarts, and so does any id
//! that named a different member earlier in this process. See
//! [`frustrate_hotpatch::contract`] for the full classification.
//!
//! Every patch is built against the launch image, never against an earlier
//! patch.

use frustrate_hotpatch::image::Image;
use frustrate_hotpatch::ir::Module;
use frustrate_hotpatch::link::Toolchain;
use frustrate_hotpatch::patch::{self, Outcome, Target};
use frustrate_hotpatch::state::{self, State};
use frustrate_hotpatch::contract;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The export whose running address anchors a patch.
const ANCHOR: &str = "flutter_hot_patch_apply";

#[derive(Default)]
struct Options {
    library: Option<PathBuf>,
    ir: Option<PathBuf>,
    contract: Option<PathBuf>,
    inputs: Option<PathBuf>,
    llc: Option<PathBuf>,
    rustc_args: Vec<String>,
    env: Vec<(String, String)>,
    command: Option<String>,
    state: Option<PathBuf>,
    symbol: Option<(String, u64)>,
    out: Option<PathBuf>,
}

fn parse(args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut o = Options::default();
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--library" => o.library = Some(value(&arg)?.into()),
            "--ir" => o.ir = Some(value(&arg)?.into()),
            "--contract" => o.contract = Some(value(&arg)?.into()),
            "--inputs" => o.inputs = Some(value(&arg)?.into()),
            "--llc" => o.llc = Some(value(&arg)?.into()),
            "--rustc-arg" => o.rustc_args.push(value(&arg)?),
            "--env" => {
                let v = value(&arg)?;
                let (k, val) = v.split_once('=').ok_or("--env takes KEY=VALUE")?;
                o.env.push((k.to_string(), val.to_string()));
            }
            "--state" => o.state = Some(value(&arg)?.into()),
            "--out" => o.out = Some(value(&arg)?.into()),
            "--symbol" => {
                let v = value(&arg)?;
                let (name, addr) = v.split_once('=').ok_or("--symbol takes NAME=0xADDRESS")?;
                let addr = u64::from_str_radix(addr.trim_start_matches("0x"), 16)
                    .map_err(|e| format!("--symbol address: {e}"))?;
                o.symbol = Some((name.to_string(), addr));
            }
            "snapshot" | "patch" if o.command.is_none() => o.command = Some(arg),
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    Ok(o)
}

fn required<'a, T>(v: &'a Option<T>, name: &str) -> Result<&'a T, String> {
    v.as_ref().ok_or_else(|| format!("{name} is required"))
}

fn main() -> ExitCode {
    let result = parse(std::env::args().skip(1)).and_then(|o| match o.command.as_deref() {
        Some("snapshot") => snapshot(&o),
        Some("patch") => patch(&o),
        _ => Err("expected `snapshot` or `patch`".into()),
    });
    match result {
        Ok(reply) => {
            println!("{reply}");
            if reply["status"] == "failed" {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(message) => {
            println!("{}", json!({"status": "failed", "message": message}));
            ExitCode::FAILURE
        }
    }
}

fn read_ir(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
}

fn input_paths(list: &Path) -> Result<Vec<String>, String> {
    Ok(std::fs::read_to_string(list)
        .map_err(|e| format!("cannot read {}: {e}", list.display()))?
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

fn snapshot(o: &Options) -> Result<serde_json::Value, String> {
    let library = required(&o.library, "--library")?;
    let state = State::new(required(&o.state, "--state")?);
    std::fs::create_dir_all(required(&o.state, "--state")?)
        .map_err(|e| format!("cannot create the state directory: {e}"))?;

    let bytes = std::fs::read(library).map_err(|e| format!("cannot read {}: {e}", library.display()))?;
    let image = Image::read(&bytes)?;
    if image.identity.is_none() {
        return Err(format!(
            "{} carries no linker identity (LC_UUID on Mach-O, a GNU build-id on ELF), so a patch could not tell it from another build",
            library.display()
        ));
    }
    if !image.exported.contains(ANCHOR) {
        return Err(format!(
            "{} was not built to take patches: build it with -c dbg through frustrate_hot_patchable, which compiles it with --cfg=frustrate_hot_patch",
            library.display()
        ));
    }

    if !o.rustc_args.iter().any(|a| a.contains("link-dead-code")) {
        return Err(format!(
            "{} was linked with dead-code stripping, so it holds only the code the launch build happened to call: a patch could not call anything else. frustrate_hot_patchable compiles a patchable library with -Clink-dead-code",
            library.display()
        ));
    }

    let text = read_ir(required(&o.ir, "--ir")?)?;
    let module = Module::parse(&text)?;
    if !module.has_debug_info() {
        return Err("the library's compilation carries no debug info, so a changed type layout could not be detected: build with -c dbg".into());
    }
    state.write_baseline(&patch::baseline(&module))?;

    std::fs::write(state.library(), &bytes).map_err(|e| format!("cannot save the library: {e}"))?;
    let contract = std::fs::read(required(&o.contract, "--contract")?)
        .map_err(|e| format!("cannot read the binding contract: {e}"))?;
    // Seeds the id ledger: what every id means in the image about to run.
    state.record_ids(&contract::member_ids(&contract)?)?;
    std::fs::write(state.contract(), contract).map_err(|e| format!("cannot save the contract: {e}"))?;
    let inputs = state::digests(&input_paths(required(&o.inputs, "--inputs")?)?, &[])?;
    state.write_inputs(&inputs)?;
    state.write_builder()?;
    Ok(json!({"status": "ok"}))
}

fn patch(o: &Options) -> Result<serde_json::Value, String> {
    let state = State::new(required(&o.state, "--state")?);
    let out_dir = required(&o.out, "--out")?;
    let (symbol, anchor) = required(&o.symbol, "--symbol")?;
    if symbol != ANCHOR {
        return Err(format!("--symbol must name {ANCHOR}, not {symbol}"));
    }

    let old_contract = std::fs::read(state.contract()).map_err(|e| format!("no snapshot in the state directory: {e}"))?;
    if !state.written_by_this_builder()? {
        return Ok(json!({"status": "restart", "reasons": [
            "the patch builder itself was rebuilt since this app launched, so it cannot compare against the launch record"
        ]}));
    }
    let new_contract = std::fs::read(required(&o.contract, "--contract")?)
        .map_err(|e| format!("cannot read the binding contract: {e}"))?;
    let interface = contract::classify(&old_contract, &new_contract);
    if !interface.refused.is_empty() {
        return Ok(json!({"status": "restart", "reasons": interface.refused}));
    }
    // An id that named a different member earlier in this process cannot be
    // served, whatever the interface difference is: Dart still holding the old
    // meaning would reach the new member. Checked before anything is built,
    // since no patch can make it safe.
    let ids = contract::member_ids(&new_contract)?;
    let reused = state.reused_ids(&ids)?;
    if !reused.is_empty() {
        return Ok(json!({"status": "restart", "reasons": reused}));
    }

    let previous = state.read_inputs()?;
    let current = state::digests(&input_paths(required(&o.inputs, "--inputs")?)?, &previous)?;
    let inputs_moved = state::changed(&previous, &current);
    if !inputs_moved.is_empty() {
        let reasons: Vec<String> = inputs_moved
            .iter()
            .map(|p| format!("`{}` changed, and a patch carries only the bridge crate's own code", describe_input(p)))
            .collect();
        return Ok(json!({"status": "restart", "reasons": reasons}));
    }

    let bytes = std::fs::read(state.library()).map_err(|e| format!("no snapshot in the state directory: {e}"))?;
    let image = Image::read(&bytes)?;
    let file_anchor = image
        .symbols
        .get(ANCHOR)
        .ok_or_else(|| format!("the launch library has no {ANCHOR}"))?
        .address;
    let target = Target {
        image: &image,
        slide: *anchor as i64 - file_anchor as i64,
        anchor: *anchor,
    };

    let base = state.read_baseline()?;
    let text = read_ir(required(&o.ir, "--ir")?)?;
    let module = Module::parse(&text)?;
    let built = match patch::build(&base, &module, &target) {
        Outcome::Unchanged => return Ok(json!({"status": "unchanged"})),
        Outcome::Restart(reasons) => return Ok(json!({"status": "restart", "reasons": reasons})),
        Outcome::Patch(p) => p,
    };
    eprintln!(
        "frustrate_hotpatch: redirecting {}",
        built.routed.join(", ")
    );
    for s in &built.kept_statics {
        eprintln!("frustrate_hotpatch: the initializer of static `{s}` changed; the running value is kept");
    }

    std::fs::create_dir_all(out_dir).map_err(|e| format!("cannot create {}: {e}", out_dir.display()))?;
    let n = (0..)
        .find(|i| !out_dir.join(format!("patch-{i}.ll")).exists())
        .expect("a free patch number");
    let stem = format!("patch-{n}");
    let ir_path = out_dir.join(format!("{stem}.ll"));
    let stubs_path = out_dir.join(format!("{stem}.stubs.ll"));
    std::fs::write(&ir_path, &built.ir).map_err(|e| format!("cannot write the patch: {e}"))?;
    std::fs::write(&stubs_path, &built.stubs).map_err(|e| format!("cannot write the patch: {e}"))?;

    let toolchain = Toolchain::from_rustc_argv(
        required(&o.llc, "--llc")?.clone(),
        &o.rustc_args,
        o.env.clone(),
    );
    let object = ir_path.with_extension("o");
    let stubs_object = stubs_path.with_extension("o");
    let extension = match image.format {
        frustrate_hotpatch::image::Format::MachO => "dylib",
        frustrate_hotpatch::image::Format::Elf => "so",
    };
    let library = out_dir.join(format!("{stem}.{extension}"));
    let compiled = toolchain
        .compile(&ir_path, &object)
        .and_then(|_| toolchain.compile(&stubs_path, &stubs_object))
        .and_then(|_| toolchain.link(image.format, image.arch, &[&object, &stubs_object], &library));
    if let Err(message) = compiled {
        return Ok(json!({"status": "failed", "message": message}));
    }
    // The patch links, so these ids are about to be live in this process.
    // Recorded before the reply, so the next patch is compared against them
    // even if the Dart half of this reload never arrives.
    state.record_ids(&ids)?;
    let file = std::fs::canonicalize(&library).unwrap_or(library);
    let mut reply = json!({
        "status": "patched",
        "file": file.to_string_lossy(),
        "functions": built.functions,
    });
    if !interface.carried.is_empty() {
        reply["interface"] = json!(interface.carried);
    }
    Ok(reply)
}

/// A crate name for an rlib path, the path otherwise.
fn describe_input(path: &str) -> String {
    let file = Path::new(path).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
    if let Some(stem) = file.strip_prefix("lib").and_then(|f| f.split_once('-')).map(|(s, _)| s) {
        if file.ends_with(".rlib") || file.ends_with(".rmeta") {
            return stem.to_string();
        }
    }
    path.to_string()
}
