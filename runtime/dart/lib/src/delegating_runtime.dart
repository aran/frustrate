/// The delegating bases behind package:frustrate/intercept.dart. Not part of
/// the default package surface — see the library doc on intercept.dart for why
/// that separation matters.
library;

import 'binary_codec.dart';
import 'pending_calls.dart' show FrustrateCancelToken;
import 'runtime_core.dart';

/// A [FrustrateRuntime] that forwards everything to [inner], with two hooks
/// where every bridge call passes through.
///
/// Subclass it, override [aroundSync] and/or [aroundAsync], and put it in
/// front of the bindings:
///
/// ```dart
/// class Traced extends DelegatingRuntime {
///   Traced(super.inner);
///
///   @override
///   BinaryReader aroundSync(int fnId, BinaryReader Function() next) {
///     final sw = Stopwatch()..start();
///     try {
///       return next();
///     } finally {
///       print('${frustrateMemberNames[fnId]} took ${sw.elapsed}');
///     }
///   }
/// }
///
/// FrustrateNative.init(path);
/// Frustrate.activate(Traced(Frustrate.instance));
/// ```
///
/// **It wraps the installed object; it never constructs a transport.** That is
/// what lets it work on web, whose `WebRuntime._()` is private, and what keeps
/// interception out of both platform inits — no parameter, no knob, no
/// init-time decision.
///
/// **It is the same bridge as [inner]** ([bridgeIdentity] forwards), which is
/// the property that makes it installable on a live app: putting one on and
/// taking it off requires no quiescence and invalidates no handle, no actor
/// and no channel. See [Frustrate.activate].
///
/// **What it does not give you is typed arguments.** The hooks see the fn id,
/// the flags, and the raw request only if they choose to copy it — reaching
/// the decoded values would mean decoding the request a second time on every
/// call. An interceptor that needs the arguments belongs above the generated
/// API, not under it.
///
/// **`next` runs at most once, so this is not a retry seam.** Rust → Dart
/// channels are registered *during* request encoding
/// (`FrustrateOpenScope.track`), so re-running an encoder double-registers
/// every sink and callback the call carries; and `&mut self` members are not
/// idempotent. Retry belongs in the caller, with typed arguments.
class DelegatingRuntime implements FrustrateRuntime {
  /// The runtime this forwards to — ordinarily the installed platform
  /// transport, but any [FrustrateRuntime] works, including another decorator
  /// the caller composed by hand.
  final FrustrateRuntime inner;

  DelegatingRuntime(this.inner);

  /// Wraps every **synchronous** bridge call: a `#[bridge(sync)]` free
  /// function, a sync method on an opaque handle, a sync constructor.
  ///
  /// The default is `next()` and nothing else, so a base with no overrides
  /// costs one virtual call and one closure per call and allocates nothing
  /// else. [fnId] keys the generated `frustrateMemberNames`.
  ///
  /// Whatever [next] throws is the call's own failure and should propagate;
  /// swallowing it hands the caller a reader positioned at nothing.
  BinaryReader aroundSync(int fnId, BinaryReader Function() next) => next();

  /// Wraps every **asynchronous** bridge call: a pool-dispatched `#[bridge] fn`
  /// and a Rust `async fn` on the transport, and — through
  /// [DelegatingActorHost] — every actor method, every deferred completion,
  /// and the synthetic drop an actor's `dispose()` dispatches.
  ///
  /// [host] is non-null exactly for a call on an actor executor, and is the
  /// wrapped host, so an interceptor can group a conversation by instance.
  /// [deferred] and [cancel] are the call's own flags, forwarded so a decision
  /// can be made without decoding anything.
  ///
  /// **An override must reject through the returned future, never throw
  /// synchronously.** That is [FrustrateRuntime.callAsync]'s contract and this
  /// sits inside it: a synchronous throw here would escape `unawaited(…)`,
  /// `Future.wait([…])` and store-then-await-later, which are exactly the
  /// shapes a transport error is least expected in. The base adds no
  /// `Future.sync` to enforce it — that would be an allocation on every call
  /// to catch a mistake a subclass makes once.
  Future<BinaryReader> aroundAsync(
    int fnId,
    Future<BinaryReader> Function() next, {
    bool deferred = false,
    FrustrateCancelToken? cancel,
    ActorHost? host,
  }) => next();

