/// Platform-neutral runtime surface. Generated bindings depend only on this
/// (plus the codec and OpaqueHandle); the FFI and web transports implement it
/// behind conditional exports in frustrate.dart.
library;

import 'binary_codec.dart';
import 'exceptions.dart';
import 'pending_calls.dart' show FrustrateCancelToken;

/// Delivery callbacks for one open Rust → Dart stream, registered via
/// `openStream`. `onItem` receives a reader positioned at one encoded item;
/// `onError`/`onDone` are terminal.
typedef StreamItemHandler = void Function(BinaryReader item);
typedef StreamErrorHandler = void Function(Object error, StackTrace st);
typedef StreamDoneHandler = void Function();

/// Walk one payload that will **not** be dispatched and dispose every opaque
/// handle it carries.
///
/// A channel item or a callback argument may carry a handle Rust already
/// minted, and the only thing that would ever have disposed it is the wrapper
/// the item handler builds. Where the router absorbs a payload instead —
/// an item for a cancelled subscription, an invocation for a retired
/// registration — this stands in for that handler: same wire, same order,
/// disposing where the handler would have handed the wrapper on. Supplied by
/// the generated binding, and only for a payload that can carry one.
typedef StreamReclaim = void Function(BinaryReader payload);

/// A channel a Dart-object handle can be registered on — the isolate/page
/// transport, or one actor host.
///
/// Generated code binds handles against this rather than branching on which
/// of the two it holds, which is what lets a handle appear anywhere in a
/// parameter (nested in a struct, in a list) with the same emitted code.
abstract interface class FrustrateChannel {
  /// Register a Dart object under a fresh id, with [methods] indexed by
  /// method selector (0 is the primary method — `add`, or the invocation of
  /// a callback). The object is kept alive by the registration and released
  /// when the Rust side drops its handle.
  int openObject(
    List<StreamItemHandler> methods,
    StreamErrorHandler onError,
    StreamDoneHandler onDone, {
    String? label,
    List<StreamReclaim?>? reclaim,
  });

  /// Open a Rust → Dart stream on this channel:
  /// allocates an id and registers the handlers. The generated caller
  /// encodes the id into the request, in the position the handle occupies.
  /// Shorthand for the single-method [openObject].
  int openStream(
    StreamItemHandler onItem,
    StreamErrorHandler onError,
    StreamDoneHandler onDone, {
    String? label,
    List<StreamReclaim?>? reclaim,
  });

  /// Cancel a channel from the Dart side. Delivery stops immediately and
  /// unconditionally; the Rust producer observes a cooperative flag (its
  /// `add` returns false) — immediately on native and threaded wasm, at the
  /// next turn on a single-threaded instance.
  void cancelStream(int id);

  /// Signal backpressure to the Rust producer: the consumer paused its
  /// subscription. A producer awaiting `StreamSink::send` parks until
  /// [resumeStream]; a producer using the non-blocking `add` is unaffected.
  /// Wired to `StreamController.onPause` by the generated binding.
  /// Cooperative and lossless-ordered with [resumeStream].
  void pauseStream(int id);

  /// Release backpressure: the consumer resumed. Wakes any producer parked in
  /// `StreamSink::send`. Wired to `StreamController.onResume`.
  void resumeStream(int id);

  /// Route an opening-call failure to the channel's error handler, if still
  /// open, and retire the registration either way.
  ///
  /// Called by the generated binding through [FrustrateOpenScope.failAllOpened]
  /// when the call that registered the channel fails — registration happens
  /// during encoding, so a rejection on the way in would otherwise strand it.
  /// A no-op for an id the router no longer holds, which is the common case
  /// when Rust got far enough to end the channel itself.
  void failStream(int id, Object error, StackTrace st);

  /// Register a value-returning Dart callback (a `DartFunction<T, R>`
  /// param). [onInvoke] decodes the argument,
  /// runs the closure, and returns the encoded result payload; the router
  /// envelopes it (a throw becomes the error envelope, surfacing as a
  /// panic in the blocked Rust worker) and responds through the transport.
  ///
  /// [onDeclaredError] is supplied only for a **fallible** closure —
  /// `DartFunction<T, Result<R, E>>` — and encodes a throw of the declared
  /// error type into the value the Rust body receives as `Err(E)`, returning
  /// null for anything else so an undeclared throw stays the loud path. It
  /// travels beside [onInvoke] rather than inside it because the *status byte*
  /// is what discriminates the two, and only the router writes that.
  int openFunction(
    BinaryWriter Function(BinaryReader args) onInvoke, {
    String? label,
    BinaryWriter? Function(Object error)? onDeclaredError,
    StreamReclaim? reclaim,
  });
}

/// The registration context for one call's request encoding: which channel
/// handles register on, what to label them, and which ids this call has opened
/// so far.
///
/// Threaded explicitly through the generated encoders rather than kept in
/// ambient state, for the reason the channel alone always was: a struct encoder
/// is a plain top-level function, and a handle nested inside a struct belongs to
/// the same call as one passed directly.
///
/// [opened] exists because registration happens during *encoding* — before Rust
/// has the call. A call that fails on the way in (a dead pool, a trap on the
/// hand-off, an issue-path rollback) would otherwise leave its channels
/// registered for the life of the isolate, with the consumer's stream neither
/// erroring nor closing. The generated caller retires them with
/// [FrustrateChannel.failStream] and rethrows, so the failure reaches both the
/// caller and every channel the call had opened.
final class FrustrateOpenScope {
  final FrustrateChannel channel;

  /// The member that opened these channels (`'TextDoc.watch'`), carried into
  /// the registration for leak attribution.
  final String label;

  final List<int> opened = [];

  FrustrateOpenScope(this.channel, this.label);

