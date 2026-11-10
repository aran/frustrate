//! frustrate's hot patch builder: the command a `*.hot_patch.json` manifest names.
//!
//! ```text
//! frustrate_hotpatch <build options> snapshot --state <dir>
//! frustrate_hotpatch <build options> patch --state <dir> --symbol flutter_hot_patch_apply=0x<addr> --out <dir>
//! ```
//!
//! Build options, written into the manifest by `frustrate_hot_patchable`:
//! `--library` and `--llc`, then one group per compilation the library is
//! built from, each opened by `--unit <crate>`:
//!
//! * `--ir` and `--contract`: the crate's LLVM IR and binding contract. A unit
//!   with them is a bridge crate a patch can carry code from.
//! * `--inputs`: a file listing the compilation's other inputs. A reload that
//!   finds one moved restarts, since a patch carries only bridge crates' code.
//! * repeated `--rustc-arg` and `--env`: the compilation's argv and
//!   environment.
//! * `--links`: this compilation links the library, so its argv names the
//!   linker a patch is linked with. Exactly one unit links.
//!
//! A single bridge's library is one unit that is both. A `frustrate_shared_library` is
//! a linking unit and one unit per bridge crate. Naming a unit again continues
//! its group, and a repeated single-valued option takes its last value.
//!
//! Both commands print exactly one JSON line on stdout; stderr carries
//! diagnostics only.
//!
//! * `snapshot` records the launch build: `{"status":"ok"}`, or
//!   `{"status":"failed","message":…}` with a non-zero exit when the library
//!   cannot be patched at all.
//! * `patch` compares the current build with that record: `unchanged`;
//!   `patched` with the patch `file`, the changed `functions`, and — when a
//!   bridge's interface moved — the `interface` differences the patch carries;
//!   `restart` with `reasons`; or `failed` with a `message` (a compile or link
//!   error). One patch carries every bridge crate's edits.
//!
//! An interface difference is not by itself a restart: a member added,
//! removed, or given a new signature is served by the patch, because dispatch
//! ids are derived from each member's own facts and the patch installs whole
//! new dispatch tables. A changed *type* still restarts, and so does any id
//! that named a different member of the same bridge earlier in this process.
//! See [`frustrate_hotpatch::contract`] for the full classification.
//!
//! Every patch is built against the launch image, never against an earlier
//! patch.

use frustrate_hotpatch::contract;
use frustrate_hotpatch::image::Image;
use frustrate_hotpatch::ir::Module;
use frustrate_hotpatch::link::Toolchain;
use frustrate_hotpatch::patch::{self, Outcome, Target};
use frustrate_hotpatch::state::{self, State};
use serde_json::json;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The export whose running address anchors a patch.
const ANCHOR: &str = "flutter_hot_patch_apply";

/// One compilation of the library: see the module docs.
#[derive(Default)]
struct Unit {
    name: String,
    ir: Option<PathBuf>,
    contract: Option<PathBuf>,
    inputs: Option<PathBuf>,
    rustc_args: Vec<String>,
    env: Vec<(String, String)>,
    links: bool,
}

impl Unit {
    /// The IR and contract of a bridge crate; `None` for a unit that only links.
    fn bridge(&self) -> Result<Option<(&Path, &Path)>, String> {
        match (&self.ir, &self.contract) {
            (Some(ir), Some(contract)) => Ok(Some((ir, contract))),
            (None, None) => Ok(None),
            _ => Err(format!("unit `{}` needs both --ir and --contract, or neither", self.name)),
        }
    }
}

#[derive(Default)]
struct Options {
    library: Option<PathBuf>,
    llc: Option<PathBuf>,
    units: Vec<Unit>,
    command: Option<String>,
    state: Option<PathBuf>,
    symbol: Option<(String, u64)>,
    out: Option<PathBuf>,
}

impl Options {
    fn bridges(&self) -> Result<Vec<(&Unit, &Path, &Path)>, String> {
        let mut out = Vec::new();
        for u in &self.units {
            if let Some((ir, contract)) = u.bridge()? {
                out.push((u, ir, contract));
            }
        }
        if out.is_empty() {
            return Err("no unit has --ir and --contract, so there is no crate to patch".into());
        }
        Ok(out)
    }