  @override
  BinaryReader callSync(
    int fnId,
    int sizeHint,
    void Function(BinaryWriter w) encode, {
    Object Function(BinaryReader)? typedError,
  }) => aroundSync(
    fnId,
    () => inner.callSync(fnId, sizeHint, encode, typedError: typedError),
  );

  @override
  Future<BinaryReader> callAsync(
    int fnId,
    int sizeHint,
    void Function(BinaryWriter w) encode, {
    Object Function(BinaryReader)? typedError,
    FrustrateCancelToken? cancel,
  }) => aroundAsync(
    fnId,
    () => inner.callAsync(
      fnId,
      sizeHint,
      encode,
      typedError: typedError,
      cancel: cancel,
    ),
    cancel: cancel,
  );

  /// The same bridge as [inner]: a decorator changes who observes a call, not
  /// which library answers it, which raws come back, or which export frees
  /// them.
  @override
  Object get bridgeIdentity => inner.bridgeIdentity;

  @override
  int get inFlightCallCount => inner.inFlightCallCount;

  /// Wrapped, so a `dispose()` and a GC finalization are observable the same
  /// way a call is — and so actor calls reach [aroundAsync].
  ///
  /// Only [inner] memoizes per symbol; this allocates a fresh wrapper per mint,
  /// which is correct because the wrapper holds the memoized hook and so keeps
  /// its finalizer reachable exactly as the handle does.
  @override
  HandleDrop handleDrop(String symbol) =>
      DelegatingHandleDrop(inner.handleDrop(symbol));

  @override
  HandleDrop residentHandleDrop(String symbol) =>
      DelegatingHandleDrop(inner.residentHandleDrop(symbol));

  @override
  int get residentLeakCount => inner.residentLeakCount;

  @override
  Future<ActorHost> spawnActorHost({String? debugName}) async =>
      DelegatingActorHost(
        this,
        await inner.spawnActorHost(debugName: debugName),
      );

  @override
  void checkSchemaHash(BigInt expected) => inner.checkSchemaHash(expected);

  @override
  bool get asyncIsParallel => inner.asyncIsParallel;

  @override
  int get hardwareParallelism => inner.hardwareParallelism;

  @override
  int get openChannelCount => inner.openChannelCount;

  @override
  List<String> get openChannelLabels => inner.openChannelLabels;

  /// Rust → Dart registration. The handlers arrive here as closures, so a
  /// subclass that wants to observe *inbound* traffic — stream items,
  /// callback invocations — wraps them before forwarding.
  ///
  /// The base deliberately does not wrap them. Doing so by default would
  /// allocate a closure per method per registration for every decorator,
  /// including the ones that only ever wanted to time outbound calls; a
  /// subclass that wants it writes four lines and pays for what it uses.
  @override
  int openObject(
    List<StreamItemHandler> methods,
    StreamErrorHandler onError,
    StreamDoneHandler onDone, {
    String? label,
    List<StreamReclaim?>? reclaim,
  }) => inner.openObject(
    methods,
    onError,
    onDone,
    label: label,
    reclaim: reclaim,
  );

  @override
  int openStream(
    StreamItemHandler onItem,
    StreamErrorHandler onError,
    StreamDoneHandler onDone, {
    String? label,
    List<StreamReclaim?>? reclaim,
  }) =>
      inner.openStream(onItem, onError, onDone, label: label, reclaim: reclaim);

  @override
  int openFunction(
    BinaryWriter Function(BinaryReader args) onInvoke, {
    String? label,
    BinaryWriter? Function(Object error)? onDeclaredError,
    StreamReclaim? reclaim,
  }) => inner.openFunction(
    onInvoke,
    label: label,
    onDeclaredError: onDeclaredError,
    reclaim: reclaim,
  );