  /// Register [id] as opened by this call. Returns [id], so a generated
  /// `open*` result can be recorded inline.
  int track(int id) {
    opened.add(id);
    return id;
  }

  /// Retire everything this call opened, because the call itself failed. Safe
  /// to call when some or all have already been retired by Rust: `failStream`
  /// is a no-op for an id the router no longer holds.
  ///
  /// **Only for failures raised before Rust owned the call.** An envelope error
  /// proves the opposite — Rust received the request, decoded it, and ran — so
  /// the channels are its to end: on native its unwinding drop already retired
  /// them, and on web under `panic=abort` no destructor runs and the stream is
  /// left open by design. Retiring here would also be
  /// wrong rather than merely redundant, because a member that *stored* a sink
  /// before failing leaves a live clone that a later call may still feed.
  ///
  /// A claimed cancellation is in that same category and for the same reason:
  /// the claim is proof Rust had the call, and dropping its future drops the
  /// sinks it captured, which retires those channels through their own end
  /// events. A token cancelled *before* the call was issued never reaches here
  /// at all — the call is refused before encoding, so nothing was opened
  /// (pending_calls.dart).
  void failAllOpened(Object error, StackTrace st) {
    if (error is BridgeException ||
        error is BridgePanicException ||
        error is ContentionException ||
        error is CancelledCallException) {
      return;
    }
    for (final id in opened) {
      channel.failStream(id, error, st);
    }
  }
}

/// One platform transport for bridge calls. Implementations: the FFI
/// transport on native (runtime_native.dart) and the wasm transport on web
/// (runtime_web.dart).
abstract interface class FrustrateRuntime implements FrustrateChannel {
  /// Which **bridge** this runtime speaks for — the identity every handle,
  /// actor host and channel is stamped against.
  ///
  /// A platform transport answers `this`: it *is* the bridge, and its raws,
  /// its drop exports and its channel registry are its own. A decorator
  /// (package:frustrate/intercept.dart) answers its inner runtime's identity,
  /// because wrapping a transport does not change any of that — the same raws
  /// come back, the same exports free them. A fake answers `this`.
  ///
  /// That distinction is the whole reason this is a separate concept from the
  /// runtime object. [Frustrate.activate] compares *bridges*, so putting a
  /// tracing decorator on a live app or taking it off again neither requires
  /// quiescence nor invalidates a single handle, while swapping in a fake
  /// does both.
  Object get bridgeIdentity;

  /// How many async bridge calls this transport still owes a completion — its
  /// own, plus those in flight on every actor host it spawned.
  ///
  /// Exact, and derived from the pending registrations rather than counted by
  /// hand. Hosts are deliberately not enumerated: neither transport keeps a
  /// list of the hosts it spawned, a GC-reaped host never reports its own
  /// death, and a live-host count would overcount anyway. What matters here is
  /// only work that a bridge swap would strand, and that is exactly the set of
  /// calls someone is still waiting on.
  ///
  /// Read by [Frustrate.activate] and [Frustrate.reset] alongside
  /// [openChannelCount]; the two together are what "quiescent" means. Also a
  /// fine teardown assertion in its own right.
  int get inFlightCallCount;

  /// Run a sync bridge call. Returns a reader positioned at the return-value
  /// payload, or throws the envelope's exception.
  ///
  /// [encode] writes the request; the transport supplies the writer. That
  /// inversion is what lets the native transport hand the encoder the FFI
  /// block the call is about to use, so the request is written once, in place,
  /// instead of into a Dart buffer that is then copied there.
  ///
  /// It cannot be the other way round. A throw during encoding is reachable —
  /// `writeU64`'s range check, `writeChar`, a user `BytesCodec.toBytes` — and
  /// with a caller-built writer over transport memory there would be no frame
  /// left to free the block. The closure gives the block exactly one owner:
  /// the transport allocates, calls [encode], dispatches, and frees in one
  /// `try`/`finally`, whatever happens in between.
  ///
  /// [sizeHint] is [BinaryWriter]'s hint and carries its contract exactly — a
  /// lower bound on the bytes about to be written, never an assertion about
  /// them. Being wrong in either direction is safe.
  ///
  /// [typedError], when the member declares a bridged error type, decodes a
  /// `statusTypedError` payload into the exception to throw. Only the
  /// generated binding knows that type, so it travels with the call.
  BinaryReader callSync(
    int fnId,
    int sizeHint,
    void Function(BinaryWriter w) encode, {
    Object Function(BinaryReader)? typedError,
  });

  /// Run an async bridge call. The request is decoded by Rust before the
  /// future is returned (handle borrows are upgraded to owned clones there),
  /// so the request buffer and any handles only need to live through this
  /// call.
  ///
  /// **Never throws synchronously.** Every failure arrives through the
  /// returned future, including one raised before Rust ever saw the call (a
  /// missing export, a failed request allocation, a trap on the way in) — so
  /// `unawaited(…)`, `Future.wait([…])`, `.catchError(…)` and
  /// store-then-await-later all behave. A failed issue also leaves nothing
  /// registered, which on native is what lets the isolate still exit.
  ///
  /// [encode] now runs inside that guarantee too: encoding happens where the
  /// request buffer is allocated, so a throwing encoder rejects the returned
  /// future like any other issue failure rather than escaping synchronously.
  /// See [callSync] for why the writer arrives through a closure.
  ///
  /// [cancel] binds the call to a [FrustrateCancelToken], so cancelling the
  /// token claims the call out of the Rust cooperative executor and drops its
  /// future. Supplied by the generated binding exactly when the member's Rust
  /// body is an `async fn` — the only shape *on this transport* with a
  /// droppable future — and never otherwise. (The other one lives on an actor:
  /// see [ActorHost.call]'s `cancel`, which takes the same token type for the
  /// same reason.) An already-cancelled token refuses the call before it
  /// encodes anything.
  Future<BinaryReader> callAsync(
    int fnId,
    int sizeHint,
    void Function(BinaryWriter w) encode, {
    Object Function(BinaryReader)? typedError,
    FrustrateCancelToken? cancel,
  });

