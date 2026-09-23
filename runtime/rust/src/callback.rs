//! Dart → Rust callbacks: Dart closures that Rust code can invoke.
//!
//! Two shapes, mirroring the streams design (the coherent trio: a
//! `StreamSink<T>` is a Rust→Dart data channel, a `DartCallback<T>` is the
//! same channel consumed by a Dart closure, a `DartFunction<T, R>` adds
//! the reverse round trip):
//!
//! - [`DartCallback<T>`] — fire-and-forget. Protocol-wise it IS a stream
//!   whose consumer is the closure: `call` posts an item event, dropping
//!   the last clone retires the Dart registration via the end event.
//!   Portable everywhere a bridge call is.
//! - [`DartFunction<T, R>`] — value-returning, with two ways to invoke:
//!   - [`call_async`](DartFunction::call_async) — the **portable** primitive.
//!     Awaited inside a bridged `async fn`, it parks a *future* (not a
//!     thread) on the enclosing executor task's waker: the invocation is
//!     posted, the poll returns `Pending`, and `frustrate_callback_respond`
//!     wakes it when the closure answers. No thread blocks, so it runs on
//!     every platform — native, single-threaded web, threaded web — reusing
//!     the executor's waker seam with zero new executor code.
//!   - [`call`](DartFunction::call) — **blocking**, worker-only. Blocks the
//!     invoking worker thread while the application thread runs the closure
//!     and responds. Blocking a thread against the caller's event loop is a
//!     native/actor-thread capability (same fact as `on_contention =
//!     "block"`), for sync-in-async use; it is illegal on a sync member
//!     (whose body runs on the very thread the closure needs) and off a
//!     worker thread, guarded statically by the checker and at runtime here.
//!
//! Both shapes share ONE invocation registry and ONE
//! `frustrate_callback_respond` export (a `Waiter` discriminates them).

use crate::codec::ByteWriter;
use crate::stream::StreamSink;

/// A Dart closure Rust can fire and forget. Storable, cloneable, callable
/// from any thread that may run bridge code; the closure observes calls in
/// happens-before order. The Dart-side registration lives until the last
/// clone drops.
pub struct DartCallback<T> {
    sink: StreamSink<T>,
}

impl<T> Clone for DartCallback<T> {
    fn clone(&self) -> Self {
        DartCallback {
            sink: self.sink.clone(),
        }
    }
}

impl<T: Send + 'static> DartCallback<T> {
    /// Invoke the Dart closure with `arg`. Fire-and-forget: the closure
    /// runs on the application thread's event loop, ordered with other
    /// bridge events.
    pub fn call(&self, arg: T) {
        let _ = self.sink.add(arg);
    }
}

/// Construct the callback handle for a decoded callback id. Called by
/// generated glue, which supplies the argument encoder.
/// A `DartCallback` *is* a stream whose consumer is the closure, so the
/// undelivered-argument reclaim rides `StreamSink`'s machinery unchanged.
pub fn callback<T>(id: u64, encode: fn(T, &mut ByteWriter)) -> DartCallback<T> {
    DartCallback {
        sink: crate::stream::sink(id, encode),
    }
}

// The DartFunction machinery compiles and RUNS on every target: a member
// taking one is portable via `call_async`, so its glue is emitted on the web
// surface too. Only the blocking `call` path needs a worker thread (its guard
// is a runtime assert, not a cfg gate).
pub use returning::{
    enter_worker_context, fallible_function, function, CallFuture, DartFunction,
};

/// Fail a dead isolate's outstanding callback invocations. Called by
/// `post::deliver` when a post is refused; see [`returning`].
#[cfg(not(target_family = "wasm"))]
pub(crate) use returning::fail_invocations_for;

mod returning {
    use crate::codec::{ByteReader, ByteWriter, FramedWriter};
    use crate::envelope::{STATUS_CALLBACK_CALL, STATUS_ERROR, STATUS_OK, STATUS_TYPED_ERROR};
    use crate::stream::Inner;
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc::{channel, Sender};
    use crate::spin::SpinLock;
    use std::sync::Arc;
    use std::task::{Context, Poll, Waker};

    std::thread_local! {
        static IS_WORKER: Cell<bool> = const { Cell::new(false) };
    }

    /// Mark the current thread as a frustrate worker (pool or actor host).
    /// [`DartFunction::call`] is legal only on such threads: anywhere else
    /// the application thread — which must run the Dart closure — could be
    /// the one blocking, and that is a deadlock, not a wait.
    pub fn enter_worker_context() {
        IS_WORKER.with(|w| w.set(true));
    }

    /// A writer framed for a callback invocation:
    /// `[STATUS_CALLBACK_CALL, CALL, invocation id, argument]`. The single
    /// place that names the pair, shared by the blocking and awaited paths so
    /// the two cannot drift apart — they encode the same wire shape and only
    /// differ in how they park.
    /// An argument that reaches an opaque is minted by this encode, and an
    /// invocation the consumer never takes has to give those objects back —
    /// which `post::deliver` does from the writer's own ledger.
    fn invocation_writer() -> FramedWriter {
        FramedWriter::event(STATUS_CALLBACK_CALL, crate::stream::selector::CALL)
    }

    /// A parked invocation awaiting its Dart response. Two shapes share ONE
    /// registry and ONE `frustrate_callback_respond` export: the blocking
    /// [`DartFunction::call`] parks a worker thread on an mpsc channel; the
    /// cooperative [`DartFunction::call_async`] parks a future on the
    /// executor task's waker. `respond` routes by the shape it finds.
    enum Waiter {
        Blocking(Sender<Vec<u8>>),
        Async(Arc<SpinLock<Slot>>),
    }

    /// One registered invocation: the waiter, plus the isolate that owes it a
    /// response. The isolate is recorded so [`fail_invocations_for`] can fail a
    /// dead isolate's waiters in bulk — invocation ids come from their own
    /// sequence and carry no isolate tag, unlike channel ids.
    struct Registered {
        // Recorded on every config, read only where the sweep exists:
        // `fail_invocations_for` is native-only, because a wasm page has one
        // consumer that cannot die separately from the module. Written rather
        // than `#[cfg]`-ed away so the two construction sites stay
        // config-independent — the cost is one allow, not a forked struct.
        #[cfg_attr(target_family = "wasm", allow(dead_code))]
        isolate: u32,
        waiter: Waiter,
    }

