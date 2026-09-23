//! frustrate runtime for bridge crates.
//!
//! Generated glue (see frustrate-codegen) calls into this crate; bridge
//! authors only see `#[frustrate::bridge]`.

// Threaded wasm is nightly by construction (-Zbuild-std +atomics); the pool's
// worker parking uses the wasm wait/notify intrinsics directly. Stable
// builds (native, single-threaded web) never activate this.
#![cfg_attr(
    all(target_family = "wasm", feature = "wasm-threads"),
    feature(stdarch_wasm_atomic_wait)
)]

pub use frustrate_macros::bridge;
pub use frustrate_macros::bridge_file;

/// The six representation keywords, spelled as types so a use site may name
/// one explicitly wherever a bridged type name may appear — `fn f(doc:
/// &Locked<Doc>)`. Each is `T` with a zero-cost identity mapping (`pub type
/// X<T> = T;`), so a body written against `Locked<Doc>` is a body written
/// against `Doc` as far as rustc is concerned — nothing to construct, nothing
/// to unwrap, no runtime representation of its own.
///
/// frustrate's own codegen sees these as source syntax, not as Rust types:
/// it parses with `syn` and never expands macros or resolves `pub type`
/// aliases, so `Locked<Doc>` is recognised by its literal spelling wherever
/// a bridged type name may appear (a parameter, a return, a struct field, a
/// `Vec`/`Option`/`HashMap` element, an `impl` block's self type — see
/// `codegen/src/parse.rs`'s `parse_path_type` and `self_type_and_claim`).
/// The wrapper resolves to the exact same `Type` the bridge would give the
/// bare name, so it changes nothing about what is bridged; what it adds is a
/// checked claim (FR0062) — `Locked<Doc>` where `Doc` is declared `#[bridge(
/// frozen)]` is refused, naming the mismatch.
///
/// Optional on a type that declares one representation, where it can only
/// restate what the declaration already says. **Required** on one that
/// declares two — `#[bridge(data, locked)] struct Doc` crosses as a value
/// class *and* a handle class, and a bare `Doc` names both and resolves to
/// neither (FR0067). The marker is how each position, and each `#[bridge]
/// impl` block, says which half it means.
pub type Data<T> = T;
/// See [`Data`].
pub type Confined<T> = T;
/// See [`Data`].
pub type Resident<T> = T;
/// See [`Data`].
pub type Frozen<T> = T;
/// See [`Data`].
pub type Locked<T> = T;
/// See [`Data`].
pub type Actor<T> = T;

#[cfg(not(target_family = "wasm"))]
pub mod actor;
pub mod codec;
pub mod deferred;
/// Rust handles to Dart objects — the dual of opaque handles. See the module
/// docs for which mirror to take.
pub mod dart;
pub mod envelope;
pub mod error;
pub mod executor;
pub mod handle;
/// Taking a patch into a running library. Present only in a library built with
/// `--cfg=frustrate_hot_patch` (`frustrate_hot_patchable`, `-c dbg`).
#[cfg(all(frustrate_hot_patch, not(target_family = "wasm")))]
pub mod hot_patch;
/// The one slot an embedder registers a value into, shared by every hook that
/// offers one. Private: a bridge sees each hook's own `register`, never the
/// primitive underneath.
mod hook;
/// Rust's `log` facade → a Dart `Stream`: register a sink and every
/// `log::info!` in the crate graph becomes an item on it.
pub mod logging;
pub mod callback;
/// The panic listener: one process-global callback told about every Rust panic
/// this runtime observes, so crash reporting can live on the Rust side.
pub mod panic;
pub mod pool;
pub mod post;
/// The host CSPRNG (`crypto.getRandomValues`), for a wasm module that has no
/// OS to ask. Wasm-only; native has `getrandom` and the module doc says why
/// reimplementing it here would be worse than absent.
#[cfg(target_family = "wasm")]
pub mod random;
/// Which thread built each `#[bridge(resident)]` object, and which isolate
/// owns it — the model's soundness check and its leak report. Native-only;
/// see the module doc for why web needs neither.
#[cfg(not(target_family = "wasm"))]
pub mod resident;
/// The async-runtime hook: lend the cooperative executor a tokio/async-std
/// context so a bridged `async fn` can `.await` that runtime's leaves.
pub mod runtime;
/// The non-parking lock every main-thread-reachable registry uses. Private:
/// it is an implementation detail of this runtime's platform constraints, not
/// something a bridge should build on.
mod spin;
/// The `locked` model's read/write lock. Private for the same reason as
/// `spin`: what a bridge names is [`handle::LockedCell`] and its guards, which
/// are re-exported there so generated glue reaches one module.
mod rwlock;
pub mod testing;
pub mod stream;