  /// The drop/finalization hooks for an opaque type, looked up by its
  /// generated `frustrate_drop_<Type>` symbol.
  HandleDrop handleDrop(String symbol);

  /// [handleDrop] for a `#[bridge(resident)]` type, which needs a different
  /// GC path and gets one here rather than a flag on [handleDrop] so that the
  /// two are memoized separately and neither can be handed out for the other.
  ///
  /// A resident object has no `Send` bound: only the thread that built it may
  /// run its `Drop`. Native's ordinary path is a `NativeFinalizer`, whose
  /// callback fires on an arbitrary VM thread, so it cannot be used — this one
  /// is backed by a `dart:core` [Finalizer], whose callback runs on the
  /// isolate that attached it. Web's ordinary path already is one. The native
  /// implementation also tells Rust which isolate owns the handle, which is
  /// the only way Rust can learn it: a resident is minted on a sync call, and
  /// the sync entry point carries no isolate id.
  HandleDrop residentHandleDrop(String symbol);

  /// Read the loaded library's `frustrate_schema_hash` export and compare it
  /// against [expected] (the generated `frustrateSchemaHash`). Throws
  /// [StateError] on a mismatch — stale bindings dispatch the wrong `fn_id`s
  /// and decode wrong types into unsafe handle derefs, so the guard must be
  /// loud. [expected] is the full unsigned 64-bit value as a [BigInt] (the
  /// faithful Dart form of a `u64`); the transport reads the export and does
  /// one comparison, once, at init.
  void checkSchemaHash(BigInt expected);

  /// Create a fresh actor executor: a dedicated thread on native, a
  /// worker-hosted wasm instance on web. Generated actor constructors call
  /// this once per spawn — one instance, one executor.
  ///
  /// [debugName] is the actor's type name (generated constructors pass it),
  /// carried into host-attributed reports — most importantly the
  /// disposed-deferred `StateError` — so they name the actor rather than an
  /// anonymous host.
  Future<ActorHost> spawnActorHost({String? debugName});

  /// Whether async bridge calls run on real worker threads. True on native
  /// and on the threaded web transport; false on single-threaded web,
  /// where async bodies run inline on the caller and parallelism is the
  /// Actor model's job.
  bool get asyncIsParallel;

  /// Best-effort hardware thread count, always >= 1: logical cores on
  /// native, `navigator.hardwareConcurrency` on web (fallback 4). A sizing
  /// hint — the default width of an `ActorPool`
  /// (package:frustrate/workers.dart) — not a promise about where async calls
  /// run; that is [asyncIsParallel].
  ///
  /// It is **not** the Rust call pool's width, and does not move when an
  /// embedder declares one (`pool::declare_width`, runtime/rust/src/pool.rs).
  /// Two different populations: actor instances, which this sizes, and the
  /// workers that run `#[bridge]` async bodies, which it never did. Even by
  /// default the two numbers may differ — the Rust pool probes
  /// `available_parallelism`, which a cgroup quota clamps below the processor
  /// count this reports. Code that needs the Rust width asks Rust for it.
  int get hardwareParallelism;

  /// Diagnostic: how many Rust → Dart channels (open streams + Rust-held
  /// callbacks) this transport currently has registered. On native an open
  /// channel keeps the isolate alive, so a leaked
  /// registration shows up only as an isolate that never exits — assert this
  /// is `0` after teardown to catch that at test time rather than as a hang.
  int get openChannelCount;

  /// How many `#[bridge(resident)]` objects this process has lost, all-time.
  ///
  /// A resident is freed by the thread that built it and by nothing else, so
  /// two things put an object beyond reach: an isolate that exits still
  /// holding one, and a reclaim that arrives on another thread (which cannot
  /// run the `Drop`, and so strands the object rather than corrupting it).
  /// Neither is a bug in the bridge — they are the price the model states —
  /// but both are invisible from Dart without this.
  ///
  /// Read it the way [openChannelCount] is read: a number that should be flat
  /// across a workload, and whose rise says a resident handle is outliving the
  /// thread that owns it. The Rust runtime also logs each loss by type name
  /// through the `log` facade, which is where the *what* is; this is the
  /// *whether*, available with nothing installed.
  ///
  /// 0 on web, where nothing can be lost: there are no isolates to exit, and
  /// every resident touch is on the one Dart thread (`frustrate::resident`).
  int get residentLeakCount;

  /// Where each currently-registered channel was opened
  /// (`['TextDoc.watch']`), for reporting alongside [openChannelCount].
  ///
  /// The complement to the leak terminal rather than a duplicate of it: a
  /// collected handle reports itself through `LeakedChannelError`, but an
  /// isolate *already* pinned by a leak has gone idle, so nothing allocates,
  /// no collection runs, and no finalizer ever fires.
  /// For that case this is the only lever, and it is the one case actors do
  /// not close either — their reaper (`ActorHost.attachReaper`) is a
  /// collection away like every other, so an isolate that never collects never
  /// reaches it.
  List<String> get openChannelLabels;
}