    /// The isolate that owns `channel_id` — the high bits of every id the Dart
    /// side allocates (post.rs). Always 0 on wasm, which has one page-wide
    /// consumer and no tag.
    fn isolate_of(channel_id: u64) -> u32 {
        #[cfg(not(target_family = "wasm"))]
        {
            (channel_id >> crate::post::ISOLATE_SHIFT) as u32
        }
        #[cfg(target_family = "wasm")]
        {
            let _ = channel_id;
            0
        }
    }

    /// Fail every invocation still awaiting a response from `isolate`, which is
    /// gone and so will never answer.
    ///
    /// Without this a worker thread blocked in [`DartFunction::call`] waits
    /// forever on a channel nobody will fill, permanently narrowing the pool,
    /// and a [`CallFuture`] suspended in [`DartFunction::call_async`] leaks its
    /// executor task and never completes its enclosing bridge call.
    ///
    /// **Reached from `post::mark_gone`, on either of its two triggers** — the
    /// isolate's own exit notice, or a refused post. The exit notice is what
    /// makes this cover the case the sweep was written for and could not reach:
    /// an invocation the isolate *accepted* and then dropped by dying with it
    /// queued. Nothing refuses, so before the notice existed nothing swept, and
    /// if no producer ever posted to that isolate again the waiter parked for
    /// the life of the process. Both waiter shapes are covered identically,
    /// which is why the awaited path needed no cure of its own.
    ///
    /// Poison-tolerant on both locks: `post::handle_exit` reaches this from an
    /// `extern "C"` frame, where a panic would unwind across the FFI boundary.
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn fail_invocations_for(isolate: u32) {
        // Collect under the lock and settle after it: filling an async slot
        // wakes a task, which runs the executor's scheduler, and nesting that
        // inside the registry lock would invert the lock order that
        // `frustrate_callback_respond` already established. It is also what
        // keeps the exit-notice handler non-blocking — no lock is held across
        // a wake.
        let orphans: Vec<Waiter> = {
            with_invocations(|map| {
                let ids: Vec<u64> = map
                    .iter()
                    .filter(|(_, r)| r.isolate == isolate)
                    .map(|(id, _)| *id)
                    .collect();
                ids.into_iter()
                    .filter_map(|id| map.remove(&id))
                    .map(|r| r.waiter)
                    .collect()
            })
        };
        for waiter in orphans {
            match waiter {
                // Dropping the sender makes the blocked `recv` fail, which
                // `call` turns into an attributable panic on the enclosing
                // bridge call.
                Waiter::Blocking(tx) => drop(tx),
                Waiter::Async(slot) => {
                    let waker = slot.with(|g| {
                        g.response =
                            Some(crate::envelope::string_bytes(STATUS_ERROR, DEAD_CONSUMER));
                        g.waker.take()
                    });
                    if let Some(waker) = waker {
                        waker.wake();
                    }
                }
            }
        }
    }

    /// What a caller sees when the Dart side of a returning callback is gone.
    /// Reported as a thrown-closure error, so it reaches the bridge caller
    /// through the enclosing call's panic envelope like any other callback
    /// failure.
    const DEAD_CONSUMER: &str = "the Dart isolate that owns this callback is gone \
         (it exited, was killed, or the app hot restarted), so no response can arrive";

    /// The cell a [`CallFuture`] and `frustrate_callback_respond` rendezvous
    /// on: respond fills `response` and wakes; the next poll drains it. The
    /// `CallFuture` holds one `Arc` clone, the registry the other, so respond
    /// can deliver even between polls.
    struct Slot {
        response: Option<Vec<u8>>,
        waker: Option<Waker>,
    }

    /// In-flight invocations awaiting their Dart response.
    ///
    /// A [`SpinLock`], not a `Mutex`, and const-built rather than behind a
    /// `OnceLock` — both for the same reason, and both load-bearing on
    /// threaded wasm:
    ///
    /// * **The lock.** `call_async` registers here from a bridged `async fn`
    ///   body, which the executor drains on a *pool worker*; the answer arrives
    ///   through `frustrate_callback_respond`, which the web transport calls on
    ///   the page instance — i.e. from the *browser main thread*
    ///   (`runtime_web.dart`). Main thread and worker
    ///   therefore contend one lock, and a futex-parking lock traps there
    ///   ("Atomics.wait cannot be called in this context"). Critical sections
    ///   are single map operations — never a message body, which is why spin is
    ///   right here and a parking lock is right in `actor.rs` — so this is the
    ///   same stance `stream.rs`'s cancel registry takes, for the same reason.
    /// * **The `OnceLock`.** `get_or_init` parks on its cold path, so the first
    ///   two threads to reach an uninitialised registry could put the main
    ///   thread into `Once`'s futex — the exact failure `pool::QUEUE` shipped.
    ///   A const-built static has no initialiser to race.
    static INVOCATIONS: SpinLock<Option<HashMap<u64, Registered>>> = SpinLock::new(None);

    fn with_invocations<R>(f: impl FnOnce(&mut HashMap<u64, Registered>) -> R) -> R {
        INVOCATIONS.with(|m| f(m.get_or_insert_with(HashMap::new)))
    }

    static NEXT_INVOCATION: AtomicU64 = AtomicU64::new(1);