  @override
  void cancelStream(int id) => inner.cancelStream(id);

  @override
  void pauseStream(int id) => inner.pauseStream(id);

  @override
  void resumeStream(int id) => inner.resumeStream(id);

  @override
  void failStream(int id, Object error, StackTrace st) =>
      inner.failStream(id, error, st);
}

/// One actor executor, seen through the [DelegatingRuntime] that spawned it.
///
/// Actor methods never touch `Frustrate.instance` — a generated actor routes
/// everything through the host it was constructed with — so without this an
/// interceptor would see free functions and opaque members and nothing else.
/// [call] routes through [owner]'s [DelegatingRuntime.aroundAsync] with `host`
/// set to this, which is what makes "one place that sees every bridge call"
/// true rather than nearly true: a plain method, a deferred completion, and
/// the synthetic drop `dispose()` dispatches all arrive there.
class DelegatingActorHost implements ActorHost {
  /// The decorator that spawned this host, and whose funnel its calls run
  /// through.
  final DelegatingRuntime owner;

  /// The real host — a dedicated thread on native, a Worker-hosted wasm
  /// instance on web.
  final ActorHost inner;

  DelegatingActorHost(this.owner, this.inner);

  @override
  Future<BinaryReader> call(
    int fnId,
    int sizeHint,
    void Function(BinaryWriter w) encode, {
    Object Function(BinaryReader)? typedError,
    bool deferred = false,
    FrustrateCancelToken? cancel,
  }) => owner.aroundAsync(
    fnId,
    () => inner.call(
      fnId,
      sizeHint,
      encode,
      typedError: typedError,
      deferred: deferred,
      cancel: cancel,
    ),
    deferred: deferred,
    cancel: cancel,
    host: this,
  );

  @override
  Object get bridgeIdentity => inner.bridgeIdentity;

  @override
  Future<void> shutdown() => inner.shutdown();

  @override
  void attachReaper(Object owner) => inner.attachReaper(owner);

  @override
  void detachReaper(Object owner) => inner.detachReaper(owner);

  @override
  int openObject(
    List<StreamItemHandler> methods,
    StreamErrorHandler onError,
    StreamDoneHandler onDone, {
    String? label,
    List<StreamReclaim?>? reclaim,
  }) => inner.openObject(
    methods,
    onError,
    onDone,
    label: label,
    reclaim: reclaim,
  );

  @override
  int openStream(
    StreamItemHandler onItem,
    StreamErrorHandler onError,
    StreamDoneHandler onDone, {
    String? label,
    List<StreamReclaim?>? reclaim,
  }) =>
      inner.openStream(onItem, onError, onDone, label: label, reclaim: reclaim);

  @override
  int openFunction(
    BinaryWriter Function(BinaryReader args) onInvoke, {
    String? label,
    BinaryWriter? Function(Object error)? onDeclaredError,
    StreamReclaim? reclaim,
  }) => inner.openFunction(
    onInvoke,
    label: label,
    onDeclaredError: onDeclaredError,
    reclaim: reclaim,
  );

  @override
  void cancelStream(int id) => inner.cancelStream(id);

  @override
  void pauseStream(int id) => inner.pauseStream(id);

  @override
  void resumeStream(int id) => inner.resumeStream(id);

  @override
  void failStream(int id, Object error, StackTrace st) =>
      inner.failStream(id, error, st);
}

/// An opaque type's drop hooks, seen through a decorator.
///
/// The third way a bridge crossing happens and the one that is easy to forget:
/// `dispose()` and the GC finalizer both free a Rust object without any call
/// going through [DelegatingRuntime.aroundSync] or
/// [DelegatingRuntime.aroundAsync]. Override [drop] to see the eager path and
/// [attach]/[detach] to see registration.
class DelegatingHandleDrop implements HandleDrop {
  final HandleDrop inner;

  DelegatingHandleDrop(this.inner);

  @override
  void attach(Object owner, int raw) => inner.attach(owner, raw);

  @override
  void detach(Object owner) => inner.detach(owner);

  @override
  void drop(int raw) => inner.drop(raw);
}
