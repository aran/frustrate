//! A patchable library, launched in this process and patched through its
//! manifest the way a dev tool drives it.
//!
//! One test, because the steps share the loaded image and are ordered: each
//! patch is built against the launch build, and what the process runs after
//! one depends on the patches before it.

use serde_json::Value;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::process::Command;

extern "C" {
    fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
    fn dlerror() -> *const c_char;
}

type CallSync = unsafe extern "C" fn(u32, *const u8, u64, *mut u8, u64) -> i32;
type Apply = unsafe extern "C" fn(*const c_char, *mut c_char, usize) -> i32;

/// A fixture member's dispatch id, read from the interface IR the fixture's
/// two halves were generated from.
///
/// Not a literal. Ids are derived from each member's wire facts
/// (`codegen/src/hash.rs`), so a literal would be a copy of a hash — and the
/// copy would still dispatch *somewhere* after a signature changed, which is
/// the failure this whole test file exists to make impossible.
fn fn_id(contract: &Path, member: &str) -> u32 {
    let ir: serde_json::Value =
        serde_json::from_slice(&std::fs::read(contract).expect("the contract")).expect("interface IR");
    let found: Vec<&serde_json::Value> = ir["functions"]
        .as_array()
        .expect("functions")
        .iter()
        .filter(|f| f["name"] == member)
        .collect();
    assert_eq!(found.len(), 1, "exactly one member named {member}: {found:?}");
    found[0]["fn_id"].as_u64().expect("an fn_id") as u32
}

struct Data {
    files: Vec<PathBuf>,
}

impl Data {
    fn load() -> Data {
        let root = std::env::var("RUNFILES_DIR")
            .or_else(|_| std::env::var("TEST_SRCDIR"))
            .expect("runfiles");
        let files = std::env::var("HOTPATCH_DATA")
            .expect("HOTPATCH_DATA")
            .split_whitespace()
            .map(|p| Path::new(&root).join(p))
            .collect();
        Data { files }
    }

    fn find(&self, suffix: &str) -> PathBuf {
        let found: Vec<&PathBuf> = self
            .files
            .iter()
            .filter(|f| f.to_string_lossy().ends_with(suffix))
            .collect();
        assert_eq!(found.len(), 1, "exactly one data file ends with {suffix}: {found:?}");
        found[0].clone()
    }
}

struct Library {
    call_sync: CallSync,
    apply: Apply,
    anchor: usize,
}

impl Library {
    fn open(path: &Path) -> Library {
        let c = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let handle = unsafe { dlopen(c.as_ptr(), 2) };
        assert!(!handle.is_null(), "dlopen: {}", unsafe { CStr::from_ptr(dlerror()) }.to_string_lossy());
        let sym = |name: &CStr| {
            let p = unsafe { dlsym(handle, name.as_ptr()) };
            assert!(!p.is_null(), "{name:?} is not exported");
            p
        };
        let abi: unsafe extern "C" fn() -> u32 = unsafe { std::mem::transmute(sym(c"flutter_hot_patch_abi")) };
        assert_eq!(unsafe { abi() }, 1);
        let apply = sym(c"flutter_hot_patch_apply");
        Library {
            call_sync: unsafe { std::mem::transmute(sym(c"frustrate_call_sync")) },
            apply: unsafe { std::mem::transmute(apply) },
            anchor: apply as usize,
        }
    }

    fn call(&self, fn_id: u32, args: &[i64]) -> i64 {
        let req: Vec<u8> = args.iter().flat_map(|a| a.to_le_bytes()).collect();
        let mut out = [0u8; 64];
        let n = unsafe { (self.call_sync)(fn_id, req.as_ptr(), req.len() as u64, out.as_mut_ptr(), out.len() as u64) };
        assert_eq!((n, out[0]), (9, 0), "an ok envelope with one i64: {:?}", &out[..n.max(0) as usize]);
        i64::from_le_bytes(out[1..9].try_into().unwrap())
    }

    /// The response envelope's status byte, without demanding success — 0 is
    /// ok, and a non-zero status is what a refused call answers. `None` means
    /// the dispatcher wrote nothing at all.
    fn call_status(&self, fn_id: u32, args: &[i64]) -> Option<u8> {
        let req: Vec<u8> = args.iter().flat_map(|a| a.to_le_bytes()).collect();
        let mut out = [0u8; 64];
        let n = unsafe { (self.call_sync)(fn_id, req.as_ptr(), req.len() as u64, out.as_mut_ptr(), out.len() as u64) };
        (n > 0).then(|| out[0])
    }