    /// A Dart closure Rust can invoke for a result. Storable and cloneable
    /// like [`super::DartCallback`]. Two ways to invoke: [`call_async`] — the
    /// portable primitive, awaited inside a bridged `async fn`, parks a
    /// future and runs everywhere — and [`call`] — blocking, worker-only,
    /// for sync-in-async on a native/actor thread.
    ///
    /// [`call`]: DartFunction::call
    /// [`call_async`]: DartFunction::call_async
    pub struct DartFunction<T, R> {
        end: Arc<Inner>,
        /// By value: an argument that reaches an opaque handle is minted by
        /// this encoder, and minting takes the object. See `StreamSink`.
        encode: fn(T, &mut ByteWriter),
        decode: fn(&mut ByteReader) -> R,
        /// `Some` for a **fallible** closure — `DartFunction<T, Result<K, E>>`.
        /// It decodes the `STATUS_TYPED_ERROR` reply the Dart side sends when the
        /// closure throws the *declared* exception, so the failure reaches the
        /// Rust body as `Err(E)`.
        ///
        /// `None` is the original shape, and it is what keeps the loud case
        /// loud: `STATUS_ERROR` still means "the closure failed in a way nobody
        /// declared" on both shapes, and still panics.
        decode_err: Option<fn(&mut ByteReader) -> R>,
    }

    impl<T, R> Clone for DartFunction<T, R> {
        fn clone(&self) -> Self {
            DartFunction {
                end: Arc::clone(&self.end),
                encode: self.encode,
                decode: self.decode,
                decode_err: self.decode_err,
            }
        }
    }

    impl<T, R> DartFunction<T, R> {
        /// Invoke the Dart closure and block until it responds. A closure
        /// that throws becomes a Rust panic here — attributable through
        /// the enclosing call's panic envelope, never silent.
        ///
        /// **Bounded in liveness, not in duration** (native): the wait has no
        /// deadline — a closure that takes ten minutes parks its caller for ten
        /// minutes — but it ends the moment the isolate that owes the answer is
        /// gone, whether it refused the invocation, was swept by another
        /// producer's refusal, or died with the invocation already queued. The
        /// last of those used to need a probe loop here; the isolate now
        /// reports its own death (`post::handle_exit`) and this is a plain
        /// `recv` again.
        ///
        /// Contract (checked): only from pool or actor context. The
        /// checker already rejects `DartFunction` params on sync members;
        /// this guard catches a stored handle invoked from one anyway.
        pub fn call(&self, arg: T) -> R {
            assert!(
                IS_WORKER.with(|w| w.get()),
                "frustrate: DartFunction::call outside pool/actor context would \
                 deadlock — it blocks the current thread while the application \
                 thread runs the Dart closure. Invoke it from an async member's \
                 body, or use a DartCallback (fire-and-forget)"
            );
            let invocation = NEXT_INVOCATION.fetch_add(1, Ordering::Relaxed);
            // Encode BEFORE registering the waiter. The argument encoder is
            // user-supplied and may panic; registering first would strand the
            // entry on unwind (ids never recycle → a slow leak). Registration
            // still precedes the `respond` below, so no response can arrive
            // before its waiter exists.
            // [status, selector, invocation id, argument] — the selector is
            // the mirror's method, exactly as on the void path; a
            // DartFunction declares one method, so it is the primary slot.
            let mut w = invocation_writer();
            w.write_handle(invocation);
            (self.encode)(arg, &mut w);
            let (tx, rx) = channel();
            with_invocations(|m| {
                m.insert(
                    invocation,
                    Registered {
                        isolate: isolate_of(self.end.id),
                        waiter: Waiter::Blocking(tx),
                    },
                )
            });
            if !crate::post::deliver(self.end.id, w) {
                // `deliver` has already given back what the argument minted:
                // it never reached Dart, so no wrapper will ever dispose it.
                //
                // The consumer isolate is gone, so nothing will ever fill `rx`.
                // Retire the entry and fail here rather than parking this
                // worker thread forever.
                // Bound out: dropping a `Registered` inside the critical
                // section would run `Waiter`'s drop there — a `Sender` drop
                // unparks a blocked receiver, an `Arc<SpinLock<Slot>>` drop can
                // free a `Waker`. Neither belongs under a spin lock.
                let orphan = with_invocations(|m| m.remove(&invocation));
                drop(orphan);
                panic!("frustrate: {DEAD_CONSUMER}");
            }
            // A closed channel means the waiter was taken by
            // `fail_invocations_for`, i.e. `post::mark_gone` ran for this
            // isolate — because it reported its own exit, or because a post to
            // it was refused. Nothing else can close it.
            let resp = rx
                .recv()
                .unwrap_or_else(|_| panic!("frustrate: {DEAD_CONSUMER}"));
            decode_response(&resp, self.decode, self.decode_err)
        }

        /// Invoke the Dart closure and await its result cooperatively — the
        /// portable primitive (native, single-threaded web, threaded web).
        /// Unlike [`call`](Self::call) it parks a *future*, not a thread, so
        /// it needs no worker context and never blocks the caller's event
        /// loop; a `Pending` poll yields to the executor, which resumes the
        /// task when the response arrives. Await it inside a bridged
        /// `async fn`. A closure that throws becomes a Rust panic here
        /// (identical decode to `call`), attributable through the enclosing
        /// call's panic envelope.
        ///
        /// Inert until awaited: the invocation is allocated, encoded, and
        /// posted on the *first poll*, so a `call_async` that is dropped
        /// unawaited posts nothing.
        pub fn call_async(&self, arg: T) -> CallFuture<T, R> {
            CallFuture {
                end: Arc::clone(&self.end),
                encode: self.encode,
                decode: self.decode,
                decode_err: self.decode_err,
                arg: Some(arg),
                id: 0,
                slot: None,
            }
        }
    }

    /// The future returned by [`DartFunction::call_async`]. On the first poll
    /// it allocates an invocation, encodes `handle + arg`, registers a
    /// [`Waiter::Async`], posts the call, and parks on the enclosing executor
    /// task's waker; [`frustrate_callback_respond`] fills the slot and wakes,
    /// and the re-poll decodes the response. It couples to NO executor type —
    /// only to `cx.waker()`, exactly the `YieldOnce` seam an `async fn` uses.
    ///
    /// `Drop` removes the invocation id from the registry, so a cancelled (or
    /// panicked-out) call strands nothing — ids never recycle, so a stranded
    /// entry would leak for the session.
    pub struct CallFuture<T, R> {
        end: Arc<Inner>,
        encode: fn(T, &mut ByteWriter),
        decode: fn(&mut ByteReader) -> R,
        /// See [`DartFunction::decode_err`] — carried so the awaited path reads
        /// a declared failure exactly as the blocking one does.
        decode_err: Option<fn(&mut ByteReader) -> R>,
        /// Taken (encoded, then discarded) on the first poll.
        arg: Option<T>,
        /// The allocated invocation id, or 0 before the first poll. 0 is never
        /// a real id (`NEXT_INVOCATION` starts at 1), so drop/remove is a safe
        /// no-op before the entry exists.
        id: u64,
        slot: Option<Arc<SpinLock<Slot>>>,
    }