/// One actor executor. Calls are FIFO and run one at a time; a generated
/// actor class owns exactly one host and routes every method (and its
/// synthetic drop) through it.
abstract interface class ActorHost implements FrustrateChannel {
  /// The bridge this executor belongs to — its runtime's
  /// [FrustrateRuntime.bridgeIdentity], forwarded.
  ///
  /// An actor handle has no [OpaqueHandleBase] to carry a stamp for it (its
  /// identity on this side is the host, not the object), so the generated
  /// receiver read compares this against [Frustrate.activeBridge] instead. Same
  /// rule, same message, one concept — and, like the opaque one, the comparison
  /// is inside an `assert`, because a build without asserts cannot reach a
  /// bridge change at all ([Frustrate.activate]).
  Object get bridgeIdentity;

  /// Dispatch one call on this executor. Same envelope semantics as
  /// [FrustrateRuntime.callAsync], and the same failure contract — including
  /// after [shutdown], where the call is refused with a `StateError` through
  /// the returned future, identically on every platform.
  ///
  /// Channel plumbing (the [FrustrateChannel] members) carries the same
  /// contracts as on the transport. Ids come from one sequence per isolate
  /// (native) or page (web) so a single router can route them, but a cancel
  /// still has to reach *this* host: on web each actor runs its own wasm
  /// instance, and the producer's cancel registry lives in that instance's
  /// memory. An actor busy in a method observes a cancel only once its
  /// executor goes idle — worker-instance physics.
  ///
  /// [deferred] marks a call whose completion the method hands off
  /// (`Deferred<T>`): the executor is
  /// released as soon as the body's synchronous prefix returns, and the
  /// response arrives whenever the detached future resolves — possibly after
  /// later calls' responses. The host tracks these ids because the dispose
  /// contract is theirs alone: a deferred completion still outstanding at
  /// [shutdown] is **cancelled**, and its future throws a `StateError` naming
  /// the actor — identically on both platforms, though by different
  /// mechanisms (native claims the call from the executor race-free; web's
  /// worker dies with the future).
  ///
  /// [cancel] is the per-call lever beside that all-at-once one: a
  /// [FrustrateCancelToken] the caller can use to abandon *this* deferred call
  /// while the actor keeps serving. Supplied by the generated binding exactly
  /// when [deferred] is, because a deferred completion is the only actor shape
  /// with a droppable future — a plain method is finished the moment it is
  /// dequeued. Cancelling claims the call out of the executor and releases
  /// whatever it had reached — its future, or the queued job that would have
  /// produced one; the two hosts reach that executor differently (an FFI call
  /// on native, where it is the process-global one; a `postMessage` round trip
  /// on web, where it lives in the worker's own wasm instance) and the caller
  /// cannot tell them apart.
  ///
  /// [encode] arrives as a closure for the same reason it does on
  /// [FrustrateRuntime.callSync]: the request buffer belongs to the call, and
  /// the frame that allocates it is the frame that must free it.
  Future<BinaryReader> call(
    int fnId,
    int sizeHint,
    void Function(BinaryWriter w) encode, {
    Object Function(BinaryReader)? typedError,
    bool deferred = false,
    FrustrateCancelToken? cancel,
  });

  /// Release the executor. Called by the generated `dispose()` after the
  /// actor's drop call completed; the host must not be used afterwards — a
  /// later [call] is refused rather than dispatched (see [call]).
  Future<void> shutdown();

  /// Register [owner] so this executor is released, and its object dropped,
  /// if [owner] is collected without `dispose()`.
  ///
  /// The actor counterpart of [HandleDrop.attach], and it has to be here
  /// rather than on a per-type hook because an actor's identity on this side
  /// is the *host*, not the object: the object's pointer reaches Rust only
  /// inside a call, and a handle nobody disposed makes no more calls. So the
  /// executor was told how to free its object at construction
  /// (`handle::actor_new`), and all this has to carry is which executor.
  ///
  /// Best-effort by nature — Dart promises a finalizer *may* run, and on web
  /// only that. What it is not is best-effort about *safety*: a reap that
  /// arrives late finds the host already gone, and one that arrives for a
  /// stale id cannot hit a live host, because host ids are never reused.
  void attachReaper(Object owner);

  /// Unregister [owner]. Called by the generated `dispose()` before it can
  /// suspend.
  ///
  /// Not an ordering guard, despite looking like one: the drop call is
  /// submitted synchronously and the executor is a FIFO, so a reap enqueued
  /// afterwards lands behind the drop and finds nothing left to do. What this
  /// releases is the finalizer's own retained state — both implementations
  /// hold the *host* as the token, so an actor that is disposed but still
  /// attached keeps its executor's bookkeeping alive until the handle is
  /// collected.
  void detachReaper(Object owner);
}

/// The error a deferred call's future throws when its actor is disposed with
/// the completion still outstanding. One factory, used by both hosts, so the
/// two platforms throw the identical error for the identical fact even though
/// the cancellation mechanisms differ.
StateError disposedDeferredError(String? debugName) => StateError(
  'frustrate: ${debugName ?? 'the actor'} was disposed with a deferred call '
  'outstanding; its completion was cancelled',
);

/// Per-opaque-type drop hooks: a GC finalizer plus an eager dispose path.
/// `raw` is the handle value exactly as it crosses the wire (a u64).
abstract interface class HandleDrop {
  /// Register [owner] so the Rust object is dropped if the Dart handle is
  /// GC'd without an explicit dispose().
  void attach(covariant Object owner, int raw);

  /// Unregister [owner] (called on explicit dispose, before [drop]).
  void detach(Object owner);

  /// Eagerly drop the Rust object behind [raw].
  void drop(int raw);
}

