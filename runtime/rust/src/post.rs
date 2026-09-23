//! Completion delivery for async calls.
//!
//! Native: Dart owns one `RawReceivePort` per isolate and hands frustrate its
//! native port id — plus the `dart_api_dl` function table — through
//! `frustrate_init_dl`. Worker threads deliver `[call_id, payload]` messages
//! into it with `Dart_PostCObject`.
//!
//! **Why the raw port and not `NativeCallable.listener`.** They are the same
//! transport — a listener callable marshals its arguments over a `SendPort` to
//! the isolate that created it — and differ only in what happens once that
//! isolate is gone: the callable aborts the process (`Callback invoked after it
//! has been deleted`), the port returns false.
//!
//! That difference decides it, because delivery here is scheduled by Rust
//! rather than by Dart. A producer holding a stored `StreamSink` posts from a
//! pool or actor thread long after the call that created it returned, so its
//! consumer may be gone for reasons ranging from a leaked handle to the
//! developer pressing `r`. The transport cannot tell which, so an abort is
//! wrong wherever the exit was legitimate and a silent drop is wrong wherever
//! it was a bug. A `false` is neither: [`deliver`] hands it to the producer,
//! which retires the channel and stops, and the bug case is caught by the
//! diagnostics built for it rather than by this refusal: a handle collected
//! without `dispose()` ends its channels with a `LeakedChannelError` naming the
//! type ([`crate::stream::finalize_scope`]), and `openChannelCount` /
//! `openChannelLabels` are asserted flat in teardown for the leaks no
//! collection ever reaches.
//!
//! Wasm: there is no port and no isolate — completion is a wasm import
//! (`frustrate.post`) the embedding JS provides at instantiation, and delivery
//! happens synchronously during the `frustrate_call_async` export (the job runs
//! inline; see pool.rs). The page is always alive, so its `deliver` always
//! reports success.
//!
//! Buffer ownership differs by platform for the same reason. Wasm leases the
//! buffer to the host, which copies and calls `frustrate_buffer_free`. Native
//! hands the payload to `Dart_PostCObject`, which **copies it into the message**
//! — so the `Vec` is dropped here and there is no lease outstanding. That is
//! what makes a refused post leak no *bytes*: the VM owns the copy and frees it
//! with the undelivered message, and on the refusal path there is no copy at
//! all.
//!
//! The bytes are not the whole payload, though. A framed payload that carries
//! an opaque handle registered the Rust object while encoding, which is before
//! anything here can know whether the answer will be taken, so the buffer
//! arrives paired with a ledger of what it minted (`codec::Minted`).
//! [`deliver`] discharges that ledger on the branch it lands on — handed over
//! when the port accepted, freed when it refused — which is why nothing that
//! calls [`respond`] has to know the ledger exists.

#[cfg(not(target_family = "wasm"))]
pub use native::{deliver, exit_send_port, init_dl, respond, ISOLATE_SHIFT};
// `not(wasm)` as well as `test`, like the `init` seam below: `native` does not
// exist on wasm, so the bare `cfg(test)` made `--target wasm32 --tests` fail to
// resolve this import. Nothing in the shipped build changed.
#[cfg(all(test, not(target_family = "wasm")))]
pub use native::mark_gone_for_test;

/// The test-only capture seam: the crate's unit tests exercise every producer
/// path without a Dart VM, so they register a raw C callback in place of a port
/// (see [`native::init`]).
#[cfg(all(test, not(target_family = "wasm")))]
pub(crate) use native::init;

/// The crate's lock for process-global test state. Three things take it, and
/// the third is the one that keeps being missed.
///
/// 1. **Registering a post callback.** Tests exercise the single-isolate fast
///    path (they register under isolate 0), which is last-write-wins, so
///    concurrent post-using tests would steal each other's completions. Every
///    such test takes this lock first and re-registers its own callback via
///    [`native::init`].
/// 2. **Swapping the `std` panic hook.** There is one per process, and the
///    save/restore pairs tests use would interleave into a stranded hook.
/// 3. **Raising a deliberate panic that can reach a funnel** —
///    `envelope::run`, `executor::drain_one`, or `actor`'s `Reap` arm. Such a
///    panic is reported to whatever [`crate::panic`] listener is registered,
///    and that listener belongs to whichever test registered it. Not only a
///    literal `panic!` counts: an `unwrap` meant to fail, or a call to
///    something that panics by contract, reaches the funnel the same way, and
///    that spelling is what an audit by grep misses.
///
/// The failure is never local: it is another test failing, in another module,
/// on one run in a hundred.
///
/// A panic that reaches **no** funnel needs no lock. It used to, because
/// `panic::tests::the_hook_arm_reports_the_same_report` made the funnel the
/// process `std` hook for the length of its window and then asserted its log
/// held exactly one entry — an assertion about every panic in the binary,
/// which no test can own. That test asserts the presence of its own report
/// now, so the nine `#[should_panic]` tests in `codec` and `envelope` are free
/// to panic whenever they are scheduled.
#[cfg(all(test, not(target_family = "wasm")))]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    // A panicking test poisons the lock; the serialization it provides is
    // unaffected, so keep going.
    LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Native completion routing across isolates.