    // No self-referential fields, so moving is sound: `CallFuture` is `Unpin`,
    // and `poll` reaches its fields through `get_mut`.
    impl<T, R> Future for CallFuture<T, R> {
        type Output = R;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<R> {
            // Sound: `CallFuture` has no pinned or self-referential fields, so
            // it never relies on its pinned address — moving through this
            // reference is safe. (Using `get_unchecked_mut` avoids a `T: Unpin`
            // bound leaking onto the argument type of every returning callback.)
            let this = unsafe { self.get_unchecked_mut() };
            if this.id == 0 {
                // First poll. Encode BEFORE registering (same anti-strand
                // ordering as `call`): the user-supplied encoder may panic,
                // and `id` staying 0 until the insert succeeds means such a
                // panic drops through `Drop` as a no-op — nothing stranded.
                let arg = this
                    .arg
                    .take()
                    .expect("frustrate: CallFuture polled after completion");
                let invocation = NEXT_INVOCATION.fetch_add(1, Ordering::Relaxed);
                // See the blocking path: [status, selector, invocation, arg].
                let mut w = invocation_writer();
                w.write_handle(invocation);
                (this.encode)(arg, &mut w);
                let slot = Arc::new(SpinLock::new(Slot {
                    response: None,
                    waker: Some(cx.waker().clone()),
                }));
                with_invocations(|m| {
                    m.insert(
                        invocation,
                        Registered {
                            isolate: isolate_of(this.end.id),
                            waiter: Waiter::Async(Arc::clone(&slot)),
                        },
                    )
                });
                this.id = invocation;
                this.slot = Some(slot);
                // Registration precedes the post, so no response can arrive
                // before its waiter exists.
                if !crate::post::deliver(this.end.id, w) {
                    // `deliver` reclaimed what the argument minted.
                    // The consumer isolate is gone. Parking would leak this
                    // task until process exit, so fail the invocation the same
                    // way a thrown closure does — the enclosing call's panic
                    // envelope carries it.
                    let orphan = with_invocations(|m| m.remove(&invocation));
                    drop(orphan);
                    this.id = 0;
                    panic!("frustrate: {DEAD_CONSUMER}");
                }
                return Poll::Pending;
            }
            let slot = this.slot.as_ref().expect("slot present once posted");
            let taken = slot.with(|g| {
                let resp = g.response.take();
                if resp.is_none() {
                    // Not answered yet: refresh the waker (the task may resume
                    // on a different executor thread) and keep parking.
                    g.waker = Some(cx.waker().clone());
                }
                resp
            });
            let Some(resp) = taken else {
                return Poll::Pending;
            };
            // `respond` already removed the registry entry; this is the sole
            // remover on the never-answered cancel path, and idempotent here.
            let answered = with_invocations(|m| m.remove(&this.id));
            drop(answered);
            Poll::Ready(decode_response(&resp, this.decode, this.decode_err))
        }
    }

    /// Decode one callback response into the closure's result — the single
    /// place that says what a reply status *means*, shared by the blocking
    /// [`DartFunction::call`] and the awaited [`CallFuture`] so the two cannot
    /// drift apart.
    ///
    /// Three statuses, and the split between the last two is the whole feature:
    ///
    /// - `STATUS_OK` — the closure returned; decode the value.
    /// - `STATUS_TYPED_ERROR` — the closure threw the error type the signature
    ///   **declared**, so it is a value: `Err(E)`, which the Rust body handles.
    /// - `STATUS_ERROR` — the closure failed in a way nobody declared. Still a
    ///   panic, on both shapes, which is what keeps "contracts loud": a Dart bug
    ///   can never arrive dressed as a business failure.
    ///
    /// A dead consumer settles waiters with `STATUS_ERROR`
    /// (`fail_invocations_for`, `DEAD_CONSUMER`), so an isolate that died owing
    /// an answer is a panic and never a plausible `Err(E)`.
    fn decode_response<R>(
        resp: &[u8],
        decode: fn(&mut ByteReader) -> R,
        decode_err: Option<fn(&mut ByteReader) -> R>,
    ) -> R {
        let mut r = ByteReader::new(resp);
        match r.read_u8() {
            STATUS_OK => {
                // Symmetric with generated Dart's `r.assertConsumed()` on a
                // response and generated Rust's `r.assert_consumed()` on a
                // request: a callback answer is exactly a status byte plus one
                // encoded value, so trailing bytes are a bridge bug, never
                // something to ignore.
                let v = decode(&mut r);
                r.assert_consumed();
                v
            }
            STATUS_TYPED_ERROR => match decode_err {
                Some(decode_err) => {
                    let v = decode_err(&mut r);
                    r.assert_consumed();
                    v
                }
                // Unreachable in a consistent pair, and named rather than
                // guessed for the same reason envelope.dart names its mirror
                // case: fallibility is part of the IR the schema fingerprint
                // covers, so a binding that does not know this closure can fail
                // cannot match the library that lets it. The alternative on an
                // impossible path is decoding the payload as whatever the next
                // arm expects.
                None => panic!(
                    "frustrate: a Dart closure answered with a typed error, but this \
                     DartFunction declares none. The generated binding and the Rust \
                     library disagree about this closure — rebuild both from the same \
                     api.rs."
                ),
            },
            // The error arm consumes the rest by construction (one string) and
            // panics regardless, so it needs no assert.
            STATUS_ERROR => panic!("frustrate: Dart callback threw: {}", r.read_string()),
            other => panic!("frustrate: invalid callback response status {other}"),
        }
    }