    fn apply(&self, patch: Option<&str>) -> Result<(), String> {
        let path = patch.map(|p| CString::new(p).unwrap());
        let mut message = [0u8; 512];
        let rc = unsafe {
            (self.apply)(
                path.as_ref().map_or(std::ptr::null(), |p| p.as_ptr()),
                message.as_mut_ptr() as *mut c_char,
                message.len(),
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(CStr::from_bytes_until_nul(&message).unwrap().to_string_lossy().into_owned())
        }
    }
}

struct Builder {
    command: Vec<String>,
    exec_root: PathBuf,
    state: PathBuf,
    out: PathBuf,
}

impl Builder {
    fn run(&self, extra: &[&str]) -> Value {
        self.run_with_diagnostics(extra).0
    }

    /// The JSON line, and what the builder said on stderr.
    fn run_with_diagnostics(&self, extra: &[&str]) -> (Value, String) {
        let out = Command::new(self.exec_root.join(&self.command[0]))
            .args(&self.command[1..])
            .args(extra)
            .current_dir(&self.exec_root)
            .output()
            .expect("run the patch builder");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert_eq!(stdout.lines().count(), 1, "one JSON line on stdout: {stdout}\n{stderr}");
        (
            serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}")),
            stderr,
        )
    }

    fn patch(&self, ir: &Path, contract: &Path, anchor: usize) -> Value {
        self.patch_with_diagnostics(ir, contract, anchor).0
    }

    fn patch_with_diagnostics(&self, ir: &Path, contract: &Path, anchor: usize) -> (Value, String) {
        self.run_with_diagnostics(&[
            "--ir",
            &ir.to_string_lossy(),
            "--contract",
            &contract.to_string_lossy(),
            "patch",
            "--state",
            &self.state.to_string_lossy(),
            "--symbol",
            &format!("flutter_hot_patch_apply={anchor:#x}"),
            "--out",
            &self.out.to_string_lossy(),
        ])
    }
}

fn reasons(reply: &Value) -> String {
    assert_eq!(reply["status"], "restart", "{reply}");
    reply["reasons"].to_string()
}