/// One lock for the tests that touch a **process global**, so they cannot run
/// beside each other.
///
/// `cargo test` runs a binary's tests on parallel threads, and several things
/// here are one per process: `log`'s logger and `logging`'s `HOOK`/`DROPPED`,
/// and `resident`'s live-object and loss registries. Two suites collide as
/// soon as one of them *emits* what the other is counting — `resident` warns
/// through the `log` facade when an isolate exits holding an object, which
/// arrives in whatever sink a logging test just installed.
///
/// Crate-wide rather than per module, because the collision is between
/// modules. A module-local lock orders a suite against itself and says nothing
/// about the suite that is actually interfering, which is how a logging
/// assertion came to fail on a record `resident` wrote.
///
/// Test-only in the strict sense: `#[cfg(test)]`, so no build that ships
/// contains it.
#[cfg(test)]
pub(crate) fn serialize_globals() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub use callback::{DartCallback, DartFunction};
pub use codec::BytesCodec;
pub use deferred::Deferred;
pub use stream::{StreamClosed, StreamSink};

/// Number of live native actor hosts. 0 on wasm, where the actor executor
/// is a Worker owned by the Dart side and this crate hosts nothing. Test
/// support for executor-leak pins; portable so bridge fixtures need no cfg.
pub fn actor_host_count() -> usize {
    #[cfg(not(target_family = "wasm"))]
    {
        actor::host_count()
    }
    #[cfg(target_family = "wasm")]
    {
        0
    }
}

// --- block-check export gating ---------------------------------------------
//
// Every `#[no_mangle]` in this crate that a wasm build would emit carries
// `#[cfg(not(frustrate_block_check))]` — the same gate codegen puts on the
// exports it emits. Under `--cfg frustrate_block_check` the only surviving
// exports are the generated `frustrate_check_block_*` roots, so lld's
// dead-code elimination, rather than a hand-written call graph, decides what
// a `#[bridge(no_block)]` body can reach. Nothing that ships sets the cfg, so
// with it off this crate is unchanged. The full rationale — in particular why
// restricting a module's exports from the link line does *not* work — lives on
// `BLOCK_CHECK_SUPPRESS` in codegen/src/emit_rust.rs, and is not repeated at
// the individual gates.
//
// Which exports need the gate is decided by "would wasm32 without the
// `wasm-threads` feature emit this symbol", the configuration the check
// artifact is built in. So the gate goes on the wasm-only exports too
// (`frustrate_alloc`, `frustrate_web_init`, `executor::frustrate_mark_actor_instance`):
// the check build *is* a wasm build, and `cfg(target_family = "wasm")` does
// not exclude it. Conversely `frustrate_init_dl`/`frustrate_exit_port` and all
// of `actor` are `cfg(not(target_family = "wasm"))`, and `pool`'s four live in
// `mod threaded` behind `feature = "wasm-threads"`; none of those exist in the
// check build, so none of them are gated.
//
// Leaving `pool`'s four ungated is not a hole waiting on a feature-on check
// build. That build is unusable for reasons of its own — `run_worker`'s wait is
// address-taken and a claimed body leaves the direct-call graph — and were
// one attempted anyway, the scanner's stray-export control runs ahead of its
// reachability rules, so the four would be named as strays before anything got
// as far as "cannot decide". Gating them would change which error arrives, not
// whether one does.

/// Free a Vec<u8> that was leased to Dart (sync return buffers and async
/// response buffers).
///
/// # Safety
///
/// Must be called exactly once per leased buffer, with the exact
/// (ptr, len, cap) triple that was leased.
///
/// `unsafe` states a contract the host has always had to honour; it changes
/// neither the exported symbol nor the calling convention, so the JS and Dart
/// callers are unaffected.
#[cfg(not(frustrate_block_check))]
#[no_mangle]
pub unsafe extern "C" fn frustrate_buffer_free(ptr: *mut u8, len: u64, cap: u64) {
    if ptr.is_null() {
        return;
    }
    unsafe {
        drop(Vec::from_raw_parts(ptr, len as usize, cap as usize));
    }
}