    impl<T, R> Drop for CallFuture<T, R> {
        fn drop(&mut self) {
            if self.id != 0 {
                let abandoned = with_invocations(|m| m.remove(&self.id));
                drop(abandoned);
            }
        }
    }

    /// Construct the function handle for a decoded callback id. Called by
    /// generated glue, which supplies the argument encoder and result
    /// decoder.
    pub fn function<T, R>(
        id: u64,
        encode: fn(T, &mut ByteWriter),
        decode: fn(&mut ByteReader) -> R,
    ) -> DartFunction<T, R> {
        DartFunction {
            end: crate::stream::end(id),
            encode,
            decode,
            decode_err: None,
        }
    }

    /// Construct the handle for a **fallible** Dart closure —
    /// `DartFunction<T, Result<K, E>>`, whose failure the Rust body handles as
    /// a value.
    ///
    /// Both decoders are spelled `-> Result<K, E>` rather than `-> K` and
    /// `-> E`, which is what lets one storage slot hold either and still makes
    /// this signature *pin the shape*: a caller that passes decoders for
    /// mismatched types cannot infer `K`/`E`, so codegen's mistake is a Rust
    /// type error and never a wire-level surprise. Generated code emits
    /// `|r| Ok(<decode K>)` and `|r| Err(<decode E>)`.
    pub fn fallible_function<T, K, E>(
        id: u64,
        encode: fn(T, &mut ByteWriter),
        decode_ok: fn(&mut ByteReader) -> Result<K, E>,
        decode_err: fn(&mut ByteReader) -> Result<K, E>,
    ) -> DartFunction<T, Result<K, E>> {
        DartFunction {
            end: crate::stream::end(id),
            encode,
            decode: decode_ok,
            decode_err: Some(decode_err),
        }
    }

    /// Deliver one callback response from Dart. Copies the buffer; the
    /// caller keeps ownership of its memory.
    ///
    /// # Safety
    /// `ptr` must be valid for `len` bytes.
    // `frustrate_block_check` gating: see the "block-check export gating" note
    // in lib.rs. `mod returning` is not cfg'd, so this export is present on
    // wasm even though the blocking `call` path beside it is not.
    #[cfg(not(frustrate_block_check))]
    #[no_mangle]
    pub unsafe extern "C" fn frustrate_callback_respond(
        invocation: u64,
        ptr: *const u8,
        len: u64,
    ) {
        let bytes = if ptr.is_null() || len == 0 {
            vec![]
        } else {
            unsafe { std::slice::from_raw_parts(ptr, len as usize).to_vec() }
        };
        // An unknown invocation id means the waiter is gone (a blocking
        // caller panicked out, or an async caller was cancelled/dropped);
        // dropping the response is the only option left, and any failure was
        // already reported. Route by the waiter's shape.
        let waiter = with_invocations(|m| m.remove(&invocation)).map(|r| r.waiter);
        match waiter {
            Some(Waiter::Blocking(tx)) => {
                let _ = tx.send(bytes);
            }
            Some(Waiter::Async(slot)) => {
                // Take the waker and release the slot BEFORE waking: a native
                // wake may immediately schedule a re-poll on another thread
                // that locks this same slot — and under a spin lock that
                // re-entry would not block, it would hang.
                let waker = slot.with(|g| {
                    g.response = Some(bytes);
                    g.waker.take()
                });
                if let Some(waker) = waker {
                    waker.wake();
                }
            }
            None => {}
        }
    }

    /// Test-only: number of in-flight invocation entries. Lets a test prove
    /// that an encoder panic strands none (the leak fixed in `call`).
    #[cfg(test)]
    pub(crate) fn invocation_count() -> usize {
        with_invocations(|m| m.len())
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;
    use crate::codec::{ByteReader, ByteWriter};
    use crate::envelope::{STATUS_CALLBACK_CALL, STATUS_STREAM_END, STATUS_STREAM_ITEM};
    use std::sync::Mutex;

    fn enc_i64(v: i64, w: &mut ByteWriter) {
        w.write_i64(v);
    }
    fn dec_i64(r: &mut ByteReader) -> i64 {
        r.read_i64()
    }

    static SEEN: Mutex<Vec<(u64, Vec<u8>)>> = Mutex::new(Vec::new());

    extern "C" fn record(call_id: u64, ptr: *mut u8, len: u64, cap: u64) {
        let bytes = unsafe { Vec::from_raw_parts(ptr, len as usize, cap as usize) };
        SEEN.lock().unwrap().push((call_id, bytes));
    }

    fn events_for(id: u64) -> Vec<Vec<u8>> {
        SEEN.lock()
            .unwrap()
            .iter()
            .filter(|(i, _)| *i == id)
            .map(|(_, b)| b.clone())
            .collect()
    }

    use super::returning;

    // One test fn, serialized with other post-using tests via
    // post::test_lock (the post callback is process-global).
    #[test]
    fn callback_events_function_round_trip_and_guards() {
        let _serial = crate::post::test_lock();
        crate::post::init(record);

        // DartCallback: item events, then the end event on last drop —
        // stream framing exactly.
        let cb = callback::<i64>(8001, enc_i64);
        cb.call(4);
        cb.clone().call(5);
        drop(cb);
        let ev = events_for(8001);
        assert_eq!(ev.len(), 3, "{ev:?}");
        // [status, selector, payload] — a DartCallback is the primary-slot
        // mirror, so its invocation is selector 0, exactly like `Sink.add`.
        assert_eq!(ev[0][0], STATUS_STREAM_ITEM);
        assert_eq!(ev[0][1], crate::stream::selector::ADD);
        assert_eq!(ByteReader::new(&ev[0][2..]).read_i64(), 4);
        assert_eq!(ByteReader::new(&ev[1][2..]).read_i64(), 5);
        assert_eq!(ev[2], vec![STATUS_STREAM_END]);

        // DartFunction: invoking outside worker context is a loud panic,
        // not a deadlock.
        fn panic_message(p: Box<dyn std::any::Any + Send>) -> String {
            p.downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .expect("string panic payload")
        }
        let f = function::<i64, i64>(8002, enc_i64, dec_i64);
        let denied = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f.call(1)));
        let msg = panic_message(denied.unwrap_err());
        assert!(msg.contains("deadlock"), "{msg}");
        assert!(msg.contains("DartCallback"), "must name the portable alternative: {msg}");