/// Refuse a handle whose bridge is no longer the active one.
///
/// One thrower for both halves of the rule — [OpaqueHandleBase.handleValue]
/// for opaque, trait-typed and nested handles, the generated actor receiver
/// read for actors — so a user meets one message however the handle was
/// stranded. Both call sites are inside an `assert`, so this function is not
/// in a build without asserts at all; [Frustrate.activate] is what keeps that
/// honest, by refusing there what this would otherwise have to catch here.
Never staleBridgeHandle(String type) => throw StateError(
  'frustrate: this $type belongs to a different bridge than the one now '
  'active. Its handle is a value the transport that minted it issued, so '
  'encoding it into a call on another transport would dereference something '
  'that transport never handed out. Frustrate.reset() (or re-activate the '
  'bridge it came from) to use it again — it was not invalidated, only '
  'stranded, and dispose() still works and still goes to the transport that '
  'minted it.',
);

/// Which bridge minted a handle, for the handles where that is not the answer
/// [_HandleBridge.baseline] gives.
///
/// An [Expando] rather than a field on [OpaqueHandleBase], because a field
/// costs a word on every handle in every build and this costs nothing in the
/// build that ships: every read and every write is inside an `assert`, so a
/// compiler with asserts off drops the calls, then this table, then the two
/// functions below (`tests/dart_integration/tool/handle_bridge_release_check.dart`
/// reads the compiler's own retained set to say so).
///
/// Weak by construction — an [Expando] does not retain its keys — which is not
/// incidental: a handle whose only remaining reference was this table would
/// never be finalized, and the GC drop path is the backstop for every handle
/// nobody disposed.
final Expando<Object> _mintedUnder = Expando<Object>('frustrate handle bridge');

/// Record which bridge minted [handle]. Called only from inside an `assert`.
///
/// Returns true so it can be the *condition* of one: `assert(f())` is stripped
/// whole — call included — from a build without asserts, which a statement
/// guarded by a boolean would not be.
///
/// **What an absent entry means** is the load-bearing part: the handle was
/// minted under [Frustrate._firstBridge], the first bridge identity this
/// isolate ever made active. Sound because a handle can only be minted while
/// something is active, and until the first identity change there is only one
/// thing that can have been. So an app that never swaps — every shipping app,
/// and most tests — records nothing here at all.
bool stampHandleBridge(Object handle) {
  final active = Frustrate.activeBridge;
  if (!identical(active, Frustrate._firstBridge)) {
    _mintedUnder[handle] = active!;
  }
  return true;
}

/// Refuse [handle] if the bridge that minted it is not the active one. Called
/// only from inside an `assert`; returns true so it can be its condition.
bool checkHandleBridge(Object handle) {
  // Nothing has ever changed bridge identity in this isolate, so every live
  // handle was minted under what is active now, stamped or not. The whole
  // check is this one static load for as long as that holds.
  if (!Frustrate._bridgeSwapped) return true;
  final minted = _mintedUnder[handle] ?? Frustrate._firstBridge;
  if (!identical(minted, Frustrate.activeBridge)) {
    staleBridgeHandle('${handle.runtimeType}');
  }
  return true;
}

/// Which member consumed a handle, for the message its next use gets.
///
/// An [Expando] under an `assert`, for the reason [_mintedUnder] gives: a
/// field would cost a word on every handle in every build, and this costs
/// nothing in the build that ships. What a shipping build loses is the member
/// name, not the refusal — a spent handle throws the ordinary
/// `used after dispose()` there, which is true (it *is* disposed) and is the
/// same message every other lifecycle mistake gets.
final Expando<String> _consumedBy = Expando<String>('frustrate consumed by');

/// Record that [member] consumed [handle]. Called only from inside an
/// `assert`; returns true so it can be its condition.
bool recordConsumedBy(Object handle, String member) {
  _consumedBy[handle] = member;
  return true;
}

/// Throw the more specific message when [handle] was consumed rather than
/// disposed. Called only from inside an `assert` on a path that is about to
/// throw anyway; returns true (so it can be an assert's condition) when it has
/// nothing more precise to say.
bool reportConsumed(Object handle) {
  final member = _consumedBy[handle];
  if (member == null) return true;
  throw StateError(
    'frustrate: this ${handle.runtimeType} was consumed by `$member` and no '
    'longer refers to anything. `take()` gives the Rust object to the call '
    'it is passed to, which is what makes that call legal; the handle is '
    'spent from that point, exactly as after dispose().',
  );
}

/// Whether this build runs with asserts on.
///
/// The compilers fold it: the assignment is unreachable with asserts off, so
/// this is a constant `false` there and a constant `true` under
/// `--enable-asserts`, `dart test`, `flutter test` and the JIT's checked mode.
bool get _assertsEnabled {
  var on = false;
  assert(on = true);
  return on;
}

