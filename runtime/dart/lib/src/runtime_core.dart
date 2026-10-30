/// Platform-neutral runtime surface. Generated bindings depend only on this
/// (plus the codec and OpaqueHandle); the FFI and web transports implement it
/// behind conditional exports in frustrate.dart.
library;

import 'binary_codec.dart';
import 'bridge.dart';
import 'exceptions.dart';
import 'pending_calls.dart' show FrustrateCancelToken;

export 'bridge.dart';

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
  /// The bridge whose calls this runtime answers. Generated code reaches it
  /// through [Frustrate.of] with that bridge, and [Frustrate.activate] puts it
  /// in front of that bridge alone.
  FrustrateBridge get servedBridge;

  /// What actually issues and frees this runtime's raws — the identity every
  /// handle, actor host and channel is stamped against.
  ///
  /// A platform runtime answers its transport: the one Rust runtime of this
  /// isolate (page, on web), shared by every bridge linked into its library.
  /// A decorator (package:frustrate/intercept.dart) answers its inner
  /// runtime's identity, because wrapping a runtime does not change any of
  /// that — the same raws come back, the same exports free them. A fake
  /// answers `this`.
  ///
  /// That distinction is the whole reason this is a separate concept from the
  /// runtime object. [Frustrate.activate] compares identities, so putting a
  /// tracing decorator on a live app or taking it off again neither requires
  /// quiescence nor invalidates a single handle, while swapping in a fake
  /// does both.
  Object get bridgeIdentity;

  /// How many async bridge calls this transport still owes a completion — its
  /// own, plus those in flight on every actor host it spawned. A platform
  /// runtime counts its own bridge's calls, not the other bridges sharing its
  /// transport.
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
  /// generated `frustrate_<crate>_drop_<Type>` symbol.
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

  /// Read this bridge's `frustrate_<crate>_schema_hash` export and compare it
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
  /// The actor belongs to the zone this is called in: a panic in its work
  /// that no call waits on (a posted closure, a detached task) is an uncaught
  /// error there.
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
  /// receiver read compares this against [Frustrate.identityOf] instead. Same
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
  /// is the *host*, not the object: the object lives in its executor's object
  /// table (`handle::actor_new`), which drops whatever is still in it when the
  /// executor stops. So all this has to carry is which executor.
  ///
  /// Best-effort by nature — Dart promises a finalizer *may* run, and on web
  /// only that. What it is not is best-effort about *safety*: a reap that
  /// arrives after the host stopped finds nothing to do, because stopping is
  /// idempotent, and the token it carries keeps the host it names alive until
  /// the reap has run.
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

/// What minted a handle ([FrustrateRuntime.bridgeIdentity]), for the handles
/// where that is not the first identity their bridge ever had active.
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

/// Record what minted [handle], a handle of [bridge]. Called only from inside
/// an `assert`.
///
/// Returns true so it can be the *condition* of one: `assert(f())` is stripped
/// whole — call included — from a build without asserts, which a statement
/// guarded by a boolean would not be.
///
/// **What an absent entry means** is the load-bearing part: the handle was
/// minted under the first identity [bridge] ever had active. So an app that
/// never swaps — every shipping app, and most tests — records nothing here at
/// all, however many bridges it has.
bool stampHandleBridge(Object handle, FrustrateBridge bridge) {
  final slot = Frustrate._slots[bridge];
  final minted = slot?.identity;
  if (minted != null && !identical(minted, slot!.first)) {
    _mintedUnder[handle] = minted;
  }
  return true;
}