        // Round trip from a (fake) worker thread: the "Dart side" here
        // reads the posted invocation, doubles the argument, and responds
        // through the export.
        let handle = {
            let f = f.clone();
            std::thread::spawn(move || {
                enter_worker_context();
                f.call(21)
            })
        };
        let (invocation, arg) = loop {
            let ev = events_for(8002);
            if let Some(e) = ev.first() {
                assert_eq!(e[0], STATUS_CALLBACK_CALL);
                assert_eq!(e[1], crate::stream::selector::CALL);
                let mut r = ByteReader::new(&e[2..]);
                break (r.read_handle(), r.read_i64());
            }
            std::thread::yield_now();
        };
        assert_eq!(arg, 21);
        let mut resp = vec![0u8]; // STATUS_OK
        let mut w = ByteWriter::new();
        w.write_i64(arg * 2);
        resp.extend_from_slice(&w.take());
        unsafe { returning::frustrate_callback_respond(invocation, resp.as_ptr(), resp.len() as u64) };
        assert_eq!(handle.join().unwrap(), 42);

        // A throwing closure (error response) becomes an attributable
        // Rust panic in the invoking worker.
        let handle = {
            let f = f.clone();
            std::thread::spawn(move || {
                enter_worker_context();
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f.call(7)))
            })
        };
        let invocation = loop {
            let ev = events_for(8002);
            if ev.len() >= 2 {
                assert_eq!(ev[1][1], crate::stream::selector::CALL);
                let mut r = ByteReader::new(&ev[1][2..]);
                break r.read_handle();
            }
            std::thread::yield_now();
        };
        let mut resp = vec![1u8]; // STATUS_ERROR
        let mut w = ByteWriter::new();
        w.write_string("closure exploded");
        resp.extend_from_slice(&w.take());
        unsafe { returning::frustrate_callback_respond(invocation, resp.as_ptr(), resp.len() as u64) };
        let msg = panic_message(handle.join().unwrap().unwrap_err());
        assert!(msg.contains("closure exploded"), "{msg}");

        // Dropping the function retires its registration.
        drop(f);
        let ev = events_for(8002);
        assert_eq!(ev.last().unwrap(), &vec![STATUS_STREAM_END]);

        // A response for a dead invocation is dropped, never a crash.
        unsafe { returning::frustrate_callback_respond(999_999, std::ptr::null(), 0) };
    }

    // A panicking argument encoder must not strand an invocation entry: the
    // registration is inserted only after encoding succeeds, so an unwind
    // leaves the registry exactly as it found it (ids never recycle, so a
    // stranded entry would be a permanent leak). Serialized via test_lock —
    // it reads the process-global invocation registry and drops a function
    // (which posts an end event), both shared with the round-trip test above.
    #[test]
    fn call_does_not_strand_invocation_on_encoder_panic() {
        let _serial = crate::post::test_lock();
        crate::post::init(record);

        fn boom(_v: i64, _w: &mut ByteWriter) {
            panic!("argument encoder exploded");
        }

        returning::enter_worker_context();
        let baseline = returning::invocation_count();
        let f = function::<i64, i64>(8010, boom, dec_i64);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f.call(1)));
        assert!(result.is_err(), "the encoder panic must propagate");
        assert_eq!(
            returning::invocation_count(),
            baseline,
            "encoder panic stranded an invocation entry in the registry",
        );
    }

    // ------------------------------------------------- call_async (executor) --
    //
    // These drive the cooperative path: a real `frustrate::executor::Executor`
    // runs an async task that awaits `call_async`, and the test plays the Dart
    // side — reading the posted invocation off the recorded post events and
    // answering through `frustrate_callback_respond`. All serialize on
    // `post::test_lock` (the post callback and the invocation registry are
    // process-global) and use distinct ids to stay independent.

    use crate::codec::FramedWriter;
    use crate::envelope::{Outcome, STATUS_ERROR, STATUS_OK, STATUS_PANIC};
    use crate::executor::Executor;
    use std::future::Future;
    use std::sync::Arc;
    use std::task::{Context, Poll};

    /// A recording executor: `schedule` is a no-op (tests drive drains by
    /// hand; a wake still re-enqueues the runnable) and `complete` logs the
    /// delivered `(call_id, envelope)`.
    /// What the recording executor's completion hook appends to:
    /// `(call_id, envelope)` per delivered answer.
    type Delivered = Arc<Mutex<Vec<(u64, Vec<u8>)>>>;

    fn recording_executor() -> (Executor, Delivered) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink = log.clone();
        let exec = Executor::new(|| {}, move |id, reply: FramedWriter| {
            sink.lock().unwrap().push((id, reply.delivered_bytes()))
        });
        (exec, log)
    }

    /// The invocation id of the (single) posted CALLBACK_CALL on `end_id`.
    fn posted_invocation(end_id: u64) -> u64 {
        let ev = events_for(end_id);
        assert_eq!(ev.len(), 1, "exactly one invocation posted: {ev:?}");
        // [status, selector, invocation id, argument]: a DartFunction
        // declares one method, so it rides the primary selector.
        assert_eq!(ev[0][0], STATUS_CALLBACK_CALL);
        assert_eq!(ev[0][1], crate::stream::selector::CALL);
        ByteReader::new(&ev[0][2..]).read_handle()
    }

    fn ok_response(value: i64) -> Vec<u8> {
        let mut resp = vec![STATUS_OK];
        let mut w = ByteWriter::new();
        w.write_i64(value);
        resp.extend_from_slice(&w.take());
        resp
    }

    #[test]
    fn call_async_round_trip_completes_on_the_executor() {
        let _serial = crate::post::test_lock();
        crate::post::init(record);
        let baseline = returning::invocation_count();

        let (exec, log) = recording_executor();
        let f = function::<i64, i64>(8100, enc_i64, dec_i64);
        // A bridged `async fn` body: awaits the returning callback, encodes
        // the result — no worker context, no blocking.
        exec.spawn(700, async move {
            let doubled = f.call_async(21).await;
            let mut w = FramedWriter::status(STATUS_OK);
            w.write_i64(doubled);
            Outcome::Ok(w)
        });

        // First poll posts the invocation and parks; nothing delivered yet.
        assert!(exec.drain_one());
        assert!(log.lock().unwrap().is_empty(), "parked, not completed");
        assert_eq!(exec.task_count(), 1, "task suspended, awaiting the response");
        assert_eq!(returning::invocation_count(), baseline + 1, "one waiter parked");

        // The Dart side answers; the wake re-enqueues the task.
        let invocation = posted_invocation(8100);
        let resp = ok_response(42);
        unsafe { returning::frustrate_callback_respond(invocation, resp.as_ptr(), resp.len() as u64) };

        assert!(exec.drain_one(), "the wake re-enqueued the task");
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 1, "completed on the re-poll");
        assert_eq!(log[0].0, 700);
        assert_eq!(log[0].1[0], STATUS_OK);
        assert_eq!(ByteReader::new(&log[0].1[1..]).read_i64(), 42);
        assert_eq!(exec.task_count(), 0, "finished task reaped");
        assert_eq!(returning::invocation_count(), baseline, "waiter drained");
    }

    #[test]
    fn call_async_throwing_closure_becomes_a_panic_envelope() {
        let _serial = crate::post::test_lock();
        crate::post::init(record);

        let (exec, log) = recording_executor();
        let f = function::<i64, i64>(8110, enc_i64, dec_i64);
        exec.spawn(710, async move {
            let v = f.call_async(7).await;
            let mut w = FramedWriter::status(STATUS_OK);
            w.write_i64(v);
            Outcome::Ok(w)
        });
        assert!(exec.drain_one());

        // An error response: the re-poll panics (the closure threw), the
        // executor catches it and delivers a panic envelope for THIS call.
        let invocation = posted_invocation(8110);
        let mut resp = vec![STATUS_ERROR];
        let mut w = ByteWriter::new();
        w.write_string("closure exploded");
        resp.extend_from_slice(&w.take());
        unsafe { returning::frustrate_callback_respond(invocation, resp.as_ptr(), resp.len() as u64) };

        assert!(exec.drain_one());
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].0, 710);
        assert_eq!(log[0].1[0], STATUS_PANIC);
        assert!(ByteReader::new(&log[0].1[1..])
            .read_string()
            .contains("closure exploded"));
        assert_eq!(exec.task_count(), 0, "panicked task removed");
    }

    #[test]
    fn call_async_encoder_panic_strands_nothing() {
        let _serial = crate::post::test_lock();
        crate::post::init(record);
        let baseline = returning::invocation_count();

        fn boom(_v: i64, _w: &mut ByteWriter) {
            panic!("argument encoder exploded");
        }
        let (exec, log) = recording_executor();
        let f = function::<i64, i64>(8120, boom, dec_i64);
        exec.spawn(720, async move {
            let v = f.call_async(1).await;
            let mut w = FramedWriter::status(STATUS_OK);
            w.write_i64(v);
            Outcome::Ok(w)
        });

        // The first poll encodes (and panics) BEFORE registering: the executor
        // turns it into a panic envelope, and nothing is stranded.
        assert!(exec.drain_one());
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].1[0], STATUS_PANIC);
        assert_eq!(exec.task_count(), 0);
        assert_eq!(
            returning::invocation_count(),
            baseline,
            "encoder panic on the first poll stranded a waiter",
        );
    }

    #[test]
    fn call_async_drop_deregisters_and_a_late_respond_is_a_noop() {
        let _serial = crate::post::test_lock();
        crate::post::init(record);
        let baseline = returning::invocation_count();

        let f = function::<i64, i64>(8130, enc_i64, dec_i64);
        // Poll the future once by hand so it registers, then drop it. A bare
        // `Waker` (noop) suffices — we only need the registration side effect.
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut fut = Box::pin(f.call_async(5));
        assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Pending));
        assert_eq!(returning::invocation_count(), baseline + 1, "registered");
        let invocation = posted_invocation(8130);
        drop(fut);
        assert_eq!(returning::invocation_count(), baseline, "drop deregistered");

        // A response for the now-dead invocation is silently dropped.
        let resp = ok_response(99);
        unsafe { returning::frustrate_callback_respond(invocation, resp.as_ptr(), resp.len() as u64) };
        assert_eq!(returning::invocation_count(), baseline, "late respond is a no-op");
        drop(f);
    }

    #[test]
    fn cancelling_an_in_flight_call_async_releases_the_future() {
        let _serial = crate::post::test_lock();
        crate::post::init(record);
        let baseline = returning::invocation_count();

        let (exec, log) = recording_executor();
        let f = function::<i64, i64>(8140, enc_i64, dec_i64);
        exec.spawn(740, async move {
            let v = f.call_async(3).await;
            let mut w = FramedWriter::status(STATUS_OK);
            w.write_i64(v);
            Outcome::Ok(w)
        });
        assert!(exec.drain_one(), "first poll posts and parks");
        assert_eq!(returning::invocation_count(), baseline + 1);
        let invocation = posted_invocation(8140);

        // Cancel drops the task; draining runs async_task's final no-poll
        // runnable, which drops the future — whose `Drop` deregisters the
        // waiter. No completion is ever delivered.
        exec.cancel(740);
        exec.drain_all();
        assert_eq!(exec.task_count(), 0);
        assert_eq!(returning::invocation_count(), baseline, "cancel deregistered the waiter");
        assert!(log.lock().unwrap().is_empty(), "a cancelled call never completes");

        // A late response after cancel is a no-op.
        let resp = ok_response(1);
        unsafe { returning::frustrate_callback_respond(invocation, resp.as_ptr(), resp.len() as u64) };
        assert!(log.lock().unwrap().is_empty());
    }

    // ------------------------------------ a Dart closure that can refuse --
    //
    // `DartFunction<T, Result<K, E>>`: the DECLARED failure crosses under STATUS_TYPED_ERROR and reaches the Rust body as
    // `Err(E)`; everything else keeps its old meaning and stays loud. These
    // drive both invocation shapes, because both are settled by one
    // `decode_response`.

    /// The fixture error: an i64 payload, standing in for a bridged enum.
    fn fallible_fn(id: u64) -> DartFunction<i64, Result<i64, i64>> {
        fallible_function::<i64, i64, i64>(
            id,
            enc_i64,
            |r| Ok(r.read_i64()),
            |r| Err(r.read_i64()),
        )
    }

    /// `[STATUS_TYPED_ERROR, <encoded E>]` — the reply a Dart closure sends
    /// when it throws the error type the signature declared.
    fn typed_error_response(value: i64) -> Vec<u8> {
        let mut resp = vec![crate::envelope::STATUS_TYPED_ERROR];
        let mut w = ByteWriter::new();
        w.write_i64(value);
        resp.extend_from_slice(&w.take());
        resp
    }

    #[test]
    fn call_async_declared_failure_is_a_value_and_undeclared_still_panics() {
        let _serial = crate::post::test_lock();
        crate::post::init(record);

        // (1) The declared failure. The body handles it and completes OK —
        // which is the whole feature: no panic envelope anywhere.
        let (exec, log) = recording_executor();
        let f = fallible_fn(8150);
        exec.spawn(750, async move {
            let v = f.call_async(1).await;
            let mut w = FramedWriter::status(STATUS_OK);
            w.write_i64(match v {
                Ok(n) => n,
                Err(code) => -code,
            });
            Outcome::Ok(w)
        });
        assert!(exec.drain_one());
        let invocation = posted_invocation(8150);
        let resp = typed_error_response(250);
        unsafe {
            returning::frustrate_callback_respond(invocation, resp.as_ptr(), resp.len() as u64)
        };
        assert!(exec.drain_one());
        {
            let log = log.lock().unwrap();
            assert_eq!(
                log[0].1[0], STATUS_OK,
                "a declared failure must reach the Rust body as a value, not a panic"
            );
            assert_eq!(
                ByteReader::new(&log[0].1[1..]).read_i64(),
                -250,
                "the payload crossed, not just the fact of failure"
            );
        }

        // (2) An UNDECLARED throw on the very same handle. STATUS_ERROR keeps
        // its one meaning on both shapes — "the closure failed in a way nobody
        // declared" — so it is still this call's panic. This is the assertion
        // that makes the feature safe rather than merely convenient.
        let (exec, log) = recording_executor();
        let f = fallible_fn(8151);
        exec.spawn(751, async move {
            let v = f.call_async(1).await;
            let mut w = FramedWriter::status(STATUS_OK);
            w.write_i64(v.unwrap_or(0));
            Outcome::Ok(w)
        });
        assert!(exec.drain_one());
        let invocation = posted_invocation(8151);
        let mut resp = vec![STATUS_ERROR];
        let mut w = ByteWriter::new();
        w.write_string("a bug, not a refusal");
        resp.extend_from_slice(&w.take());
        unsafe {
            returning::frustrate_callback_respond(invocation, resp.as_ptr(), resp.len() as u64)
        };
        assert!(exec.drain_one());
        let log = log.lock().unwrap();
        assert_eq!(log[0].1[0], STATUS_PANIC, "an undeclared throw stays loud");
        assert!(ByteReader::new(&log[0].1[1..])
            .read_string()
            .contains("a bug, not a refusal"));
    }

    #[test]
    fn blocking_call_reads_a_declared_failure_the_same_way() {
        let _serial = crate::post::test_lock();
        crate::post::init(record);

        // The blocking and awaited shapes share ONE `decode_response`, so this
        // is the pin that they cannot drift apart — the native/actor members
        // that must use `call` get the identical contract.
        let f = fallible_fn(8160);
        let handle = {
            let f = f.clone();
            std::thread::spawn(move || {
                enter_worker_context();
                f.call(9)
            })
        };
        let invocation = loop {
            let ev = events_for(8160);
            if let Some(e) = ev.first() {
                break ByteReader::new(&e[2..]).read_handle();
            }
            std::thread::yield_now();
        };
        let resp = typed_error_response(42);
        unsafe {
            returning::frustrate_callback_respond(invocation, resp.as_ptr(), resp.len() as u64)
        };
        assert_eq!(handle.join().unwrap(), Err(42), "Err, not a panic");
        drop(f);
    }

    #[test]
    fn a_typed_error_to_an_infallible_function_is_a_loud_bridge_bug() {
        let _serial = crate::post::test_lock();
        crate::post::init(record);

        // Unreachable in a consistent pair — fallibility is in the IR the
        // schema fingerprint covers — but decoding the payload as whatever the
        // next arm expects would be the worse answer on an impossible path.
        let (exec, log) = recording_executor();
        let f = function::<i64, i64>(8170, enc_i64, dec_i64);
        exec.spawn(770, async move {
            let v = f.call_async(1).await;
            let mut w = FramedWriter::status(STATUS_OK);
            w.write_i64(v);
            Outcome::Ok(w)
        });
        assert!(exec.drain_one());
        let invocation = posted_invocation(8170);
        let resp = typed_error_response(1);
        unsafe {
            returning::frustrate_callback_respond(invocation, resp.as_ptr(), resp.len() as u64)
        };
        assert!(exec.drain_one());
        let log = log.lock().unwrap();
        assert_eq!(log[0].1[0], STATUS_PANIC);
        let msg = ByteReader::new(&log[0].1[1..]).read_string();
        assert!(msg.contains("declares none"), "{msg}");
        assert!(msg.contains("rebuild both"), "must name the fix: {msg}");
    }

    /// A no-op `Waker` for hand-driven polls (the drop test needs a `Context`
    /// but never a real wake).
    fn noop_waker() -> std::task::Waker {
        use std::task::{RawWaker, RawWakerVTable, Waker};
        fn no_op(_: *const ()) {}
        fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(std::ptr::null(), &VTABLE)
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, no_op, no_op, no_op);
        unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
    }
}