///
/// Each Dart isolate/runtime registers its OWN `RawReceivePort` through
/// `frustrate_init_dl`, which allocates the isolate a small id and returns it.
/// The isolate stamps that id into the high [`ISOLATE_SHIFT`]-and-up bits of
/// every call/stream/callback id it allocates, so a `call_id` is globally
/// unique and self-describing: it carries the identity of the isolate that owns
/// its completion. `deliver` extracts that isolate id and dispatches to the
/// right port — a second isolate can no longer steal the first's completions
/// (the bug that made this a routed registry instead of a single global slot).
///
/// Layout of a `call_id` (u64): bits `[ISOLATE_SHIFT, 64)` are the isolate
/// id, bits `[0, ISOLATE_SHIFT)` are that isolate's per-call sequence. With
/// `ISOLATE_SHIFT = 48` that is 65536 distinct isolates and 2^48 (~2.8e14)
/// calls per isolate — both effectively unreachable, including across hot
/// restarts (each restart is a fresh isolate and consumes one id; entries
/// are never removed, so ids are not reused). Bit 63 is therefore never set,
/// which is why the id survives the trip through Dart's signed `int`.
///
/// Fast path: the overwhelmingly common case is exactly one isolate. While
/// that holds, `deliver` is a single relaxed atomic load of `SOLE_PORT` — no
/// lock, no map lookup. A second registration flips `ROUTED` (monotonically),
/// after which `deliver` consults the `RwLock<HashMap>`; reads there are
/// concurrent across the worker threads posting completions.
#[cfg(not(target_family = "wasm"))]
mod native {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
    use std::sync::{OnceLock, RwLock};

    /// Bit position separating the isolate id (above) from the per-isolate
    /// call sequence (below) inside a `call_id`. Kept in sync with the Dart
    /// transport, which stamps `isolate_id << ISOLATE_SHIFT` into every id it
    /// allocates.
    pub const ISOLATE_SHIFT: u32 = 48;

    /// Where one isolate's completions go.
    #[derive(Clone, Copy)]
    enum Target {
        /// The isolate's `RawReceivePort`, by native port id.
        Port(i64),
        /// A raw C callback. Test-only: the crate's unit tests capture posted
        /// envelopes without a Dart VM (see [`init`]).
        #[cfg(test)]
        Callback(PostCallback),
        /// The isolate's port refused a message, so the isolate is gone for
        /// good. Latched: ports are never reopened, and a tombstone keeps
        /// later posts from re-probing a dead port.
        Gone,
    }

    /// The C signature a test callback is invoked with — the leased-buffer
    /// triple, matching what the wasm `post` import receives.
    #[cfg(test)]
    pub type PostCallback = extern "C" fn(call_id: u64, ptr: *mut u8, len: u64, cap: u64);

    /// Isolate id allocator. Starts at 1 so isolate 0 is reserved for the
    /// single-isolate/test registrar ([`init`]); real isolates registered
    /// through [`register_port`] get 1, 2, 3, …
    static NEXT_ISOLATE: AtomicU64 = AtomicU64::new(1);

    /// The sole isolate's port, cached for the single-isolate fast path.
    /// Meaningful only while `ROUTED` is false; 0 means "not cached".
    static SOLE_PORT: AtomicI64 = AtomicI64::new(0);

    /// Set once a second isolate registers: from then on `deliver` must
    /// consult the registry rather than `SOLE_PORT`. Monotonic — entries are
    /// never removed, so this never flips back.
    static ROUTED: AtomicBool = AtomicBool::new(false);