/// Refuse [handle], a handle of [bridge], if what minted it is not what
/// serves [bridge] now. Called only from inside an `assert`; returns true so
/// it can be its condition.
bool checkHandleBridge(Object handle, FrustrateBridge bridge) {
  // Nothing has ever changed identity in this isolate, so every live handle
  // was minted under what is active now, stamped or not. The whole check is
  // this one static load for as long as that holds.
  if (!Frustrate._bridgeSwapped) return true;
  final slot = Frustrate._slots[bridge];
  final minted = _mintedUnder[handle] ?? slot?.first;
  if (!identical(minted, slot?.identity)) {
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

/// Where generated bindings find their runtime: [of] with their bridge.
///
/// ## One Rust runtime, any number of bridges
///
/// An isolate (a page, on web) loads one library and runs one Rust runtime:
/// one std, one allocator, one worker pool, one memory. Every bridge linked
/// into that library is served by it — natively each through its own
/// `FrustrateNative.init`, on web all of them by the one module
/// `FrustrateWeb.init` loads. A bridge built into a *different* library cannot
/// join: its Rust runtime would be a second one, with its own pool and its own
/// handle registry. The platform init refuses it, naming the fix (compose the
/// bridges into one library with `frustrate_library`).
///
/// ## Installed versus active, per bridge
///
/// Each bridge has two slots, answering two different questions.
///
/// **Installed** is the platform runtime for that bridge. It is first-wins and
/// never replaced, and the transport behind it is never rebuilt — a second
/// native transport would re-register the isolate-exit listener on the
/// process-global port and break the callback sweep, and a web reset that
/// blanked the slot would let a later init reap the live runtime's pool
/// workers. [isInstalled] reports whether a transport exists at all, and the
/// web transport's orphan reap depends on that meaning.
///
/// **Active** is where that bridge's generated bindings send calls: what [of]
/// returns. [install] fills it when it is empty, and [activate]/[reset] move
/// it. That is what lets a test put a fake in front of one bridge — the others
/// stay on the real library — and a production build put a tracing decorator
/// there (package:frustrate/intercept.dart), without either one having to
/// reach inside a platform init.
///
/// A swap that **changes identity** ([FrustrateRuntime.bridgeIdentity])
/// requires the outgoing runtime to be quiescent — see [activate]. A swap that
/// keeps it does not, because nothing is at stake: handles, actor hosts and
/// channels are all stamped with an identity, and a decorator keeps it.
///
/// ## The repeated-init contract
///
/// A platform init may be called more than once, and what happens then is
/// decided by *what the call names*:
///
///   * **The same library again — a declared no-op.** Nothing is constructed,
///     nothing is replaced, the call returns. Init is an *ensure*, and the
///     shapes that reach it twice are ordinary: an `initBridge()` helper called
///     from `main()` and from a test's `setUp`, two widgets each making sure
///     the bridge is up, a `package:test` file initializing per group.
///
///   * **Another bridge in the same library — attached.** It joins the
///     installed transport: same completion port and call-id sequence, with
///     pending calls and a stream router of its own.
///
///   * **A different library — a [StateError].** The first transport stays
///     installed and keeps serving, and the bridge named would be answered by
///     a runtime it was not built into.
///
/// The guard runs **before** a transport is constructed ([initializedFrom]),
/// not merely before it is installed, and that is load-bearing rather than
/// tidy — see the note there.
///
/// ### Init while a fake is active
///
/// A platform init that runs with **nothing installed and its bridge already
/// served** — a test that declared a fake (package:frustrate/testing.dart) and
/// a later code path lazily ensuring the bridge is up — returns having built
/// nothing, and the fake keeps serving. That is "init is an ensure" applied to
/// the case where the ensure is already satisfied by something that is not a
/// platform runtime: a widget whose first build calls `FrustrateNative.init`
/// must not load a library that has no reason to exist under `flutter test`.
///
/// Once a transport **is** installed, the repeated-init rules above apply
/// unchanged, so "real A installed, fake active, init names a library that
/// is not A's" is still a [StateError].
///
/// ## Hot restart
///
/// A repeated init is not how hot restart reaches the bridge: these are static
/// fields — per isolate on the VM, per heap on web — and hot restart resets
/// exactly that, so a restarted app runs its *first* init. The web transport
/// depends on it: `_reapOrphanedWorkers` treats `!isInstalled` as "the first
/// init of this heap" (hot_restart_workers_test.dart).
class Frustrate {
  static final Map<FrustrateBridge, _BridgeSlot> _slots = {};

  /// The last bridge [of] answered and its runtime: the cache that makes the
  /// lookup one identity compare in an app whose calls name one bridge at a
  /// time. Generated code names a `const`, so the same bridge is the same
  /// object. Cleared by every slot change.
  static FrustrateBridge? _lastBridge;
  static FrustrateRuntime? _lastRuntime;

  /// The identity of the installed transport — every platform runtime
  /// [install] accepts belongs to it.
  static Object? _transport;

  /// Whether any slot's identity has ever moved *away* from a non-null value —
  /// to a different one, or to nothing.
  ///
  /// The fast path for the debug check: while this is false, every live handle
  /// was minted under what is active now, so [checkHandleBridge] needs no side
  /// table. "Or to nothing" is not tidiness — `activate(F)`, mint, `reset()`
  /// with nothing installed, `activate(G)` leaves F's handles facing G, and
  /// treating the trip through null as "no swap" would let them through.
  ///
  /// Read only from inside an `assert`. It is still *written* in a build
  /// without asserts, which costs one static word per isolate and nothing per
  /// handle.
  static bool _bridgeSwapped = false;

  /// What the installed transport was built from, and how to name it in a
  /// disagreement. Comparison is `==` on an opaque value each transport
  /// chooses (a `BridgeLibrary`, a module identity), so this file stays
  /// platform-neutral.
  ///
  /// Recorded by [install] — i.e. only once a transport really exists. An init
  /// that failed partway (a library that would not open, a runtime ABI
  /// mismatch, a module that would not instantiate) therefore leaves the slot
  /// completely clean, so the next attempt is a first init and not a
  /// disagreement with a bridge that was never installed.
  static Object? _source;
  static String? _sourceDescription;

  /// The runtime [bridge]'s generated bindings send calls to. Throws a
  /// [StateError] when nothing serves it.
  static FrustrateRuntime of(FrustrateBridge bridge) {
    if (identical(bridge, _lastBridge)) return _lastRuntime!;
    return _ofSlow(bridge);
  }

  static FrustrateRuntime _ofSlow(FrustrateBridge bridge) {
    final runtime = _slots[bridge]?.active;
    if (runtime == null) throw _notServed(bridge);
    _lastBridge = bridge;
    _lastRuntime = runtime;
    return runtime;
  }

  static StateError _notServed(FrustrateBridge bridge) {
    final source = _sourceDescription;
    if (source == null) {
      return StateError(
        'Frustrate has not been initialized; call the platform init '
        '(on native, FrustrateNative.init with the generated bridge library) '
        'before using generated bindings',
      );
    }
    return StateError(
      'frustrate: $bridge is not initialized. This isolate runs the library '
      'loaded from $source; on native, pass the generated bridge library of '
      '${bridge.crate} to FrustrateNative.init, and on web, link the bridge '
      'into the module FrustrateWeb.init loads. Bridges used together are '
      'composed into one library with frustrate_library.',
    );
  }

  /// The active runtime when exactly one bridge is active — the shape of
  /// every app with one bridge. With several, name the one you mean with [of].
  static FrustrateRuntime get instance {
    final active = activeRuntimes.toList();
    if (active.length == 1) return active.single;
    if (active.isEmpty) {
      throw StateError(
        'Frustrate has not been initialized; call the platform init '
        '(on native, FrustrateNative.init with the generated bridge library) '
        'before using generated bindings',
      );
    }
    throw StateError(
      'frustrate: ${active.length} bridges are active '
      '(${[for (final r in active) r.servedBridge].join(', ')}), so '
      'Frustrate.instance cannot tell which one you mean. Name it: '
      'Frustrate.of(frustrateBridge), with the frustrateBridge of the '
      'bindings you want.',
    );
  }

  /// Every active runtime, one per bridge that has one.
  static Iterable<FrustrateRuntime> get activeRuntimes => [
    for (final slot in _slots.values)
      if (slot.active case final runtime?) runtime,
  ];

  /// [FrustrateRuntime.bridgeIdentity] of [bridge]'s active runtime, or null
  /// when nothing serves it.
  ///
  /// A static, deliberately: the debug checks read it, and a getter that went
  /// through the active runtime would make them a virtual call on an
  /// interface that a decorator makes polymorphic.
  static Object? identityOf(FrustrateBridge bridge) => _slots[bridge]?.identity;

  /// Whether a **platform transport** is installed in this isolate.
  ///
  /// Diagnostic, and the web transport's "is this heap's first init" test —
  /// which is why it reports the installed transport and not what is active. A
  /// page running a fake has never built a wasm module, so the orphaned
  /// workers of a previous heap are still that init's to reap.
  static bool get isInstalled => _transport != null;

  /// The repeated-init decision, taken by a platform init **before it
  /// constructs anything**.
  ///
  /// Returns true when a transport built from [source] is already installed:
  /// the caller must return immediately, having built nothing. With nothing
  /// installed it returns whether [bridge] is already served (by a fake — see
  /// "Init while a fake is active" on [Frustrate]); without a [bridge] that is
  /// false, and the caller decides once it knows which bridges it would
  /// serve. Throws a [StateError] when a transport built from something else
  /// is installed; [description] is how this call's library is named in that
  /// message.
  ///
  /// **Deciding here rather than inside [install] is load-bearing.** [install]
  /// is idempotent, but its argument is evaluated first, so an init that got
  /// that far would build a second transport and discard it. On native the
  /// discarded runtime has already registered an isolate-exit listener, and
  /// re-registering on the same port *replaces* the response value — the
  /// isolate would then report an id no live callback invocation carries and
  /// the sweep would miss every one of them.
  static bool initializedFrom(
    Object source,
    String description, {
    FrustrateBridge? bridge,
  }) {
    final installed = _source;
    if (installed == null) {
      return bridge != null && isServed(bridge);
    }
    if (installed == source) return true;
    throw StateError(
      'frustrate: already initialized from $_sourceDescription, and this '
      'call asks for $description. An isolate (a page, on web) runs one Rust '
      'runtime, loaded once, and every call keeps going to it. A bridge '
      'built into a separate library cannot join it: compose the bridges '
      'into one library with frustrate_library and initialize each from '
      'that. Calling init again with the SAME library is fine and does '
      'nothing.',
    );
  }

  /// Whether something answers [bridge]'s calls.
  static bool isServed(FrustrateBridge bridge) =>
      _slots[bridge]?.active != null;

  /// Install the platform runtime for its [FrustrateRuntime.servedBridge],
  /// recording what the transport was built from so a later init can be
  /// answered by [initializedFrom].
  ///
  /// Idempotent per bridge. Every runtime it accepts belongs to one transport
  /// (its [FrustrateRuntime.bridgeIdentity]): one from another transport is
  /// ignored. That first-wins is the backstop for two inits racing (web's is
  /// asynchronous, so two concurrent first inits can both pass
  /// [initializedFrom]); the ordinary sequential case never reaches it,
  /// because the guard already sent the second call home.
  static void install(
    FrustrateRuntime runtime, {
    required Object source,
    required String description,
  }) {
    final transport = runtime.bridgeIdentity;
    if (_transport == null) {
      _transport = transport;
      _source = source;
      _sourceDescription = description;
    } else if (!identical(_transport, transport)) {
      return;
    }
    final slot = _slotFor(runtime.servedBridge);
    if (slot.installed != null) return;
    slot.installed = runtime;
    // Only when nothing is active. A platform init that runs while a fake is
    // active must not silently repoint the bindings at the real bridge
    // mid-suite; it fills the installed slot so [reset] has somewhere to go,
    // and the fake keeps serving until the caller says otherwise.
    if (slot.active == null) {
      _setActive(slot, runtime, 'install');
    }
  }

  static _BridgeSlot _slotFor(FrustrateBridge bridge) =>
      _slots[bridge] ??= _BridgeSlot(bridge);

  /// Make [runtime] what its bridge's bindings call — a fake for a test, a
  /// decorator for tracing or metrics. Other bridges are untouched.
  ///
  /// Refused with a [StateError] unless that bridge's active runtime is empty
  /// or its installed platform runtime. **One active runtime per bridge,
  /// declared**: composing a decorator over a fake is
  /// `activate(Traced(fake))`, written by the caller in the order they meant,
  /// rather than a stack this assembled behind their back from two
  /// independent `initBridge()` helpers.
  ///
  /// **Quiescence.** When [runtime] has a different identity from the outgoing
  /// one ([FrustrateRuntime.bridgeIdentity]), the outgoing runtime must owe no
  /// completion and hold no open channel, or this throws naming what is left
  /// (including [FrustrateRuntime.openChannelLabels]). Both are things a swap
  /// would strand: an in-flight call answers into a transport nothing points
  /// at any more, and a Rust producer keeps feeding a channel whose consumer
  /// has been swapped out from under it. Worse, a call that settles after the
  /// swap decodes its response — and a handle in that response would be minted
  /// with a raw from one runtime and stamped with another's identity. A
  /// platform runtime counts only its own bridge's work, so faking one bridge
  /// does not wait for another.
  ///
  /// When [runtime] keeps the identity — a decorator going on — nothing is
  /// stranded and nothing is checked. Handles, hosts and channels are stamped
  /// with an identity, not with a runtime object, so a live app with an open
  /// stream can gain and lose a tracing decorator freely. Keying the rule on
  /// the runtime object instead would make a decorator uninstallable in
  /// exactly the app that wanted one.
  ///
  /// **A build with asserts off allows one identity per bridge, and refuses
  /// any change to it.** The per-handle stamp that tells one runtime's handle
  /// from another's lives inside `assert`s, so such a build carries none of
  /// it; the cause is refused there instead of the consequence being caught,
  /// by a [StateError] naming both. Both `activate` and [reset] are held to
  /// that rule. So activating a *fake* needs asserts on — `dart test`,
  /// `flutter test` and every debug app already have them (`--enable-asserts`
  /// otherwise). A decorator is never refused, because `DelegatingRuntime`
  /// and `DelegatingActorHost` forward [FrustrateRuntime.bridgeIdentity].
  ///
  /// **What it does not do** is invalidate the outgoing runtime's handles.
  /// They cannot be enumerated (a handle collected without `dispose()` never
  /// reports), so where asserts are on each one refuses at the point of use and
  /// says so ([staleBridgeHandle]); they stay disposable through the runtime
  /// that minted it, and become usable again when it is active.
  static void activate(FrustrateRuntime runtime) {
    final slot = _slotFor(runtime.servedBridge);
    final outgoing = slot.active;
    if (outgoing != null && !identical(outgoing, slot.installed)) {
      throw StateError(
        'frustrate: $outgoing is already active, so activating $runtime '
        'would stack one runtime on another. Frustrate.reset() first, or '
        'compose them yourself and activate the composition — there is one '
        'active runtime per bridge and it is chosen in one place.',
      );
    }
    _requireQuiescent(outgoing, runtime.bridgeIdentity, 'activate');
    _setActive(slot, runtime, 'activate');
  }

  /// Put each bridge's installed platform runtime back in front of its
  /// bindings, or empty its active slot when nothing is installed — every
  /// bridge, or only [bridge]. Idempotent.
  ///
  /// Never rebuilds a transport: the installed one is retained for the life of
  /// the isolate precisely so this can restore it. A test that wants a
  /// genuinely fresh platform transport spawns an isolate — the shape
  /// `init_contract_test.dart` already uses.
  ///
  /// Held to both of [activate]'s rules, and for the same reasons: quiescence,
  /// and — where asserts are off — one identity per bridge, which a reset away
  /// from a fake that was activated first would change. Every bridge it
  /// resets is checked before any is changed.
  static void reset([FrustrateBridge? bridge]) {
    final slots = [
      for (final slot in _slots.values)
        if (bridge == null || slot.bridge == bridge)
          if (slot.active != null) slot,
    ];
    for (final slot in slots) {
      _requireQuiescent(slot.active, slot.installed?.bridgeIdentity, 'reset');
      _refuseIdentityChange(slot, slot.installed, 'reset');
    }
    for (final slot in slots) {
      _setActive(slot, slot.installed, 'reset');
    }
  }

  /// The one door to a slot's active runtime — [install], [activate] and
  /// [reset] all come through here, so the identity bookkeeping cannot be
  /// bypassed by a fourth writer added later.
  ///
  /// Note what routing [install] through here catches that a check written
  /// inside [activate] would not: `activate(fake)`, `reset()` to nothing, then
  /// a real platform init. That is null → a new identity with handles from the
  /// fake still alive, and it is refused — after the init built its transport,
  /// which is the price of deciding here rather than earlier, and is a corner
  /// only reachable in a build without asserts that ran a fake.
  static void _setActive(
    _BridgeSlot slot,
    FrustrateRuntime? runtime,
    String verb,
  ) {
    final incoming = runtime?.bridgeIdentity;
    // Decide before mutating: a refused swap is not a swap, and must leave
    // every slot as it found them.
    _refuseIdentityChange(slot, runtime, verb);
    if (!identical(incoming, slot.identity) && slot.identity != null) {
      _bridgeSwapped = true;
    }
    slot.first ??= incoming;
    slot.active = runtime;
    slot.identity = incoming;
    _lastBridge = null;
    _lastRuntime = null;
  }

  /// **Refuse an identity change in a build without asserts**, which is what
  /// lets the per-handle stamp be debug-only. The argument, in one line: the
  /// thing a stale handle does is dereference a raw on a transport that never
  /// issued it, and if a bridge's identity cannot change then no handle can
  /// ever meet one. So instead of catching the consequence at every use in
  /// every build, the cause is refused once, here, in the build that could not
  /// have caught it.
  ///
  /// Production is unaffected, because the only swap a shipping app performs
  /// is putting a decorator on or taking it off, and a decorator forwards
  /// [FrustrateRuntime.bridgeIdentity]. What is refused is activating a
  /// *fake*, which is a test doing it in a vehicle that turned asserts off.
  static void _refuseIdentityChange(
    _BridgeSlot slot,
    FrustrateRuntime? runtime,
    String verb,
  ) {
    final incoming = runtime?.bridgeIdentity;
    final first = slot.first;
    if (incoming == null || first == null || identical(incoming, first)) {
      return;
    }
    if (_assertsEnabled) return;
    throw StateError(
      'frustrate: $verb would make $runtime active for ${slot.bridge}, which '
      'is a different runtime from the one it started on — and this build '
      'has asserts off, so it cannot tell a handle minted by one from a '
      'handle minted by the other. That check is the per-handle stamp, which '
      'costs nothing here precisely because a build like this one cannot '
      'reach such a change at all. Sending a raw to a transport that never '
      'issued it is not something the wire can catch, so the swap is refused '
      'instead of being allowed to become a bad dereference later. Run with '
      'asserts on (--enable-asserts; dart test and flutter test already do) '
      'if this is a test. A tracing or metrics decorator is not such a change '
      'and is always allowed.',
    );
  }

  /// The shared half of [activate] and [reset]: a swap that changes identity
  /// is refused while the outgoing runtime still has live work.
  ///
  /// "In flight" means the transport owes a completion. A response that has
  /// already settled its future but whose caller has not yet resumed to decode
  /// it is not counted — Dart offers no way to observe that, and closing the
  /// gap would mean threading the answering runtime through every generated
  /// decode site rather than reading the active slot at the mint.
  static void _requireQuiescent(
    FrustrateRuntime? outgoing,
    Object? incomingIdentity,
    String verb,
  ) {
    if (outgoing == null) return;
    if (identical(outgoing.bridgeIdentity, incomingIdentity)) return;
    final calls = outgoing.inFlightCallCount;
    final open = outgoing.openChannelCount;
    if (calls == 0 && open == 0) return;
    throw StateError(
      'frustrate: $verb would change which runtime serves '
      '${outgoing.servedBridge} while $outgoing still has $calls call(s) in '
      'flight and $open open channel(s) ${outgoing.openChannelLabels}. An '
      'in-flight call would answer into a transport nothing points at, and a '
      'handle in its response would be minted against the wrong runtime. '
      'Await the calls and end the streams first.',
    );
  }
}

/// One bridge's two slots ([Frustrate]) and the identity bookkeeping beside
/// them.
final class _BridgeSlot {
  final FrustrateBridge bridge;

  /// The platform runtime for this bridge, once installed. Retained for the
  /// life of the isolate even while something else is active: [Frustrate.reset]
  /// restores it, and a handle minted under it stays disposable through it the
  /// whole time.
  FrustrateRuntime? installed;

  /// What [Frustrate.of] returns for this bridge.
  FrustrateRuntime? active;

  /// [FrustrateRuntime.bridgeIdentity] of [active], cached so the debug-only
  /// checks are a load and an identity compare rather than a virtual call.
  /// Null exactly when nothing is active.
  Object? identity;

  /// The first identity this bridge ever had active. *In a build without
  /// asserts* it is the only one allowed ([Frustrate._refuseIdentityChange]);
  /// with asserts on, it is what an unstamped handle of this bridge was minted
  /// under ([stampHandleBridge]). Never cleared, because a `reset` to nothing
  /// does not un-mint the handles that exist.
  Object? first;

  _BridgeSlot(this.bridge);
}