    fn linker(&self) -> Result<&Unit, String> {
        let linking: Vec<&Unit> = self.units.iter().filter(|u| u.links).collect();
        match linking.as_slice() {
            [one] => Ok(one),
            _ => Err(format!("exactly one unit must be marked --links, found {}", linking.len())),
        }
    }
}

fn parse(args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut o = Options::default();
    let mut current: Option<usize> = None;
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        let unit = |_: &Options, name: &str| {
            current.ok_or_else(|| format!("{name} belongs to a unit: name one with --unit first"))
        };
        match arg.as_str() {
            "--library" => o.library = Some(value(&arg)?.into()),
            "--llc" => o.llc = Some(value(&arg)?.into()),
            "--unit" => {
                let name = value(&arg)?;
                current = Some(match o.units.iter().position(|u| u.name == name) {
                    Some(i) => i,
                    None => {
                        o.units.push(Unit { name, ..Unit::default() });
                        o.units.len() - 1
                    }
                });
            }
            "--ir" => {
                let v = value(&arg)?;
                let i = unit(&o, &arg)?;
                o.units[i].ir = Some(v.into());
            }
            "--contract" => {
                let v = value(&arg)?;
                let i = unit(&o, &arg)?;
                o.units[i].contract = Some(v.into());
            }
            "--inputs" => {
                let v = value(&arg)?;
                let i = unit(&o, &arg)?;
                o.units[i].inputs = Some(v.into());
            }
            "--rustc-arg" => {
                let v = value(&arg)?;
                let i = unit(&o, &arg)?;
                o.units[i].rustc_args.push(v);
            }
            "--env" => {
                let v = value(&arg)?;
                let (k, val) = v.split_once('=').ok_or("--env takes KEY=VALUE")?;
                let i = unit(&o, &arg)?;
                o.units[i].env.push((k.to_string(), val.to_string()));
            }
            "--links" => {
                let i = unit(&o, &arg)?;
                o.units[i].links = true;
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

/// Where a unit's record lives: its own directory, so each bridge keeps its own
/// baseline, contract and id ledger.
fn unit_state(state: &Path, unit: &Unit) -> Result<State, String> {
    let dir = state.join("unit").join(&unit.name);
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create the state directory: {e}"))?;
    Ok(State::new(&dir))
}

/// A sentence about one bridge, named when the library has more than one.
fn about(several: bool, unit: &Unit, sentence: &str) -> String {
    if several {
        format!("in bridge `{}`: {sentence}", unit.name)
    } else {
        sentence.to_string()
    }
}

fn snapshot(o: &Options) -> Result<serde_json::Value, String> {
    let library = required(&o.library, "--library")?;
    let state_dir = required(&o.state, "--state")?;
    std::fs::create_dir_all(state_dir).map_err(|e| format!("cannot create the state directory: {e}"))?;
    let state = State::new(state_dir);

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
    o.linker()?;
    o.bridges()?;

    for unit in &o.units {
        if !unit.rustc_args.iter().any(|a| a.contains("link-dead-code")) {
            return Err(format!(
                "{} was linked with dead-code stripping, so it holds only the code the launch build happened to call: a patch could not call anything else. frustrate_hot_patchable compiles a patchable library and its bridge crates with -Clink-dead-code (`{}` was compiled without it)",
                library.display(),
                unit.name
            ));
        }
        let unit_state = unit_state(state_dir, unit)?;
        if let Some((ir, contract)) = unit.bridge()? {
            let text = read_ir(ir)?;
            let module = Module::parse(&text)?;
            if !module.has_debug_info() {
                return Err(format!(
                    "`{}`'s compilation carries no debug info, so a changed type layout could not be detected: build with -c dbg",
                    unit.name
                ));
            }
            unit_state.write_baseline(&patch::baseline(&module))?;
            let contract = std::fs::read(contract).map_err(|e| format!("cannot read the binding contract: {e}"))?;
            // Seeds the id ledger: what every id means in the image about to
            // run. Per bridge, because ids are: each bridge dispatches through
            // its own exports, so two bridges may hold the same id for
            // different members without either caller reaching the other.
            unit_state.record_ids(&contract::member_ids(&contract)?)?;
            std::fs::write(unit_state.contract(), contract).map_err(|e| format!("cannot save the contract: {e}"))?;
        }
        let inputs = state::digests(&input_paths(required(&unit.inputs, "--inputs")?)?, &[])?;
        unit_state.write_inputs(&inputs)?;
    }

    std::fs::write(state.library(), &bytes).map_err(|e| format!("cannot save the library: {e}"))?;
    state.write_builder()?;
    Ok(json!({"status": "ok"}))
}

fn patch(o: &Options) -> Result<serde_json::Value, String> {
    let state_dir = required(&o.state, "--state")?;
    let state = State::new(state_dir);
    let out_dir = required(&o.out, "--out")?;
    let (symbol, anchor) = required(&o.symbol, "--symbol")?;
    if symbol != ANCHOR {
        return Err(format!("--symbol must name {ANCHOR}, not {symbol}"));
    }

    let bytes = std::fs::read(state.library()).map_err(|e| format!("no snapshot in the state directory: {e}"))?;
    if !state.written_by_this_builder()? {
        return Ok(json!({"status": "restart", "reasons": [
            "the patch builder itself was rebuilt since this app launched, so it cannot compare against the launch record"
        ]}));
    }
    let bridges = o.bridges()?;
    let several = bridges.len() > 1;

    // Every bridge's interface first, then every bridge's ids: nothing is
    // built while any of them cannot be served.
    let mut carried = Vec::new();
    let mut refused = Vec::new();
    let mut ids = Vec::new();
    for (unit, _, contract) in &bridges {
        let unit_state = unit_state(state_dir, unit)?;
        let old = std::fs::read(unit_state.contract()).map_err(|e| format!("no snapshot of `{}` in the state directory: {e}", unit.name))?;
        let new = std::fs::read(contract).map_err(|e| format!("cannot read the binding contract: {e}"))?;
        let interface = contract::classify(&old, &new);
        carried.extend(interface.carried.iter().map(|s| about(several, unit, s)));
        refused.extend(interface.refused.iter().map(|s| about(several, unit, s)));
        ids.push((unit_state, contract::member_ids(&new)?));
    }
    if !refused.is_empty() {
        return Ok(json!({"status": "restart", "reasons": refused}));
    }
    // An id that named a different member of the same bridge earlier in this
    // process cannot be served, whatever the interface difference is: Dart
    // still holding the old meaning would reach the new member. Checked before
    // anything is built, since no patch can make it safe.
    let mut reused = Vec::new();
    for ((unit, _, _), (unit_state, unit_ids)) in bridges.iter().zip(&ids) {
        reused.extend(unit_state.reused_ids(unit_ids)?.iter().map(|s| about(several, unit, s)));
    }
    if !reused.is_empty() {
        return Ok(json!({"status": "restart", "reasons": reused}));
    }

    let mut inputs_moved = Vec::new();
    for unit in &o.units {
        let unit_state = unit_state(state_dir, unit)?;
        let previous = unit_state.read_inputs()?;
        let current = state::digests(&input_paths(required(&unit.inputs, "--inputs")?)?, &previous)?;
        inputs_moved.extend(state::changed(&previous, &current));
    }
    inputs_moved.sort();
    inputs_moved.dedup();
    if !inputs_moved.is_empty() {
        let reasons: Vec<String> = inputs_moved
            .iter()
            .map(|p| format!("`{}` changed, and a patch carries only the bridge crates' own code", describe_input(p)))
            .collect();
        return Ok(json!({"status": "restart", "reasons": reasons}));
    }

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

    // Each bridge crate's edit, against that crate's own launch compilation.
    // Bridge crates that depend on one another restart above instead: an edit
    // to one moves an input of the other.
    let mut built = Vec::new();
    let mut restart = Vec::new();
    for ((unit, ir, _), (unit_state, _)) in bridges.iter().zip(&ids) {
        let base = unit_state.read_baseline()?;
        let text = read_ir(ir)?;
        let module = Module::parse(&text)?;
        match patch::build(&base, &module, &target) {
            Outcome::Unchanged => {}
            Outcome::Restart(reasons) => restart.extend(reasons.iter().map(|s| about(several, unit, s))),
            Outcome::Patch(p) => built.push(p),
        }
    }
    if !restart.is_empty() {
        return Ok(json!({"status": "restart", "reasons": restart}));
    }
    if built.is_empty() {
        return Ok(json!({"status": "unchanged"}));
    }
    let routed: Vec<&str> = built.iter().flat_map(|p| p.routed.iter().map(String::as_str)).collect();
    eprintln!("frustrate_hotpatch: redirecting {}", routed.join(", "));
    for s in built.iter().flat_map(|p| &p.kept_statics) {
        eprintln!("frustrate_hotpatch: the initializer of static `{s}` changed; the running value is kept");
    }

    std::fs::create_dir_all(out_dir).map_err(|e| format!("cannot create {}: {e}", out_dir.display()))?;
    let n = (0..)
        .find(|i| !out_dir.join(format!("patch-{i}.support.ll")).exists())
        .expect("a free patch number");
    let stem = format!("patch-{n}");
    let mut modules = Vec::new();
    for (i, p) in built.iter().enumerate() {
        modules.push((out_dir.join(format!("{stem}.{i}.ll")), p.ir.clone()));
    }
    let parts: Vec<&patch::Patch> = built.iter().collect();
    for (path, text) in &modules {
        std::fs::write(path, text).map_err(|e| format!("cannot write the patch: {e}"))?;
    }

    let linker = o.linker()?;
    let toolchain = Toolchain::from_rustc_argv(
        o.llc.clone().ok_or(
            "no `llc` to compile patches with: the build found no Rust toolchain that runs on this machine and targets this library's platform",
        )?,
        &linker.rustc_args,
        linker.env.clone(),
    );
    let extension = match image.format {
        frustrate_hotpatch::image::Format::MachO => "dylib",
        frustrate_hotpatch::image::Format::Elf => "so",
    };
    let library = out_dir.join(format!("{stem}.{extension}"));
    // The crates' modules first: what the backend lowers them to can call
    // functions their IR does not name, and the support module binds those too.
    let mut objects: Vec<PathBuf> = modules.iter().map(|(p, _)| p.with_extension("o")).collect();
    let support_module = out_dir.join(format!("{stem}.support.ll"));
    let support_object = support_module.with_extension("o");
    let compiled = modules
        .iter()
        .zip(&objects)
        .try_for_each(|((module, _), object)| toolchain.compile(module, object))
        .and_then(|_| {
            let bytes = objects
                .iter()
                .map(|o| std::fs::read(o).map_err(|e| format!("cannot read {}: {e}", o.display())))
                .collect::<Result<Vec<_>, _>>()?;
            let bound: BTreeSet<&str> = built.iter().flat_map(|p| p.bound.iter().map(String::as_str)).collect();
            let backend = patch::backend_references(&bytes, &bound, &image)?;
            let text = patch::support(&parts, &target, &backend);
            std::fs::write(&support_module, &text).map_err(|e| format!("cannot write the patch: {e}"))?;
            toolchain.compile(&support_module, &support_object)
        })
        .and_then(|_| {
            objects.push(support_object.clone());
            let objects: Vec<&Path> = objects.iter().map(PathBuf::as_path).collect();
            toolchain.link(image.format, image.arch, &objects, &library)
        });
    if let Err(message) = compiled {
        return Ok(json!({"status": "failed", "message": message}));
    }
    // The patch links, so these ids are about to be live in this process.
    // Recorded before the reply, so the next patch is compared against them
    // even if the Dart half of this reload never arrives.
    for (unit_state, unit_ids) in &ids {
        unit_state.record_ids(unit_ids)?;
    }
    let file = std::fs::canonicalize(&library).unwrap_or(library);
    let functions: Vec<&String> = built.iter().flat_map(|p| &p.functions).collect();
    let mut reply = json!({
        "status": "patched",
        "file": file.to_string_lossy(),
        "functions": functions,
    });
    if !carried.is_empty() {
        reply["interface"] = json!(carried);
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