    fn registry() -> &'static RwLock<HashMap<u32, Target>> {
        static REGISTRY: OnceLock<RwLock<HashMap<u32, Target>>> = OnceLock::new();
        REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
    }

    /// Resolve the `dart_api_dl` entry point and register this isolate's port.
    /// Returns the isolate id, or a negative [`dl`] error code — the caller
    /// (`frustrate_init_dl`) hands either straight back to Dart, which turns a
    /// negative into a named `StateError`.
    ///
    /// # Safety
    /// `api_data` must be Dart's `NativeApi.initializeApiDLData` pointer, or null.
    pub unsafe fn init_dl(api_data: *mut std::ffi::c_void, port: i64) -> i64 {
        if let Err(code) = unsafe { dl::init(api_data) } {
            return code;
        }
        if let Err(code) = init_exit_port() {
            return code;
        }
        register_port(port) as i64
    }

    /// The process-global port every isolate reports its own death to.
    ///
    /// One port for the whole process, not one per isolate: the notice names
    /// its isolate in the payload, and a VM-global port is not owned by any
    /// isolate — so it survives the death it is reporting, and it enters no
    /// isolate's keep-alive computation.
    static EXIT_PORT: OnceLock<i64> = OnceLock::new();

    /// Create [`EXIT_PORT`]. Idempotent — every isolate's handshake calls this,
    /// and only the first creates anything.
    fn init_exit_port() -> Result<(), i64> {
        let port = *EXIT_PORT.get_or_init(|| {
            // `handle_concurrently: false`: `true` only widens the contract
            // this handler has to satisfy.
            dl::new_native_port(c"frustrate.exit", handle_exit, false)
        });
        if port == 0 {
            Err(dl::ERR_EXIT_PORT)
        } else {
            Ok(())
        }
    }

    /// A `SendPort` for [`EXIT_PORT`], for Dart to hand `addOnExitListener`.
    /// Null before the handshake has run, which no caller can reach: the Dart
    /// transport asks only after `frustrate_init_dl` returned a positive id.
    pub fn exit_send_port() -> dl::DartHandle {
        match EXIT_PORT.get() {
            Some(&port) if port != 0 => dl::new_send_port(port),
            _ => std::ptr::null_mut(),
        }
    }

    /// An isolate died. Runs on a VM thread with no isolate entered.
    ///
    /// This is the trigger [`mark_gone`] always needed and never had. Until
    /// now its only caller was a *refused* post, so an isolate that **accepted**
    /// an invocation and then died — the ordinary shape of `Isolate.kill`,
    /// `Isolate.exit`, and Flutter hot restart — was discovered only if some
    /// unrelated producer happened to post to it later. A parked waiter is by
    /// definition not posting, which is why the blocking path used to
    /// manufacture posts of its own and why the awaited path could not.
    ///
    /// **Must not block and must not panic.** A blocking handler stalls every
    /// other isolate's notice — one that blocks serializes every notice behind
    /// it — and a panic unwinding across this FFI
    /// boundary is undefined. `mark_gone` satisfies both: it takes two short
    /// locks poison-tolerantly, and settles waiters after releasing them.
    unsafe extern "C" fn handle_exit(_dest: i64, message: dl::Message) {
        if let Some(isolate) = dl::read_u32(message) {
            mark_gone(isolate);
        }
    }

    /// Register an isolate's port and return its id. The Dart runtime stamps
    /// the id into every id it allocates.
    ///
    /// Re-registration happens on Dart hot restart, which creates a fresh
    /// isolate with a fresh port and so a fresh id. Calls in flight across a
    /// hot restart carry the old isolate's id and route to the old port, which
    /// is closed — so they are refused rather than delivered, which is exactly
    /// right: nothing in the new isolate is waiting for them.
    fn register_port(port: i64) -> u64 {
        let id = NEXT_ISOLATE.fetch_add(1, Ordering::SeqCst);
        registry().write().unwrap().insert(id as u32, Target::Port(port));
        if id == 1 {
            // First isolate: install the fast path.
            SOLE_PORT.store(port, Ordering::SeqCst);
        } else {
            // A second isolate exists; every isolate's id is now stamped into
            // its call_ids, so deliver must route through the map. This flips
            // strictly before Dart is told this isolate's id (register returns
            // first), hence before any id carrying it can exist — so no
            // deliver ever reads a stale SOLE_PORT for a non-sole isolate.
            ROUTED.store(true, Ordering::SeqCst);
        }
        id
    }

    /// Register a callback under isolate 0 for the single-isolate fast path.
    /// Used by the crate's unit tests, which post with un-stamped (isolate 0)
    /// ids and have no Dart VM to own a port; production goes through
    /// [`init_dl`] / `frustrate_init_dl`.
    #[cfg(test)]
    pub fn init(cb: PostCallback) {
        registry().write().unwrap().insert(0, Target::Callback(cb));
        // Clear the port fast path so `target_for` reaches the map, and reset
        // routing so isolate 0 is what an un-stamped id resolves to.
        SOLE_PORT.store(0, Ordering::SeqCst);
        ROUTED.store(false, Ordering::SeqCst);
    }

    /// Drive the isolate-death notice without a VM.
    ///
    /// The real trigger is [`handle_exit`], which the Dart runtime wires to
    /// `Isolate.addOnExitListener` — unreachable from a unit test, and the
    /// reason the reap-on-death wiring would otherwise be the one part of
    /// this path nothing covers. Same seam shape as [`init`] and
    /// [`register_callback`] above, for the same reason.
    #[cfg(test)]
    pub fn mark_gone_for_test(isolate: u32) {
        mark_gone(isolate);
    }

    /// Register a callback under a fresh isolate id — the test-only twin of
    /// [`register_port`], for asserting cross-isolate routing without a VM.
    #[cfg(test)]
    pub fn register_callback(cb: PostCallback) -> u64 {
        let id = NEXT_ISOLATE.fetch_add(1, Ordering::SeqCst);
        registry()
            .write()
            .unwrap()
            .insert(id as u32, Target::Callback(cb));
        ROUTED.store(true, Ordering::SeqCst);
        id
    }

    fn target_for(call_id: u64) -> Option<Target> {
        if !ROUTED.load(Ordering::SeqCst) {
            let port = SOLE_PORT.load(Ordering::SeqCst);
            if port != 0 {
                return Some(Target::Port(port));
            }
        }
        let isolate = (call_id >> ISOLATE_SHIFT) as u32;
        registry().read().unwrap().get(&isolate).copied()
    }

    /// Tombstone `isolate`: it is gone.
    ///
    /// **The single entry point for isolate death**, with two triggers. The
    /// isolate told us on its way out ([`handle_exit`], promptly, and the only
    /// one that sees a death nobody posted into), or a post to it was refused
    /// ([`deliver`], which still catches the exits that run no exit path at all
    /// — `dart:io exit()`, a crash, a `SIGKILL`).
    ///
    /// Also fails every callback invocation still awaiting a response from it —
    /// otherwise a worker thread blocked in `DartFunction::call`, or a task
    /// suspended in `call_async`, waits forever for an isolate that will never
    /// answer.
    ///
    /// Poison-tolerant on both locks, because [`handle_exit`] calls this from
    /// an `extern "C"` frame where a panic would unwind across the FFI
    /// boundary. Nothing holds either lock across user code, so a poisoned one
    /// is not evidence the map is torn.
    fn mark_gone(isolate: u32) {
        registry()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(isolate, Target::Gone);
        // The fast path caches the sole isolate's port; clear it so `target_for`
        // consults the map and finds the tombstone. Harmless when `ROUTED` (the
        // cache is already ignored), and a hot restart's fresh registration
        // sets `ROUTED`, so the fast path simply stays off for the rest of the
        // process — a dev-time-only cost.
        SOLE_PORT.store(0, Ordering::SeqCst);
        crate::callback::fail_invocations_for(isolate);
        // Release the executors that isolate owned. Its Dart-side finalizers
        // died with it, so this is the only thing left that can.
        crate::actor::reap_isolate(isolate);
        // Its resident objects it *cannot* release: a resident has no `Send`
        // bound, so only the thread that built it may run its `Drop`, and that
        // thread is not this one. Nothing is freed here — the sweep names what
        // was lost so the leak is legible rather than silent.
        crate::resident::reap_isolate(isolate);
    }

    /// Deliver a framed payload to Dart, reporting whether the consumer took
    /// it. `false` means the owning isolate is gone — it exited, was killed, or
    /// the app hot restarted — and nothing will ever arrive for that id again.
    ///
    /// Producers that can act on the answer use this (`stream::post_method`
    /// retires the channel so `add` returns false and the producer stops);
    /// fire-and-forget completion paths use [`respond`].
    ///
    /// **Takes the writer, not its bytes, and that is where the reclaim lives.**
    /// A payload that carries a handle registered the Rust object while
    /// encoding, which is before anyone can know whether the answer will be
    /// taken; refusing it here and dropping the bytes would leave that object
    /// alive with its only Dart wrapper never built. This is the one frame that
    /// learns the answer, so this is the frame that discharges the ledger —
    /// which is what makes every caller of [`respond`] correct without any of
    /// them saying anything.
    pub fn deliver(call_id: u64, reply: crate::codec::FramedWriter) -> bool {
        let (payload, minted) = reply.into_parts();
        if post_bytes(call_id, payload) {
            minted.delivered();
            true
        } else {
            minted.reclaim();
            false
        }
    }

    /// Put the bytes on the wire, reporting whether the consumer took them.
    /// The ledger half is [`deliver`]'s, split out only so [`respond`] can
    /// guard the reclaim without also guarding this.
    fn post_bytes(call_id: u64, payload: Vec<u8>) -> bool {
        match target_for(call_id) {
            Some(Target::Port(port)) => {
                if dl::post_pair(port, call_id, &payload) {
                    return true;
                }
                mark_gone((call_id >> ISOLATE_SHIFT) as u32);
                false
            }
            // Already tombstoned by an earlier refusal: skip the syscall.
            Some(Target::Gone) => false,
            #[cfg(test)]
            Some(Target::Callback(cb)) => {
                // The callback shape is the leased-buffer one, so hand over
                // ownership exactly as the wasm import does.
                let mut payload = payload;
                let ptr = payload.as_mut_ptr();
                let len = payload.len() as u64;
                let cap = payload.capacity() as u64;
                std::mem::forget(payload);
                cb(call_id, ptr, len, cap);
                true
            }
            None => panic!(
                "frustrate: frustrate_init_dl was not called before an async bridge call. \
                 If this is a unit test, build the sink with \
                 `frustrate::testing::stream()` — a hand-made StreamSink has no \
                 registered consumer and reaches here on its first add, or on the \
                 drop that retires it"
            ),
        }
    }

    /// Deliver an async response and discard the outcome — the fire-and-forget
    /// form, for completion paths with no producer to inform (the pool, actor
    /// hosts, the executor's completion hook, and every generated glue site).
    ///
    /// Deliberately returns `()`: generated code emits `post::respond(...)` in
    /// statement position, so this signature is part of the codegen contract.
    /// "Discard the outcome" is the *report*, not the payload: a dead isolate is
    /// latched and a refused reply's handles are reclaimed, both inside
    /// [`deliver`].
    ///
    /// **The reclaim runs a user `Drop`, and the callers whose reply can carry a
    /// ledger have no frame to unwind into.** Those are the two that answer a
    /// dispatched call from a runtime-owned loop taking its next job from a bare
    /// `recv`: a pool worker (pool.rs) and an actor host thread (actor.rs),
    /// neither of which replenishes. A panicking destructor freeing a refused
    /// reply's handle would take that thread with it — and on an actor it would
    /// also wedge every later call to the host, whose `Sender` stays in the map.
    /// So the reclaim is caught and reported through the same funnel the actor's
    /// `Reap` arm uses for the same hazard, and the thread continues. What is
    /// lost is the rest of that ledger, which the unwind carries past its own
    /// reclaim — the cost a panicking encoder already pays (`codec::Minted`).
    ///
    /// The other callers post envelopes that mint nothing and so never reach the
    /// catch: the stream terminals (`stream::Inner`), the dead-host answer in
    /// `actor::submit`, generated glue's outer panic catch, and the executor's
    /// completion hook — which is itself already inside `drain_one`'s.
    ///
    /// The *post* is deliberately outside the catch: the only thing that panics
    /// there is the unregistered-runtime bridge bug, which must stay as loud
    /// here as it is on the channel door.
    ///
    /// [`deliver`] gets no such catch: it is called from inside a user body
    /// (`StreamSink::add`, `DartFunction::call`), where a destructor's panic is
    /// the enclosing call's own and must reach its envelope.
    pub fn respond(call_id: u64, reply: crate::codec::FramedWriter) {
        let (payload, minted) = reply.into_parts();
        if post_bytes(call_id, payload) {
            minted.delivered();
            return;
        }
        if let Err(payload) =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| minted.reclaim()))
        {
            crate::panic::observed(crate::panic::message(&*payload));
        }
    }

    /// The slice of `dart_api_dl` frustrate needs, mirrored by hand.
    ///
    /// This is the price of the module-level choice above, and the only place
    /// the codebase mirrors headers it does not own. Three things bound the
    /// risk:
    ///
    /// - The blob layout (`internal/dart_api_dl_impl.h`) is guarded by
    ///   `DART_API_DL_MAJOR_VERSION`, which [`init`] checks and refuses to
    ///   proceed past. Entry lookup is by **name**, never by index.
    /// - `Dart_CObject` (`dart_native_api.h`) carries no separate version, so
    ///   its layout is asserted statically below, at compile time.
    /// - Every async completion posts through here, so a layout error cannot
    ///   lurk: it surfaces on the first call of the first test.
    mod dl {
        use std::ffi::{c_char, c_int, c_void, CStr};
        use std::sync::OnceLock;

        /// `DART_API_DL_MAJOR_VERSION` this mirror was written against.
        const SUPPORTED_MAJOR: c_int = 2;

        /// Negative results of [`init`], returned to Dart through
        /// `frustrate_init_dl` and mapped to named errors there.
        pub const ERR_NULL_API_DATA: i64 = -1;
        pub const ERR_UNSUPPORTED_MAJOR: i64 = -2;
        pub const ERR_SYMBOL_MISSING: i64 = -3;
        pub const ERR_NO_NEW_NATIVE_PORT: i64 = -4;
        pub const ERR_NO_NEW_SEND_PORT: i64 = -5;
        /// `Dart_NewNativePort` returned `ILLEGAL_PORT`.
        pub const ERR_EXIT_PORT: i64 = -6;

        #[repr(C)]
        struct DartApiEntry {
            name: *const c_char,
            function: Option<unsafe extern "C" fn()>,
        }

        #[repr(C)]
        struct DartApi {
            major: c_int,
            minor: c_int,
            functions: *const DartApiEntry,
        }

        /// `Dart_CObject_Type` discriminants we construct or read.
        /// `K_INT32` is read-only: it is what the VM tags an isolate id with on
        /// the exit-notice path (see [`read_u32`]).
        const K_INT32: i32 = 2;
        const K_INT64: i32 = 3;
        const K_ARRAY: i32 = 6;
        const K_TYPED_DATA: i32 = 7;
        /// `Dart_TypedData_kUint8` — a different enum from the tags above.
        const K_UINT8: i32 = 2;

        #[repr(C)]
        #[derive(Copy, Clone)]
        struct AsTypedData {
            ty: i32,
            length: isize,
            values: *const u8,
        }

        #[repr(C)]
        #[derive(Copy, Clone)]
        struct AsArray {
            length: isize,
            values: *mut *mut CObject,
        }

        #[repr(C)]
        union CValue {
            as_int32: i32,
            as_int64: i64,
            as_array: AsArray,
            as_typed_data: AsTypedData,
            /// Sizes the union to the real one's largest variant
            /// (`as_external_typed_data`: a type tag, an `intptr_t`, and three
            /// pointers). The VM reads only the member the `ty` tag selects, but
            /// `CObject` must still be the size the VM strides by when it walks
            /// an array's elements. It is also the field every `CObject` is
            /// initialized through, so no uninitialized byte is ever handed
            /// across the FFI boundary.
            _size: [u64; 5],
        }

        /// Public only so [`Message`] can name it; the fields stay private, so
        /// the layout is still knowledge this module alone holds.
        #[repr(C)]
        pub struct CObject {
            ty: i32,
            value: CValue,
        }

        // The mirror is sound only if it agrees with the C layout. On a 64-bit
        // target `Dart_CObject` is a 4-byte tag, 4 bytes of padding, then a
        // 40-byte union: 48 bytes, 8-aligned.
        const _: () = assert!(std::mem::size_of::<CObject>() == 48);
        const _: () = assert!(std::mem::align_of::<CObject>() == 8);
        const _: () = assert!(std::mem::offset_of!(CObject, value) == 8);
        // `Dart_TypedData_Type` is a C enum (4 bytes) followed by an
        // `intptr_t`, so the padding between them is the one place a naive
        // mirror goes wrong.
        const _: () = assert!(std::mem::offset_of!(AsTypedData, length) == 8);
        const _: () = assert!(std::mem::offset_of!(AsTypedData, values) == 16);
        const _: () = assert!(std::mem::offset_of!(AsArray, values) == 8);

        type PostCObject = unsafe extern "C" fn(port: i64, message: *mut CObject) -> bool;
        type NewNativePort = unsafe extern "C" fn(
            name: *const c_char,
            handler: NativeMessageHandler,
            handle_concurrently: bool,
        ) -> i64;
        type NewSendPort = unsafe extern "C" fn(port: i64) -> DartHandle;

        /// `Dart_NativeMessageHandler_DL`.
        pub type NativeMessageHandler = unsafe extern "C" fn(dest: i64, message: Message);

        /// A message the VM hands a native-port handler. Opaque outside this
        /// module — the handler passes it straight back to [`read_u32`], so
        /// only the mirror knows the layout.
        pub type Message = *mut CObject;

        /// `Dart_Handle`. Only ever passed straight through to Dart as an FFI
        /// `Handle` return; frustrate never inspects one.
        pub type DartHandle = *mut c_void;

        /// The three entry points frustrate resolves, set as one unit.
        ///
        /// **One cell, not three.** [`init`] early-returns once it has
        /// resolved, so independent cells would let a first isolate that
        /// resolved some-but-not-all be followed by a second that skips the
        /// check entirely and reaches a null pointer — turning "a broken SDK is
        /// loud at init" into "loud exactly once". Resolving atomically is what
        /// makes the negative return codes mean what they say.
        struct Api {
            post: PostCObject,
            new_native_port: NewNativePort,
            new_send_port: NewSendPort,
        }

        static API: OnceLock<Api> = OnceLock::new();

        /// Resolve frustrate's slice of `dart_api_dl` from the table behind
        /// `NativeApi.initializeApiDLData`. Idempotent: the table is
        /// process-global but every isolate calls `frustrate_init_dl`.
        ///
        /// # Safety
        /// `api_data` must be Dart's `NativeApi.initializeApiDLData` pointer, or null.
        pub unsafe fn init(api_data: *mut c_void) -> Result<(), i64> {
            if API.get().is_some() {
                return Ok(());
            }
            if api_data.is_null() {
                return Err(ERR_NULL_API_DATA);
            }
            let (mut post, mut new_native_port, mut new_send_port) = (None, None, None);
            // Safety: Dart guarantees `initializeApiDLData` points at a
            // `DartApi` whose `functions` array is terminated by a null name.
            unsafe {
                let api = &*(api_data as *const DartApi);
                if api.major != SUPPORTED_MAJOR {
                    return Err(ERR_UNSUPPORTED_MAJOR);
                }
                let mut entry = api.functions;
                while !(*entry).name.is_null() {
                    // Table entries carry the bare symbol name — the `_DL`
                    // suffix belongs to the typedefs, not to the table.
                    match CStr::from_ptr((*entry).name).to_bytes() {
                        b"Dart_PostCObject" => post = (*entry).function,
                        b"Dart_NewNativePort" => new_native_port = (*entry).function,
                        b"Dart_NewSendPort" => new_send_port = (*entry).function,
                        _ => {}
                    }
                    entry = entry.add(1);
                }
                let post = post.ok_or(ERR_SYMBOL_MISSING)?;
                let new_native_port = new_native_port.ok_or(ERR_NO_NEW_NATIVE_PORT)?;
                let new_send_port = new_send_port.ok_or(ERR_NO_NEW_SEND_PORT)?;
                // Safety: the table pairs each name with a pointer to a
                // function of exactly that name's signature.
                let _ = API.set(Api {
                    post: std::mem::transmute::<unsafe extern "C" fn(), PostCObject>(post),
                    new_native_port: std::mem::transmute::<unsafe extern "C" fn(), NewNativePort>(
                        new_native_port,
                    ),
                    new_send_port: std::mem::transmute::<unsafe extern "C" fn(), NewSendPort>(
                        new_send_port,
                    ),
                });
            }
            Ok(())
        }

        /// `Dart_NewNativePort`. Returns 0 (`ILLEGAL_PORT`) on failure.
        ///
        /// Callable from **any** thread — `dart_api_dl.h` groups the
        /// `dart_native_api.h` symbols under exactly that guarantee — so the
        /// handshake may create the port without an entered isolate.
        pub fn new_native_port(
            name: &CStr,
            handler: NativeMessageHandler,
            handle_concurrently: bool,
        ) -> i64 {
            let Some(api) = API.get() else { return 0 };
            // Safety: `name` is nul-terminated and `handler` has the signature
            // the typedef names.
            unsafe { (api.new_native_port)(name.as_ptr(), handler, handle_concurrently) }
        }

        /// `Dart_NewSendPort`. **Dart threads only** — `dart_api_dl.h` groups
        /// the `dart_api.h` symbols under that restriction, so the one caller
        /// is an FFI export Dart invokes, never the native-port handler (which
        /// runs on a VM thread with no isolate entered).
        pub fn new_send_port(port: i64) -> DartHandle {
            let Some(api) = API.get() else {
                return std::ptr::null_mut();
            };
            // Safety: called from Dart, so an isolate is entered.
            unsafe { (api.new_send_port)(port) }
        }

        /// Read a `u32` out of a posted message, accepting **both** integer
        /// tags.
        ///
        /// Load-bearing: Dart posts a value `< 2^31` as `kInt32` and `>= 2^31`
        /// as `kInt64`, so a reader that knows only one of them is wrong for
        /// half the range. Isolate ids start at 1 and climb, so the boundary is
        /// reachable by a long-lived process, not just in theory.
        pub fn read_u32(message: Message) -> Option<u32> {
            if message.is_null() {
                return None;
            }
            // Safety: the VM hands a handler a fully initialized `Dart_CObject`
            // whose `ty` selects the live union member.
            unsafe {
                match (*message).ty {
                    K_INT32 => u32::try_from((*message).value.as_int32).ok(),
                    K_INT64 => u32::try_from((*message).value.as_int64).ok(),
                    _ => None,
                }
            }
        }

        /// Post `[call_id, payload]` to `port`. False means the port is closed
        /// or its isolate is gone; the VM copies `payload` on success, so the
        /// caller's buffer is free either way.
        pub fn post_pair(port: i64, call_id: u64, payload: &[u8]) -> bool {
            let Some(api) = API.get() else {
                // Unreachable in production: a registered port implies a
                // resolved entry point, because `init_dl` resolves first.
                return false;
            };
            let post = api.post;
            let mut id = CObject {
                ty: K_INT64,
                value: CValue { _size: [0; 5] },
            };
            id.value.as_int64 = call_id as i64;
            let mut bytes = CObject {
                ty: K_TYPED_DATA,
                value: CValue { _size: [0; 5] },
            };
            bytes.value.as_typed_data = AsTypedData {
                ty: K_UINT8,
                length: payload.len() as isize,
                values: payload.as_ptr(),
            };
            let mut items: [*mut CObject; 2] = [&mut id, &mut bytes];
            let mut message = CObject {
                ty: K_ARRAY,
                value: CValue { _size: [0; 5] },
            };
            message.value.as_array = AsArray {
                length: 2,
                values: items.as_mut_ptr(),
            };
            // Safety: `message` is a fully initialized array of two fully
            // initialized elements, and the VM only reads it.
            unsafe { post(port, &mut message) }
        }

        #[cfg(test)]
        mod tests {
            use super::*;

            /// Build the `Dart_CObject` the VM would post for an integer with
            /// the given tag. In here rather than in `native::tests` because
            /// the layout is this module's secret.
            fn tagged(ty: i32, as_int64: i64) -> CObject {
                let mut o = CObject {
                    ty,
                    value: CValue { _size: [0; 5] },
                };
                // Both integer members alias the same offset; writing the wide
                // one and reading the narrow one is how the VM's own union
                // behaves for a value that fits.
                o.value.as_int64 = as_int64;
                o
            }

            /// The straddle, which is the whole reason this reader exists:
            /// Dart tags `< 2^31` as `kInt32` and `>= 2^31` as `kInt64`, so a
            /// handler that knows one tag is wrong for half the id space.
            #[test]
            fn an_isolate_id_is_read_through_either_integer_tag() {
                let mut small = tagged(K_INT32, 7);
                assert_eq!(read_u32(&mut small), Some(7));

                let mut big = tagged(K_INT64, 3_000_000_000);
                assert_eq!(read_u32(&mut big), Some(3_000_000_000));

                // The same id under the other tag: `kInt64` carries small
                // values too (the VM picks the narrowest, but the reader must
                // not depend on that choice).
                let mut small_wide = tagged(K_INT64, 7);
                assert_eq!(read_u32(&mut small_wide), Some(7));
            }

            /// Nothing else is an isolate id. A malformed or unexpected message
            /// must be ignored, never guessed at — `handle_exit` would
            /// otherwise tombstone an isolate chosen by reading garbage.
            #[test]
            fn a_message_that_is_not_a_u32_is_refused() {
                assert_eq!(read_u32(std::ptr::null_mut()), None);
                // kNull, kBool, kDouble, kString, kArray, kTypedData.
                for ty in [0, 1, 4, 5, K_ARRAY, K_TYPED_DATA] {
                    let mut o = tagged(ty, 7);
                    assert_eq!(read_u32(&mut o), None, "tag {ty} must not decode");
                }
                // In range for i64, out of range for u32, on both tags.
                let mut negative = tagged(K_INT32, -1);
                assert_eq!(read_u32(&mut negative), None);
                let mut too_wide = tagged(K_INT64, i64::from(u32::MAX) + 1);
                assert_eq!(read_u32(&mut too_wide), None);
            }
        }

    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::codec::FramedWriter;
        use std::sync::Mutex;

        // Two independent "isolates" register their own callbacks; a call_id
        // stamped for each — with IDENTICAL low bits — must reach only its
        // owner. This is the cross-isolate misdelivery the routed registry
        // exists to prevent.
        #[test]
        fn deliver_routes_by_isolate_tag() {
            let _serial = crate::post::test_lock();

            static SEEN_A: Mutex<Vec<u64>> = Mutex::new(Vec::new());
            static SEEN_B: Mutex<Vec<u64>> = Mutex::new(Vec::new());
            extern "C" fn record_a(call_id: u64, ptr: *mut u8, len: u64, cap: u64) {
                drop(unsafe { Vec::from_raw_parts(ptr, len as usize, cap as usize) });
                SEEN_A.lock().unwrap().push(call_id);
            }
            extern "C" fn record_b(call_id: u64, ptr: *mut u8, len: u64, cap: u64) {
                drop(unsafe { Vec::from_raw_parts(ptr, len as usize, cap as usize) });
                SEEN_B.lock().unwrap().push(call_id);
            }

            let a = register_callback(record_a);
            let b = register_callback(record_b);
            assert_ne!(a, b);
            assert!(ROUTED.load(Ordering::SeqCst), "two isolates must route via the map");

            // Same low bits (5), different isolate tags: the exact collision
            // that misdelivered before.
            let call_a = (a << ISOLATE_SHIFT) | 5;
            let call_b = (b << ISOLATE_SHIFT) | 5;
            assert!(deliver(call_a, FramedWriter::from_bytes(vec![0u8])));
            assert!(deliver(call_b, FramedWriter::from_bytes(vec![0u8])));

            assert_eq!(*SEEN_A.lock().unwrap(), vec![call_a]);
            assert_eq!(*SEEN_B.lock().unwrap(), vec![call_b]);

            // Tidy up so a later fast-path test isn't confused by these entries.
            registry().write().unwrap().remove(&(a as u32));
            registry().write().unwrap().remove(&(b as u32));
        }

        /// A tombstoned isolate is refused without touching its port, and the
        /// refusal is what `stream::post_method` turns into a stopped producer.
        #[test]
        fn a_gone_isolate_is_refused_and_stays_refused() {
            let _serial = crate::post::test_lock();

            static HITS: Mutex<usize> = Mutex::new(0);
            extern "C" fn count(_id: u64, ptr: *mut u8, len: u64, cap: u64) {
                drop(unsafe { Vec::from_raw_parts(ptr, len as usize, cap as usize) });
                *HITS.lock().unwrap() += 1;
            }

            let iso = register_callback(count);
            let call = (iso << ISOLATE_SHIFT) | 7;
            assert!(deliver(call, FramedWriter::from_bytes(vec![1u8])), "a live target takes the message");
            assert_eq!(*HITS.lock().unwrap(), 1);

            // Simulate the port refusing: this is what `deliver` does on a
            // false return from `Dart_PostCObject`.
            mark_gone(iso as u32);
            assert!(!deliver(call, FramedWriter::from_bytes(vec![2u8])), "a gone isolate is refused");
            assert!(!deliver(call, FramedWriter::from_bytes(vec![3u8])), "and the refusal is latched");
            assert_eq!(
                *HITS.lock().unwrap(),
                1,
                "nothing is delivered to a gone isolate"
            );

            registry().write().unwrap().remove(&(iso as u32));
        }

        /// A reply refused by a gone isolate gives back the handles it minted.
        ///
        /// The encode registers the object *before* the post is attempted —
        /// that is the only order there is, since the handle has to be on the
        /// wire — so a refusal that only dropped the bytes would leave a Rust
        /// object alive with its only Dart wrapper never built and nothing
        /// left holding the pointer. The mint is modelled the way the channel
        /// tests model one: a `u64` and a drop fn that records.
        #[test]
        fn a_refused_reply_gives_back_what_it_minted() {
            let _serial = crate::post::test_lock();

            static RECLAIMED: Mutex<Vec<u64>> = Mutex::new(Vec::new());
            unsafe fn note(h: u64) {
                RECLAIMED.lock().unwrap().push(h);
            }
            extern "C" fn take(_id: u64, ptr: *mut u8, len: u64, cap: u64) {
                drop(unsafe { Vec::from_raw_parts(ptr, len as usize, cap as usize) });
            }
            fn reply(handle: u64) -> FramedWriter {
                let mut w = FramedWriter::status(crate::envelope::STATUS_OK);
                unsafe { w.write_minted(handle, note) };
                w
            }

            RECLAIMED.lock().unwrap().clear();
            let iso = register_callback(take);
            let call = (iso << ISOLATE_SHIFT) | 3;

            // Delivered: the wrapper on the far side owns the object now.
            assert!(deliver(call, reply(0xA1)));
            assert!(
                RECLAIMED.lock().unwrap().is_empty(),
                "a delivered reply must not free what Dart now holds"
            );

            // The isolate exits. Both refusal branches — the tombstone, and
            // the port that reports the death — end here.
            mark_gone(iso as u32);
            assert!(!deliver(call, reply(0xB2)), "a gone isolate is refused");
            assert!(!deliver(call, reply(0xC3)), "and stays refused");
            assert_eq!(
                *RECLAIMED.lock().unwrap(),
                vec![0xB2, 0xC3],
                "each refused reply freed exactly the handle it had minted"
            );

            registry().write().unwrap().remove(&(iso as u32));
        }

        /// A destructor that panics while a refused reply is being reclaimed
        /// is reported, and the thread lives.
        ///
        /// `respond` is called from loops that own their thread and take the
        /// next job from a bare `recv` — a pool worker, an actor host — so an
        /// escaping panic here would be a worker gone on native (no replenish)
        /// or a host thread gone with its `Sender` still in the map, wedging
        /// every later call to that actor. Neither is a thing a user `Drop` may
        /// do to the runtime, so the panic is reported and the frame returns.
        #[test]
        fn a_panicking_drop_in_a_reclaim_is_reported_not_fatal() {
            let _serial = crate::post::test_lock();

            static REPORTS: Mutex<Vec<String>> = Mutex::new(Vec::new());
            fn note_report(r: &crate::panic::PanicReport) {
                REPORTS.lock().unwrap().push(r.message().to_string());
            }
            unsafe fn explode(_h: u64) {
                panic!("a destructor said no");
            }
            extern "C" fn take(_id: u64, ptr: *mut u8, len: u64, cap: u64) {
                drop(unsafe { Vec::from_raw_parts(ptr, len as usize, cap as usize) });
            }

            REPORTS.lock().unwrap().clear();
            let iso = register_callback(take);
            mark_gone(iso as u32);
            let call = (iso << ISOLATE_SHIFT) | 9;

            crate::panic::register(note_report);
            let mut w = crate::codec::FramedWriter::status(crate::envelope::STATUS_OK);
            unsafe { w.write_minted(0xD1E, explode) };
            respond(call, w);
            crate::panic::unregister();

            let reports = REPORTS.lock().unwrap();
            assert_eq!(reports.len(), 1, "the panic must not vanish: {reports:?}");
            assert!(reports[0].contains("a destructor said no"), "{reports:?}");

            registry().write().unwrap().remove(&(iso as u32));
        }
    }
}