/// Entry point holder: a platform init (FrustrateNative.init on native)
/// installs the transport once; generated bindings read [instance].
///
/// ## Installed versus active
///
/// Two slots, answering two different questions.
///
/// **Installed** is the platform transport this isolate (this page, on web)
/// built. It is first-wins, is never replaced, and is never rebuilt — a second
/// `NativeRuntime` would re-register the isolate-exit listener on the
/// process-global port and break the callback sweep, and a web reset that
/// blanked the slot would let a later init reap the live runtime's pool
/// workers. [isInstalled] reports this slot, and the web transport's
/// orphan reap depends on that meaning.
///
/// **Active** is where generated bindings send calls: what [instance] returns.
/// [install] fills it when it is empty, and [activate]/[reset] move it. That
/// is what lets a test put a fake in front of the bindings, and a production
/// build put a tracing decorator there
/// (package:frustrate/intercept.dart), without either one having to
/// reach inside a platform init.
///
/// A swap that **changes bridge identity** requires the outgoing runtime to be
/// quiescent — see [activate]. A swap that keeps it does not, because nothing
/// is at stake: handles, actor hosts and channels are all stamped with a
/// *bridge*, and a decorator is the same bridge.
///
/// ## The repeated-init contract
///
/// One transport is installed per isolate (per page on web), and a platform
/// init may be called more than once. What happens then is decided by *what the
/// call names*, and both halves are deliberate:
///
///   * **The same bridge again — a declared no-op.** Nothing is constructed,
///     nothing is replaced, the call returns. This is not politeness: init is
///     an *ensure*, and the shapes that reach it twice are ordinary. An app
///     with an `initBridge()` helper calls it from `main()` and from a test's
///     `setUp`; two independent widgets each make sure the bridge is up; a
///     `package:test` file inits per group. Throwing at those would buy
///     nothing and break working code.
///
///   * **A different bridge — a [StateError].** The first transport stays
///     installed and keeps serving every call, so a silent second init hands
///     the caller a bridge to a library it no longer believes it is using.
///     Nothing detects that downstream: [FrustrateRuntime.checkSchemaHash]
///     compares the bindings against the *installed* module, so it agrees. This
///     is the one repeated-init case that is always a bug, and it is the case
///     this refuses.
///
/// The guard runs **before** the transport is constructed ([initializedFrom]),
/// not merely before it is installed, and that is load-bearing rather than
/// tidy — see the note there.
///
/// ### Init while a fake is active
///
/// A platform init that runs with **nothing installed and something active** —
/// a test that declared a fake (package:frustrate/testing.dart) and a later
/// code path lazily ensuring the bridge is up — returns having built nothing,
/// and the fake keeps serving.
///
/// This is the same "init is an ensure" reasoning one paragraph up, applied to
/// the one case where the ensure is already satisfied by something that is not
/// a platform transport. A widget whose first build calls
/// `FrustrateNative.init(path)` must not dlopen a path that has no reason to
/// exist under `flutter test`, and the alternative — requiring every such code
/// path to know whether it is under a fake — is the knowledge the ensure exists
/// to remove.
///
/// It is deliberately narrower than "nothing installed": with *both* slots
/// empty this is a genuine first init and proceeds. And once a platform
/// transport **is** installed, the repeated-init rules above apply unchanged,
/// so "real A installed, fake active, init names B" is still a [StateError] —
/// after a [reset] A would serve while the caller believed B, which is exactly
/// what that rule is for.
///
/// ## Hot restart
///
/// A repeated init is not how hot restart reaches the bridge: these are static
/// fields — per isolate on the VM, per heap on web — and hot restart resets
/// exactly that, so a restarted app runs its *first* init. The web transport
/// depends on it: `_reapOrphanedWorkers` treats `!isInstalled` as "the first
/// init of this heap" (hot_restart_workers_test.dart). A restart that did
/// re-enter with the same arguments finds a no-op, not an exception.
class Frustrate {
  /// The platform transport, once built. Retained for the life of the isolate
  /// even while something else is active: [reset] restores this object, and a
  /// handle minted under it stays disposable through it the whole time.
  static FrustrateRuntime? _installed;

  /// What [instance] returns. Starts as [_installed] and moves with
  /// [activate]/[reset].
  static FrustrateRuntime? _active;

  /// [FrustrateRuntime.bridgeIdentity] of [_active], cached so the debug-only
  /// handle check is a static load and an identity compare rather than a
  /// virtual call. Null exactly when nothing is active.
  static Object? _activeBridge;

  /// The first bridge identity this isolate ever made active, and the two
  /// things that turn on it.
  ///
  /// *In a build without asserts* it is the **only** identity allowed: any
  /// change away from it is refused by [_setActive], so a handle can never
  /// meet a bridge that did not mint it. That is a stronger guarantee than
  /// catching the mismatch at the point of use, and it is what pays for the
  /// per-handle stamp being debug-only.
  ///
  /// *With asserts on* it is what an unstamped handle is taken to have been
  /// minted under ([stampHandleBridge]).
  ///
  /// Set once, on the first assignment of a non-null identity; never cleared,
  /// because a `reset` to nothing does not un-mint the handles that exist.
  static Object? _firstBridge;

  /// Whether [_activeBridge] has ever moved *away* from a non-null value —
  /// to a different bridge, or to nothing.
  ///
  /// The fast path for the debug check: while this is false, every live handle
  /// was minted under whatever is active now, so [checkHandleBridge] needs no
  /// side table. "Or to nothing" is not tidiness — `activate(F)`, mint,
  /// `reset()` with nothing installed, `activate(G)` leaves F's handles facing
  /// G, and treating the trip through null as "no swap" would let them
  /// through.
  ///
  /// Read only from inside an `assert`. It is still *written* in a build
  /// without asserts — a `reset` to nothing is allowed there, and is a swap by
  /// this definition — which costs one static word per isolate and nothing per
  /// handle.
  static bool _bridgeSwapped = false;

  /// What the installed transport was built from, and how to name it in a
  /// disagreement. Comparison is `==` on an opaque value each transport
  /// chooses (a path, a `DynamicLibrary`, a module identity), so this file
  /// stays platform-neutral.
  ///
  /// Recorded by [install] — i.e. only once a transport really exists. An init
  /// that failed partway (a library that would not open, a runtime ABI
  /// mismatch, a module that would not instantiate) therefore leaves the slot
  /// completely clean, so the next attempt is a first init and not a
  /// disagreement with a bridge that was never installed.
  static Object? _source;
  static String? _sourceDescription;

  static FrustrateRuntime get instance {
    final i = _active;
    if (i == null) {
      throw StateError(
        'Frustrate has not been initialized; call the platform init '
        '(e.g. FrustrateNative.init with the bridge library path) before '
        'using generated bindings',
      );
    }
    return i;
  }