#[test]
fn a_running_library_takes_patches_and_refuses_what_it_cannot() {
    let data = Data::load();
    let manifest_path = data.find("tests/hotpatch/fixture_hot.hot_patch.hot_patch.json");
    // A data file resolves to the build's own output, which sits under the
    // execution root the manifest's paths are relative to.
    let real = std::fs::canonicalize(&manifest_path).unwrap();
    let real = real.to_string_lossy();
    let exec_root = PathBuf::from(&real[..real.find("/bazel-out/").expect("an output path")]);
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
    assert_eq!(manifest["version"], 1);
    assert!(
        manifest["sources"].as_array().unwrap().iter().any(|s| s == "tests/hotpatch/fixture/src/api.rs"),
        "the crate's own sources are listed: {manifest}"
    );

    let tmp = PathBuf::from(std::env::var("TEST_TMPDIR").unwrap());
    let builder = Builder {
        command: manifest["command"].as_array().unwrap().iter().map(|a| a.as_str().unwrap().to_string()).collect(),
        exec_root: exec_root.clone(),
        state: tmp.join("state"),
        out: tmp.join("out"),
    };

    let library = Library::open(&exec_root.join(manifest["library"].as_str().unwrap()));
    assert_eq!(builder.run(&["snapshot", "--state", &builder.state.to_string_lossy()]), serde_json::json!({"status": "ok"}));

    let launch_contract = data.find("tests/hotpatch/fixture/bridge/interface.frustrate.json");
    // The two members these assertions drive, named rather than numbered.
    let (add, count) = (fn_id(&launch_contract, "add"), fn_id(&launch_contract, "count"));

    assert_eq!(library.call(add, &[2, 3]), 5);
    assert_eq!(library.call(count, &[]), 1, "a static and a thread-local, both fresh");
    assert_eq!(library.call(count, &[]), 3);

    let edit = |name: &str| (data.find(&format!("tests/hotpatch/{name}.ll")), data.find(&format!("tests/hotpatch/{name}/bridge/interface.frustrate.json")));

    // Two bodies edited: both run new code, and `count` keeps counting in the
    // static the launched code was counting in.
    let (ir, contract) = edit("body_edit");
    let reply = builder.patch(&ir, &contract, library.anchor);
    assert_eq!(reply["status"], "patched", "{reply}");
    let functions = reply["functions"].to_string();
    assert!(functions.contains("hotpatch_fixture::api::add") && functions.contains("hotpatch_fixture::api::count"), "{reply}");
    library.apply(reply["file"].as_str()).unwrap();
    assert_eq!(library.call(add, &[2, 3]), 1005);
    assert_eq!(library.call(count, &[]), 105, "both the static and the thread-local carried over");

    // A second patch is built against the launch build, not on top of the
    // first: `count`, unedited this time, runs as it launched.
    let (ir, contract) = edit("body_edit_again");
    let reply = builder.patch(&ir, &contract, library.anchor);
    assert_eq!(reply["status"], "patched", "{reply}");
    library.apply(reply["file"].as_str()).unwrap();
    assert_eq!(library.call(add, &[2, 3]), 6);
    assert_eq!(library.call(count, &[]), 7);

    // Every edit reverted: nothing to build, and a null patch sends calls back
    // to the launched code.
    let launch_ir = data.find("tests/hotpatch/fixture/fixture.hot_patch.ll");
    assert_eq!(builder.patch(&launch_ir, &launch_contract, library.anchor), serde_json::json!({"status": "unchanged"}));
    library.apply(None).unwrap();
    assert_eq!(library.call(add, &[2, 3]), 5);
    assert_eq!(library.call(count, &[]), 9);

    // A function the edit adds: the running library has no copy, so the patch
    // carries its body.
    let (ir, contract) = edit("new_function");
    let reply = builder.patch(&ir, &contract, library.anchor);
    assert_eq!(reply["status"], "patched", "{reply}");
    library.apply(reply["file"].as_str()).unwrap();
    assert_eq!(library.call(add, &[2, 3]), 7);
    library.apply(None).unwrap();

    // A function of a dependency crate that the launch build never linked: the
    // library retains what its crates compiled, so the patch can call it.
    let (ir, contract) = edit("dependency_call");
    let reply = builder.patch(&ir, &contract, library.anchor);
    assert_eq!(reply["status"], "patched", "{reply}");
    library.apply(reply["file"].as_str()).unwrap();
    assert_eq!(library.call(add, &[2, 3]), 5, "live_count() is 0 with no resident objects");
    library.apply(None).unwrap();

    // An actor method and an `async fn`: different entry points from the sync
    // one. Applying the patch is what proves their slots resolve — apply
    // refuses a patch naming a slot this library does not export.
    let (ir, contract) = edit("actor_edit");
    let (reply, said) = builder.patch_with_diagnostics(&ir, &contract, library.anchor);
    assert_eq!(reply["status"], "patched", "{reply}");
    let functions = reply["functions"].to_string();
    assert!(
        functions.contains("Ledger>::record") && functions.contains("api::eventually"),
        "{reply}"
    );
    assert!(
        said.contains("frustrate_actor_call") && said.contains("frustrate_call_async"),
        "the actor and async entry points are the ones redirected: {said}"
    );
    library.apply(reply["file"].as_str()).unwrap();
    // The sync entry point still answers while the actor and async ones are
    // routed into the patch.
    assert_eq!(library.call(add, &[2, 3]), 5);
    library.apply(None).unwrap();

    // The `Drop` of an opaque handle: freeing one runs through the generated
    // drop export, so a patch of it must redirect that entry point.
    let (ir, contract) = edit("drop_edit");
    let (reply, said) = builder.patch_with_diagnostics(&ir, &contract, library.anchor);
    assert_eq!(reply["status"], "patched", "{reply}");
    assert!(
        said.contains("frustrate_drop_Doc") && said.contains("frustrate_finalize_Doc"),
        "the drop exports are redirected: {said}"
    );
    library.apply(reply["file"].as_str()).unwrap();
    library.apply(None).unwrap();

    // A bridged function the launch build never had: the patch installs whole
    // new dispatch tables, so the new member is reachable, and the members
    // that did not change keep the ids their callers already hold. That second
    // half is the property positional ids could not give.
    let (ir, contract) = edit("contract_change");
    let reply = builder.patch(&ir, &contract, library.anchor);
    assert_eq!(reply["status"], "patched", "{reply}");
    assert!(
        reply["interface"].to_string().contains("api::sub") && reply["interface"].to_string().contains("added"),
        "the reply names what the interface gained: {reply}"
    );
    library.apply(reply["file"].as_str()).unwrap();
    assert_eq!(library.call(fn_id(&contract, "sub"), &[9, 4]), 5, "the added member answers");
    assert_eq!(library.call(add, &[2, 3]), 5, "an unchanged member keeps the id its callers hold");
    library.apply(None).unwrap();

    // A changed signature, which is the case the whole id scheme exists for.
    // `add` keeps its name and takes one i64 instead of two, so its wire facts
    // — and therefore its id — move. A caller generated before the change
    // holds the old id, and what it must find there is *nothing*: under
    // positional ids that id would still be live and would decode two i64s in
    // a body reading one.
    let (ir, contract) = edit("signature_change");
    let reply = builder.patch(&ir, &contract, library.anchor);
    assert_eq!(reply["status"], "patched", "{reply}");
    library.apply(reply["file"].as_str()).unwrap();
    let new_add = fn_id(&contract, "add");
    assert_ne!(new_add, add, "a changed signature must move the id");
    assert_eq!(library.call(new_add, &[41]), 42, "the new signature answers");
    assert_eq!(
        library.call_status(add, &[2, 3]),
        Some(2),
        "the old id is absent, so a stale caller gets a panic envelope rather than this body"
    );
    library.apply(None).unwrap();

    // What a patch cannot carry is refused with the cause named.

    let (ir, contract) = edit("layout_change");
    let why = reasons(&builder.patch(&ir, &contract, library.anchor));
    assert!(why.contains("hotpatch_fixture::api::Tally") && why.contains("layout"), "{why}");

    let (ir, contract) = edit("new_static");
    let why = reasons(&builder.patch(&ir, &contract, library.anchor));
    assert!(why.contains("adds static") && why.contains("ADDS"), "{why}");

    // An id that named a different member earlier in this process. Codegen
    // cannot see this case — within either interface the ids are unique, and it
    // only knows one interface — so it is the patch builder's to refuse. A
    // caller still holding the old meaning would otherwise reach the new member
    // with the old member's arguments, and nothing on the wire would notice.
    //
    // Driven from a hand-written contract because a real one cannot exhibit it
    // on demand: it needs two members whose wire facts collide under the hash,
    // which is a 1-in-2000 event across a whole bridge, not something a fixture
    // can be written to contain.
    {
        let mut ir_json: Value =
            serde_json::from_slice(&std::fs::read(&launch_contract).unwrap()).unwrap();
        let functions = ir_json["functions"].as_array_mut().unwrap();
        let victim = functions
            .iter_mut()
            .find(|f| f["name"] == "add")
            .expect("the fixture has `add`");
        // Same id, different member: exactly what a removal followed by a
        // colliding addition would produce two reloads later.
        victim["name"] = Value::from("renamed");
        let forged = tmp.join("forged-contract.json");
        std::fs::write(&forged, serde_json::to_vec(&ir_json).unwrap()).unwrap();

        // The launch build's own IR: nothing about the code differs, so the
        // refusal can only be the ledger's.
        let why = reasons(&builder.patch(&launch_ir, &forged, library.anchor));
        assert!(
            why.contains("api::renamed") && why.contains("api::add"),
            "the refusal names both meanings of the id: {why}"
        );
    }

    // Applying and resetting while other threads are calling in. Every answer
    // must be one version's or the other's, and nothing may crash: the slot is
    // an atomic pointer, and a call already inside a function runs it to the
    // end.
    let (ir, contract) = edit("body_edit");
    let reply = builder.patch(&ir, &contract, library.anchor);
    let patch = reply["file"].as_str().expect("a patch").to_string();
    let library = &library;
    let stop = std::sync::atomic::AtomicBool::new(false);
    let stop = &stop;
    let patched_seen = std::sync::atomic::AtomicBool::new(false);
    let patched_seen = &patched_seen;
    std::thread::scope(|scope| {
        let callers: Vec<_> = (0..4)
            .map(|_| {
                scope.spawn(move || {
                    let mut seen = std::collections::BTreeSet::new();
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        let answer = library.call(add, &[2, 3]);
                        if answer == 1005 {
                            patched_seen.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        seen.insert(answer);
                    }
                    seen
                })
            })
            .collect();
        // Each round waits for a caller to have run the patched code before
        // resetting, so the test cannot pass without the two versions actually
        // overlapping with calls in flight.
        for round in 0..20 {
            library.apply(Some(&patch)).unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !patched_seen.load(std::sync::atomic::Ordering::Relaxed) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "round {round}: no caller saw the patched answer"
                );
                std::hint::spin_loop();
            }
            patched_seen.store(false, std::sync::atomic::Ordering::Relaxed);
            library.apply(None).unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let seen: std::collections::BTreeSet<i64> =
            callers.into_iter().flat_map(|c| c.join().expect("a caller thread")).collect();
        assert_eq!(
            seen,
            std::collections::BTreeSet::from([5, 1005]),
            "every answer is one version's, and both were seen"
        );
    });
    assert_eq!(library.call(add, &[2, 3]), 5, "the reset is what the last apply did");

    // A patch linked for this library at another address belongs to another
    // process: loading it is refused and the running code is untouched.
    let (ir, contract) = edit("body_edit");
    let reply = builder.patch(&ir, &contract, library.anchor + 0x4000);
    assert_eq!(reply["status"], "patched", "{reply}");
    let refused = library.apply(reply["file"].as_str()).unwrap_err();
    assert!(refused.contains("different address"), "{refused}");
    assert_eq!(library.call(add, &[2, 3]), 5);
}