/// The native runtime ABI this crate speaks — the shape of the exports the
/// Dart transport calls directly, independent of any bridge's schema.
///
/// `frustrate_schema_hash` guards *generated bindings* against a stale dylib;
/// this guards the layer underneath, where a mismatched `package:frustrate`
/// would otherwise call an export with the wrong signature. Bump it whenever an
/// export's signature or contract changes, so the skew is a named error rather
/// than a renamed symbol.
///
/// 1 — `frustrate_init_dl(api_data, port) -> isolate_id | negative error`.
/// 2 — adds `frustrate_exit_port() -> Handle`, and three handshake failure
///     codes (-4, -5, -6) for the two `dart_api_dl` entry points it needs.
/// 3 — `frustrate_call_sync` answers into a caller-provided buffer:
///     `(fn_id, req, req_len, out, out_cap) -> i32` replaces the three
///     out-param pointers, and only a response too large for `out` is leased
///     back (envelope.rs, `respond_out`). Also the first ABI **web** checks:
///     this export used to be native-only.
/// 4 — adds `frustrate_call_cancel(call_id) -> u8`, the transport half of
///     `FrustrateCancelToken` (executor.rs). A new export rather than a changed
///     one, which is the same reason 2 was cut: without the bump, a
///     `package:frustrate` that knows the token would resolve the symbol
///     lazily and fail at the first `cancel()` — an `ArgumentError` from
///     `lookupFunction` on native, a missing-export throw on web, both far from
///     the skew that caused them.
///
/// Bumped rather than renamed, unlike `frustrate_init_dl`: the export whose
/// shape changed is `frustrate_call_sync`, which codegen emits per bridge
/// crate, so a rename would have to be spelled by the generator and would say
/// nothing about the runtime the bridge was built against. The version this
/// constant carries does, on both transports, before any call is dispatched.
pub const RUNTIME_ABI: u64 = 5;

/// Read [`RUNTIME_ABI`]. Checked by the Dart transport before it calls anything
/// else, so a runtime/package skew is a named error rather than a mismatched
/// call.
///
/// Exported on **wasm too** as of ABI 3, and that is the change the check was
/// invented for. Web had no runtime-ABI guard at all: `frustrate_schema_hash`
/// covers the *interface* a bridge exposes, so a module built against an older
/// runtime but the same `#[bridge]` items matched it exactly — and would then
/// have been called with the new argument list, reading a pointer where the
/// response capacity now goes and writing the response over the request. That
/// is precisely the silent corruption this constant exists to turn into a
/// named error.
#[cfg(not(frustrate_block_check))]
#[no_mangle]
pub extern "C" fn frustrate_runtime_abi() -> u64 {
    RUNTIME_ABI
}

/// Register this isolate's `RawReceivePort` — and, once per process, resolve
/// the `dart_api_dl` entry point out of `api_data`
/// (`NativeApi.initializeApiDLData`) — then return the isolate's id. Must be
/// called before any async bridge call.
///
/// The isolate stamps the returned id into the high bits of every
/// call/stream/callback id it allocates so `post::deliver` routes completions
/// back to the right isolate — see post.rs. Each isolate (and each hot restart)
/// gets a distinct id.
///
/// Returns a **negative** value if the handshake failed (unsupported
/// `DART_API_DL` major version, missing entry point, null `api_data`); the Dart
/// transport maps each to a named `StateError`. A successful id is always
/// positive, so the sign is an unambiguous discriminator.
///
/// The `_dl` suffix names the handshake, not a version: an export whose
/// signature changes gets a new name, so a stale dylib fails on the missing
/// symbol at `Frustrate.install` rather than being called with mismatched
/// arguments. [`RUNTIME_ABI`] covers the changes that do not warrant a rename.
///
/// # Safety
/// `api_data` must be the pointer Dart's `NativeApi.initializeApiDLData` yields,
/// or null; `port` must be a native port id from `SendPort.nativePort`.
#[cfg(not(target_family = "wasm"))]
#[no_mangle]
pub unsafe extern "C" fn frustrate_init_dl(api_data: *mut std::ffi::c_void, port: i64) -> i64 {
    unsafe { post::init_dl(api_data, port) }
}