#[cfg(target_family = "wasm")]
pub use wasm::{deliver, respond};

#[cfg(target_family = "wasm")]
pub mod wasm {
    #[link(wasm_import_module = "frustrate")]
    extern "C" {
        /// Provided by the embedding JS at instantiation.
        fn post(call_id: u64, ptr: *mut u8, len: u64, cap: u64);
    }

    /// Deliver an async response to the host. Leaks the Vec into a raw
    /// triple; the host copies out of linear memory and frees it via
    /// frustrate_buffer_free.
    ///
    /// The page always takes it, so the payload's mint ledger is given up
    /// unread: the wrappers built on the far side own those objects.
    pub fn respond(call_id: u64, reply: crate::codec::FramedWriter) {
        let (mut payload, minted) = reply.into_parts();
        minted.delivered();
        let ptr = payload.as_mut_ptr();
        let len = payload.len() as u64;
        let cap = payload.capacity() as u64;
        std::mem::forget(payload);
        unsafe { post(call_id, ptr, len, cap) };
    }

    /// The web twin of the native [`crate::post::deliver`]: always `true`.
    /// There are no isolates on web — the consumer is the page, which cannot
    /// go away underneath a producer while the module is running — so the
    /// dead-consumer case the native signature exists for does not arise, and
    /// shared code (`stream::post_method`) needs no cfg fork.
    pub fn deliver(call_id: u64, reply: crate::codec::FramedWriter) -> bool {
        respond(call_id, reply);
        true
    }
}