  /// The bridge a live handle is checked against
  /// ([FrustrateRuntime.bridgeIdentity] of the active runtime), or null when
  /// nothing is active.
  ///
  /// A static, deliberately: the debug check reads it on every handle use, and
  /// a getter that went through the active runtime would turn that into a
  /// virtual call on an interface that a decorator makes polymorphic.
  static Object? get activeBridge => _activeBridge;

  /// Whether a **platform transport** is installed in this isolate.
  ///
  /// Diagnostic, and the web transport's "is this heap's first init" test —
  /// which is why it reports the installed slot and not the active one. A page
  /// running a fake has never built a wasm module, so the orphaned workers of a
  /// previous heap are still that init's to reap.
  ///
  /// A platform init asks [initializedFrom] instead, because the answer it
  /// needs is not "is anything installed" but "is *this* installed".
  static bool get isInstalled => _installed != null;

  /// The repeated-init decision, taken by a platform init **before it
  /// constructs anything**.
  ///
  /// Returns true when a transport built from [source] is already installed:
  /// the caller must return immediately, having built nothing. Returns false
  /// when this isolate has no transport yet. Throws a [StateError] when a
  /// *different* bridge is installed (see the contract on [Frustrate]);
  /// [description] is how this call's bridge is named in that message.
  ///
  /// **Deciding here rather than inside [install] is load-bearing.** [install]
  /// is idempotent, but its argument is evaluated first, so an init that got
  /// that far would build a second transport and discard it. On native the
  /// discarded runtime has already registered an isolate-exit listener, and
  /// re-registering on the same port *replaces* the response value — the
  /// isolate would then report an id no live callback invocation carries and
  /// the sweep would miss every one of them.
  static bool initializedFrom(Object source, String description) {
    final installed = _source;
    if (installed == null) {
      // Nothing installed, but something *active*: a test declared a fake (or
      // any other non-platform runtime) and a later code path is lazily
      // ensuring the bridge is up. Init is an ensure, so the honest answer is
      // "it is up" — the caller returns having built nothing, and a widget
      // that calls `FrustrateNative.init(path)` on first build does not dlopen
      // a path that has no reason to exist under `flutter test`.
      //
      // Narrower than "nothing installed" on purpose: with both slots empty
      // this is a genuine first init and must proceed, which is what the
      // fresh-isolate pin in init_contract_test.dart says.
      return _active != null;
    }
    if (installed == source) return true;
    throw StateError(
      'frustrate: already initialized from $_sourceDescription, and this '
      'call asks for $description. The first bridge stays installed and '
      'every call keeps going to it, so a second init naming a different '
      'bridge is a bug rather than a way to swap bridges — there is one '
      'transport per isolate (per page on web) and it is chosen once. '
      'Calling init again with the SAME bridge is fine and does nothing.',
    );
  }

  /// Install the platform transport, recording what it was built from so a
  /// later init can be answered by [initializedFrom].
  ///
  /// Idempotent per isolate. The `??=`-shaped first-wins here is the backstop
  /// for two inits racing (web's is asynchronous, so two concurrent first
  /// inits can both pass [initializedFrom]); the ordinary sequential case never
  /// reaches it, because the guard already sent the second call home.
  static void install(
    FrustrateRuntime runtime, {
    required Object source,
    required String description,
  }) {
    if (_installed != null) return;
    _installed = runtime;
    _source = source;
    _sourceDescription = description;
    // Only when nothing is active. A platform init that runs while a fake is
    // active must not silently repoint the bindings at the real bridge
    // mid-suite; it fills the installed slot so [reset] has somewhere to go,
    // and the fake keeps serving until the caller says otherwise.
    if (_active == null) {
      _setActive(runtime, 'install');
    }
  }

  /// Make [runtime] what [instance] returns — a fake for a test, a decorator
  /// for tracing or metrics.
  ///
  /// Refused with a [StateError] unless the active runtime is empty or the
  /// installed platform transport. **One active runtime, declared**: composing
  /// a decorator over a fake is `activate(Traced(fake))`, written by the
  /// caller in the order they meant, rather than a stack this assembled behind
  /// their back from two independent `initBridge()` helpers.
  ///
  /// **Quiescence.** When [runtime] is a different *bridge* from the outgoing
  /// one ([FrustrateRuntime.bridgeIdentity]), the outgoing runtime must owe no
  /// completion and hold no open channel, or this throws naming what is left
  /// (including [FrustrateRuntime.openChannelLabels]). Both are things a swap
  /// would strand: an in-flight call answers into a transport nothing points
  /// at any more, and a Rust producer keeps feeding a channel whose consumer
  /// has been swapped out from under it. Worse, a call that settles after the
  /// swap decodes its response — and a handle in that response would be minted
  /// with a raw from one bridge and stamped with another's identity.
  ///
  /// When [runtime] is the *same* bridge — a decorator going on — nothing is
  /// stranded and nothing is checked. Handles, hosts and channels are stamped
  /// with a bridge, not with a runtime object, so a live app with an open
  /// stream can gain and lose a tracing decorator freely. Keying the rule on
  /// the runtime object instead would make a decorator uninstallable in
  /// exactly the app that wanted one.
  ///
  /// **A build with asserts off allows one bridge identity per isolate, and
  /// refuses any change to it.** The per-handle stamp that tells one bridge's
  /// handle from another's lives inside `assert`s, so such a build carries
  /// none of it; the cause is refused there instead of the consequence being
  /// caught, by a [StateError] naming both bridges. Both `activate` and
  /// [reset] are held to that rule. So activating a *fake* needs asserts on —
  /// `dart test`, `flutter test` and every debug app already have them
  /// (`--enable-asserts` otherwise). A decorator is not a bridge change and is
  /// never refused, because `DelegatingRuntime` and `DelegatingActorHost`
  /// forward [FrustrateRuntime.bridgeIdentity].
  ///
  /// **What it does not do** is invalidate the outgoing bridge's handles.
  /// They cannot be enumerated (a handle collected without `dispose()` never
  /// reports), so where asserts are on each one refuses at the point of use and
  /// says so ([staleBridgeHandle]); they stay disposable through the transport
  /// that minted it, and become usable again when that bridge is active.
  ///
  /// Scoped per isolate (per page on web), like [install].
  static void activate(FrustrateRuntime runtime) {
    final outgoing = _active;
    if (outgoing != null && !identical(outgoing, _installed)) {
      throw StateError(
        'frustrate: $outgoing is already active, so activating $runtime '
        'would stack one runtime on another. Frustrate.reset() first, or '
        'compose them yourself and activate the composition — there is one '
        'active runtime and it is chosen in one place.',
      );
    }
    _requireQuiescent(outgoing, runtime.bridgeIdentity, 'activate');
    _setActive(runtime, 'activate');
  }