/// A `SendPort` for the process-global port isolates report their own death to.
/// The Dart transport registers it once per isolate with
/// `Isolate.current.addOnExitListener(port, response: isolateId)`.
///
/// That notice is what settles a callback invocation the isolate **accepted**
/// and then died holding — the one death mode no refused post ever discovers,
/// because a waiting producer has no post of its own to learn from. It reaches
/// `post::mark_gone` like any other death, so both waiter shapes (a blocked
/// pool worker and a suspended `CallFuture`) are swept by the same code.
///
/// Returns a Dart `SendPort` through dart:ffi's `Handle`, so it must be called
/// from Dart with an isolate entered — which `Dart_NewSendPort` requires. Only
/// meaningful after `frustrate_init_dl` returned a positive id; before that
/// there is no port and this is null.
#[cfg(not(target_family = "wasm"))]
#[no_mangle]
pub extern "C" fn frustrate_exit_port() -> *mut std::ffi::c_void {
    post::exit_send_port()
}

/// Allocate `len` bytes of linear memory for the host to write a request
/// buffer (or out-param slots) into. Freed with
/// `frustrate_buffer_free(ptr, len, len)`.
///
/// **Fallible on purpose.** `vec![0u8; len]` fails through
/// `handle_alloc_error`, which does *not* run the panic hook — so an
/// exhausted heap reached Dart as `RuntimeError: unreachable` with nothing
/// attached, measured with 4 MiB entirely free. `try_reserve` puts the failure
/// back on the panic path, where the attribution shim
/// ([`frustrate_web_init`]) ships the message and the host rethrows it as a
/// `BridgePanicException` — the same treatment
/// `pool::frustrate_alloc_aligned` already gives a failed worker stack.
///
/// Still fatal, and deliberately: the host has already committed to writing a
/// request here, so there is no caller in a position to do anything else. The
/// change is that the failure says what it was.
#[cfg(target_family = "wasm")]
#[cfg(not(frustrate_block_check))]
#[no_mangle]
pub extern "C" fn frustrate_alloc(len: u64) -> *mut u8 {
    let len = len as usize;
    let mut buf: Vec<u8> = Vec::new();
    if buf.try_reserve_exact(len).is_err() {
        panic!(
            "frustrate: out of memory allocating a {len}-byte request buffer. \
             The wasm module's linear memory could not grow — on the \
             single-threaded build it declares no maximum, so this is the engine \
             or the page refusing; on the threaded build the shared memory is at \
             its declared maximum"
        );
    }
    buf.resize(len, 0);
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr
}

/// One-time wasm-side setup; the web transport must call this before any
/// bridge call.
///
/// Under today's `panic=abort` this installs the **panic-attribution shim**: a
/// panic hook that ships the message (and `file:line`) through the
/// `frustrate.panic` import, because the subsequent trap reaches the host as a
/// payload-less `RuntimeError`, and this is the only way the panic stays
/// attributable on web. The Dart-facing surface is not the import but the
/// `BridgePanicException` the host rethrows from the trapping call, which
/// reaches `PlatformDispatcher.onError` / the current zone like any other error.
///
/// The hook is also where the **panic listener** is called on web, and it is
/// the only place it can be: `panic=abort` means no `catch_unwind` anywhere in
/// this runtime ever runs, so none of the funnels that cover native
/// (`envelope::panic_envelope`, the actor `Reap` arm) exist here. Calling user
/// code from inside a panic hook is refused everywhere else in this crate —
/// a panic raised in a hook aborts, uncatchably — and is accepted here for a
/// reason specific to this build: the process is already dying, so a listener
/// that traps causes a trap the original panic was about to cause anyway. It
/// is invoked **after** the import, so a broken listener cannot take the
/// call's `BridgePanicException` attribution with it.
///
/// Forward-compat: a future `panic=unwind` web build needs none of this — the
/// executor's `catch_unwind` (executor.rs) attributes through the normal
/// response envelope exactly as native does, and the same funnels then carry
/// the listener — so the shim is gated on `cfg(panic = "abort")` and this
/// export becomes an inert no-op there. The host may keep calling it
/// unconditionally.
#[cfg(target_family = "wasm")]
#[cfg(not(frustrate_block_check))]
#[no_mangle]
pub extern "C" fn frustrate_web_init() {
    // Settle the cooperative executor here — on the main thread, before any
    // pool worker exists. See `executor::init_global`: it is what stops two
    // threads ever racing that initialisation.
    crate::executor::init_global();
    #[cfg(panic = "abort")]
    {
        #[link(wasm_import_module = "frustrate")]
        extern "C" {
            fn panic(ptr: *const u8, len: u64);
        }
        std::panic::set_hook(Box::new(|info| {
            let msg = info.to_string();
            unsafe { panic(msg.as_ptr(), msg.len() as u64) };
            // After the import, deliberately — see this function's docs.
            crate::panic::observed_in_hook(info);
        }));
    }
}