  /// Put the installed platform transport back in front of the bindings, or
  /// empty the active slot when nothing is installed. Idempotent.
  ///
  /// Never rebuilds a transport: the installed one is retained for the life of
  /// the isolate precisely so this can restore it. A test that wants a
  /// genuinely fresh platform transport spawns an isolate — the shape
  /// `init_contract_test.dart` already uses.
  ///
  /// Held to both of [activate]'s rules, and for the same reasons: quiescence,
  /// and — where asserts are off — one bridge identity per isolate, which a
  /// reset away from a fake that was activated first would change.
  static void reset() {
    final outgoing = _active;
    if (outgoing == null) return;
    final target = _installed;
    _requireQuiescent(outgoing, target?.bridgeIdentity, 'reset');
    _setActive(target, 'reset');
  }

  /// The one door to [_active] and [_activeBridge] — [install], [activate] and
  /// [reset] all come through here, so the bridge-identity bookkeeping cannot
  /// be bypassed by a fourth writer added later.
  ///
  /// **It refuses a bridge-identity change in a build without asserts**, and
  /// that refusal is what lets the per-handle stamp be debug-only. The
  /// argument, in one line: the thing a stale handle does is dereference a raw
  /// on a transport that never issued it, and if bridge identity cannot change
  /// then no handle can ever meet one. So instead of catching the consequence
  /// at every use in every build, the cause is refused once, here, in the
  /// build that could not have caught it.
  ///
  /// Production is unaffected, because the only swap a shipping app performs
  /// is putting a decorator on or taking it off, and a decorator forwards
  /// [FrustrateRuntime.bridgeIdentity] — same bridge, no change, no refusal
  /// (`DelegatingRuntime`, `DelegatingActorHost`). What is refused is
  /// activating a *fake*, which is a test doing it in a vehicle that turned
  /// asserts off.
  ///
  /// Note what routing [install] through here catches that a check written
  /// inside [activate] would not: `activate(fake)`, `reset()` to nothing, then
  /// a real platform init. That is null → a new identity with handles from the
  /// fake still alive, and it is refused — after the init built its transport,
  /// which is the price of deciding here rather than earlier, and is a corner
  /// only reachable in a build without asserts that ran a fake.
  static void _setActive(FrustrateRuntime? runtime, String verb) {
    final incoming = runtime?.bridgeIdentity;
    final first = _firstBridge;
    // Decide before mutating: a refused swap is not a swap, and must leave
    // every slot as it found them.
    if (incoming != null && first != null && !identical(incoming, first)) {
      if (!_assertsEnabled) {
        throw StateError(
          'frustrate: $verb would make $runtime active, which is a different '
          'bridge from $first — and this build has asserts off, so it cannot '
          'tell a handle minted by one from a handle minted by the other. '
          'That check is the per-handle stamp, which costs nothing here '
          'precisely because a build like this one cannot reach a bridge '
          'change at all. Sending a raw to a transport that never issued it '
          'is not something the wire can catch, so the swap is refused '
          'instead of being allowed to become a bad dereference later. Run '
          'with asserts on (--enable-asserts; dart test and flutter test '
          'already do) if this is a test. A tracing or metrics decorator is '
          'not a bridge change and is always allowed.',
        );
      }
    }
    if (!identical(incoming, _activeBridge) && _activeBridge != null) {
      _bridgeSwapped = true;
    }
    _firstBridge ??= incoming;
    _active = runtime;
    _activeBridge = incoming;
  }

  /// The shared half of [activate] and [reset]: a swap that changes bridge
  /// identity is refused while the outgoing runtime still has live work.
  ///
  /// "In flight" means the transport owes a completion. A response that has
  /// already settled its future but whose caller has not yet resumed to decode
  /// it is not counted — Dart offers no way to observe that, and closing the
  /// gap would mean threading the answering runtime through every generated
  /// decode site rather than reading the active slot at the mint.
  static void _requireQuiescent(
    FrustrateRuntime? outgoing,
    Object? incomingBridge,
    String verb,
  ) {
    if (outgoing == null) return;
    if (identical(outgoing.bridgeIdentity, incomingBridge)) return;
    final calls = outgoing.inFlightCallCount;
    final open = outgoing.openChannelCount;
    if (calls == 0 && open == 0) return;
    throw StateError(
      'frustrate: $verb would change which bridge is active while $outgoing '
      'still has $calls call(s) in flight and $open open '
      'channel(s) ${outgoing.openChannelLabels}. An in-flight call would '
      'answer into a transport '
      'nothing points at, and a handle in its response would be minted '
      'against the wrong bridge. Await the calls and end the streams first.',
    );
  }
}
