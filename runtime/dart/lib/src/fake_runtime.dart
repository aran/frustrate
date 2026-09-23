/// A `FrustrateRuntime` with no library behind it: requests go to a
/// [FakeBridge] instead of to a wasm module or a `.so`.
///
/// Not part of the default package surface — see the library doc on
/// package:frustrate/testing.dart.
library;

import 'dart:async';
import 'dart:typed_data';

import 'binary_codec.dart';
import 'envelope.dart';
import 'fake_contract.dart';
import 'pending_calls.dart';
import 'runtime_core.dart';
import 'stream_router.dart';

/// The transport half of a fake bridge: everything a real transport does with
/// bytes, without a library to send them to.
///
/// It owns exactly what a real transport owns — the id sequence, the pending
/// calls, the stream router, the handle registry, the actor executors — and
/// delegates only "what does this request mean" to its [FakeBridge], which a
/// generated harness supplies. So cancellation, deferred completions, FIFO
/// actor ordering, backpressure signalling, channel bookkeeping and the
/// never-throw-synchronously contract are all exercised here rather than
/// re-implemented per fake.
///
/// ```dart
/// final fake = FakeRuntime(FakeTestApiBridge(MyApi()));
/// Frustrate.activate(fake);
/// addTearDown(Frustrate.reset);
/// ```
///
/// **What it does not model, and why.** These are physics of the real
/// implementation, not of the interface, so a test that needs them runs
/// against the real library (under Bazel that is one `initBridge()`):
///
///   * **Lock contention.** A fake method is a Dart method; nothing can hold a
///     `RwLock` against it. A fake may still *throw* [ContentionException] to
///     drive a caller's handler.
///   * **Rust's codec.** A fake exercises the Dart half of the wire in both
///     directions and nothing else; that a Rust `Vec<u64>` decodes the bytes
///     the same way is what the cross-language suite is for.
///   * **Parallelism and worker scheduling.** [asyncIsParallel] is false and
///     [hardwareParallelism] is 1 — fixed, not configurable. Nothing here runs
///     on a second thread, so answering otherwise would be a claim the fake
///     cannot honour, and a knob would only let a test assert a lie.
///   * **Web cancel latency.** A claim here is synchronous; on a web actor it
///     is a `postMessage` round trip.
final class FakeRuntime implements FrustrateRuntime, FakeWire {
  /// The harness that decides what a request means.
  final FakeBridge bridge;

  /// One sequence for call ids, channel ids and handle raws alike. The router
  /// requires call and channel ids to share a sequence (see `StreamRouter`);
  /// handles join them so that no two live things in a fake ever answer to the
  /// same number, which is the difference between a confusing test failure and
  /// an obvious one.
  int _ids = 0;

  final InFlightCalls _inFlight = InFlightCalls();

  late final PendingCalls _calls = PendingCalls(
    _allocId,
    // A cancel always claims: the "call" is a Dart future nobody else can
    // settle, so if it is still pending, failing it here is exactly right
    // and cannot race a completion the way the real answer-once gate can.
    cancelCall: (_) => true,
    tally: _inFlight,
  );

  late final StreamRouter _router = StreamRouter(
    _allocId,
    respond: _respond,
    cancel: cancelStream,
    // Never synchronous with the fake's own `add`, matching both platforms:
    // native delivers on a later port turn, and web defers explicitly. A
    // fake that delivered inline would let a test pass on ordering no real
    // transport provides.
    deferDelivery: true,
  );

  final _FakeHandles _handles = _FakeHandles();
  final Map<String, HandleDrop> _drops = {};

  /// Channels this runtime has open, and which of them the consumer paused.
  /// The router knows the first as a private map; these are the fake
  /// producer's questions ([FakeWire.isChannelOpen], [FakeWire.isChannelPaused])
  /// and it has no other way to ask them.
  final Set<int> _open = {};
  final Set<int> _paused = {};

  /// Replies to value-returning closure invocations, keyed by invocation id.
  final Map<int, Completer<Uint8List>> _invocations = {};

  FakeRuntime(this.bridge) {
    bridge.bindWire(this);
  }

  int _allocId() => ++_ids;

  // ------------------------------------------------------------ outbound --

  /// A fake **is** its bridge: raws it minted, channels it registered and
  /// handle drops it hands out are all its own, exactly as a platform
  /// transport's are. So activating one over a real transport is a bridge
  /// change, with everything [Frustrate.activate] says that implies.
  @override
  Object get bridgeIdentity => this;

  @override
  int get inFlightCallCount => _inFlight.count;

  @override
  BinaryReader callSync(
    int fnId,
    int sizeHint,
    void Function(BinaryWriter w) encode, {
    Object Function(BinaryReader)? typedError,
  }) {
    final w = BinaryWriter(sizeHint);
    encode(w);
    return decodeEnvelope(
      bridge.answerSync(fnId, BinaryReader(w.takeBytes())),
      typedError: typedError,
    );
  }

  @override
  Future<BinaryReader> callAsync(
    int fnId,
    int sizeHint,
    void Function(BinaryWriter w) encode, {
    Object Function(BinaryReader)? typedError,
    FrustrateCancelToken? cancel,
  }) =>
      // Encoding happens inside `send`, which is what makes a throwing encoder
      // — a handle from another bridge, `writeU64` out of range, a user
      // `BytesCodec` — reject the returned future instead of escaping
      // synchronously out of a `Future`-returning API.
      _calls.issue(
        (callId) {
          final w = BinaryWriter(sizeHint);
          encode(w);
          answerInto(_calls, callId, fnId, BinaryReader(w.takeBytes()));
        },
        typedError: typedError,
        cancel: cancel,
      );

  /// Run one async answer and settle [callId] with it, on a later turn.
  ///
  /// Shared with [FakeActorHost] so both settle identically. A completion for
  /// a call that is no longer pending — cancelled, or its host shut down — is
  /// dropped: `complete` answers false and there is nothing left to tell.
  void answerInto(
    PendingCalls pending,
    int callId,
    int fnId,
    BinaryReader request,
  ) {
    Future<Uint8List> answer;
    try {
      answer = bridge.answerAsync(fnId, request);
    } catch (e, st) {
      // A harness whose dispatch threw synchronously rather than returning an
      // envelope. Still the call's failure, never this frame's.
      pending.fail(callId, e, st);
      return;
    }
    answer.then(
      (response) => pending.complete(callId, response),
      onError: (Object e, StackTrace st) => pending.fail(callId, e, st),
    );
  }

  @override
  HandleDrop handleDrop(String symbol) =>
      _drops[symbol] ??= _FakeHandleDrop(_handles);

  /// The same hook: a fake owns no Rust object, so there is no `Drop` to place
  /// on a thread and nothing for a model to change here.
  @override
  HandleDrop residentHandleDrop(String symbol) => handleDrop(symbol);

  /// A fake owns no Rust object, so none can be lost.
  @override
  int get residentLeakCount => 0;

  @override
  void checkSchemaHash(BigInt expected) {
    if (expected.toUnsigned(64) != bridge.schemaHash.toUnsigned(64)) {
      throw StateError(
        'frustrate: this fake harness was generated for a different '
        'interface than the bindings calling it (bindings '
        '0x${expected.toUnsigned(64).toRadixString(16)} != harness '
        '0x${bridge.schemaHash.toUnsigned(64).toRadixString(16)}). Both come '
        'from codegen, so regenerate them together.',
      );
    }
  }

  @override
  Future<ActorHost> spawnActorHost({String? debugName}) async =>
      FakeActorHost(this, debugName);

  @override
  bool get asyncIsParallel => false;

  @override
  int get hardwareParallelism => 1;

  @override
  int get openChannelCount => _router.openRegistrationCount;

  @override
  List<String> get openChannelLabels => _router.openChannelLabels;

  // ---------------------------------------------------- channels (Dart →) --

  @override
  int openObject(
    List<StreamItemHandler> methods,
    StreamErrorHandler onError,
    StreamDoneHandler onDone, {
    String? label,
    List<StreamReclaim?>? reclaim,
  }) {
    final id = _router.openObject(
      methods,
      onError,
      onDone,
      label: label,
      reclaim: reclaim,
    );
    _open.add(id);
    return id;
  }

  @override
  int openStream(
    StreamItemHandler onItem,
    StreamErrorHandler onError,
    StreamDoneHandler onDone, {
    String? label,
    List<StreamReclaim?>? reclaim,
  }) => openObject(
    [onItem],
    onError,
    onDone,
    label: label,
    reclaim: reclaim == null || reclaim.isEmpty ? null : [reclaim[0]],
  );

  @override
  int openFunction(
    BinaryWriter Function(BinaryReader args) onInvoke, {
    String? label,
    BinaryWriter? Function(Object error)? onDeclaredError,
    StreamReclaim? reclaim,
  }) {
    final id = _router.openFunction(
      onInvoke,
      label: label,
      onDeclaredError: onDeclaredError,
      reclaim: reclaim,
    );
    _open.add(id);
    return id;
  }

  @override
  void cancelStream(int id) {
    _router.cancelLocal(id);
    _retireChannel(id);
  }

  @override
  void pauseStream(int id) => _paused.add(id);

  @override
  void resumeStream(int id) => _paused.remove(id);

  @override
  void failStream(int id, Object error, StackTrace st) {
    _router.fail(id, error, st);
    _retireChannel(id);
  }

  void _retireChannel(int id) {
    _open.remove(id);
    _paused.remove(id);
  }

  // ----------------------------------------------------- FakeWire (→ Dart) --

  @override
  bool deliver(int channelId, int selector, BinaryWriter item) {
    if (!_open.contains(channelId)) return false;
    _router.deliver(
      channelId,
      _framed([statusStreamItem, selector], item.takeBytes()),
    );
    return true;
  }

  /// `[prefix…][body…]` as one buffer. A fake pays a copy per event that no
  /// transport pays, and it is worth nothing to avoid: the alternative is a
  /// raw-append on [BinaryWriter], whose every method is on a real crossing's
  /// hot path.
  static Uint8List _framed(List<int> prefix, Uint8List body) {
    final out = Uint8List(prefix.length + body.length);
    out.setRange(0, prefix.length, prefix);
    out.setRange(prefix.length, out.length, body);
    return out;
  }

  @override
  void endChannel(int channelId) {
    if (!_open.contains(channelId)) return;
    _retireChannel(channelId);
    _router.deliver(channelId, Uint8List.fromList(const [statusStreamEnd]));
  }

  @override
  void failChannel(int channelId, String message) {
    if (!_open.contains(channelId)) return;
    _retireChannel(channelId);
    final w = BinaryWriter(message.length + 9)
      ..writeU8(statusError)
      ..writeString(message);
    _router.deliver(channelId, w.takeBytes());
  }

  @override
  bool isChannelOpen(int channelId) => _open.contains(channelId);

  @override
  bool isChannelPaused(int channelId) => _paused.contains(channelId);

  @override
  Future<Uint8List> invokeChannel(int channelId, BinaryWriter args) {
    if (!_open.contains(channelId)) {
      return Future.error(
        StateError(
          'frustrate: the fake invoked closure #$channelId, which this '
          'transport has no registration for. A closure whose own call has '
          'answered is reported by the harness before it reaches here, so '
          'this is an id nothing ever opened — a bridge bug.',
        ),
        StackTrace.current,
      );
    }
    final invocationId = _allocId();
    final completer = Completer<Uint8List>();
    _invocations[invocationId] = completer;
    final head = BinaryWriter(10)
      ..writeU8(statusCallbackCall)
      // Selector 0: a returning mirror declares exactly one method; the
      // router asserts it.
      ..writeU8(0)
      ..writeHandle(invocationId);
    _router.deliver(channelId, _framed(head.takeBytes(), args.takeBytes()));
    return completer.future;
  }

  void _respond(int invocationId, Uint8List response) {
    _invocations.remove(invocationId)?.complete(response);
  }

  @override
  int mintHandle(Object object) => _handles.mint(_allocId(), object);

  @override
  Object resolveHandle(int raw) => _handles.resolve(raw);

  @override
  void retireHandle(int raw) => _handles.retire(raw);
}

/// One fake actor executor: a FIFO that runs one call at a time.
///
/// The ordering contract is the real one — a plain method occupies the
/// executor until it completes, a `deferred` method releases it as soon as its
/// body's future exists — implemented with a queue and an `await` instead of a
/// thread or a Worker.
///
/// **Two declared differences from a real executor**, both consequences of
/// Dart having no way to drop a running future:
///
///   * A cancel (or a `dispose()`) that reaches a call **still queued** stops
///     it: the job is skipped and never reaches the harness, which is what the
///     real host's claim of a queued job does. A cancel that reaches a call
///     whose body is **already running** cannot stop the body; it completes
///     into a call nobody holds any more and the completion is dropped. On the
///     real side the Rust future is dropped where it was suspended, so its
///     cleanup runs early — a fake cannot show that.
///   * A GC reap ([attachReaper]) stops this executor but cannot retire the
///     actor's registry entry: only the constructor's arm ever knew which raw
///     belongs to this host, and a reaped handle does not say. An explicitly
///     `dispose()`d actor retires normally, through the synthetic drop.
final class FakeActorHost implements ActorHost {
  final FakeRuntime _rt;
  final String? _debugName;

  late final PendingCalls _pending = PendingCalls(
    _rt._allocId,
    cancelCall: (_) => true,
    tally: _rt._inFlight,
  );

  final List<_FakeJob> _queue = [];
  final Set<int> _deferredInFlight = {};
  bool _running = false;
  bool _stopped = false;

  /// One finalizer per host, holding the host itself as the value: a reap
  /// releases the executor exactly as `frustrate_actor_reap` does.
  late final Finalizer<FakeActorHost> _reaper = Finalizer<FakeActorHost>(
    (h) => h.shutdown(),
  );

  FakeActorHost(this._rt, this._debugName);

  @override
  Object get bridgeIdentity => _rt.bridgeIdentity;

  @override
  Future<BinaryReader> call(
    int fnId,
    int sizeHint,
    void Function(BinaryWriter w) encode, {
    Object Function(BinaryReader)? typedError,
    bool deferred = false,
    FrustrateCancelToken? cancel,
  }) {
    int? issued;
    final future = _pending.issue(
      (callId) {
        // Before encoding, so a call on a disposed actor registers nothing —
        // the same order the real hosts refuse in, and the same StateError.
        if (_stopped) {
          throw StateError('frustrate: actor host was shut down');
        }
        final w = BinaryWriter(sizeHint);
        encode(w);
        issued = callId;
        _queue.add(
          _FakeJob(callId, fnId, BinaryReader(w.takeBytes()), deferred),
        );
        if (!_running) scheduleMicrotask(_pump);
      },
      typedError: typedError,
      cancel: cancel,
    );
    // The send closure ran synchronously inside `issue`, so `issued` is set
    // unless the call was refused before it got an id.
    final id = issued;
    if (deferred && id != null) {
      _deferredInFlight.add(id);
      // .ignore(), not unawaited(): the derived future re-raises the call's
      // own error, which the caller handles on the original one.
      future.whenComplete(() => _deferredInFlight.remove(id)).ignore();
    }
    return future;
  }

  Future<void> _pump() async {
    if (_running) return;
    _running = true;
    try {
      while (_queue.isNotEmpty) {
        final job = _queue.removeAt(0);
        // Claimed while it waited its turn — by its own token, or by a
        // dispose. The real executor releases such a job without running it,
        // and so does this one: the harness never sees the request.
        if (!_pending.ids.contains(job.callId)) continue;
        if (job.deferred) {
          // The executor is released as soon as the body's future exists,
          // which for a Dart fake is as soon as `answerAsync` returns one.
          _rt.answerInto(_pending, job.callId, job.fnId, job.request);
        } else {
          await _settleInOrder(job);
        }
      }
    } finally {
      _running = false;
    }
  }

  /// A plain method holds the executor until it answers.
  Future<void> _settleInOrder(_FakeJob job) async {
    try {
      final response = await _rt.bridge.answerAsync(job.fnId, job.request);
      _pending.complete(job.callId, response);
    } catch (e, st) {
      _pending.fail(job.callId, e, st);
    }
  }

  @override
  Future<void> shutdown() async {
    if (_stopped) return;
    _stopped = true;
    // An outstanding deferred completion is cancelled rather than drained.
    for (final id in _deferredInFlight.toList()) {
      _pending.fail(id, disposedDeferredError(_debugName), StackTrace.current);
    }
    _deferredInFlight.clear();
  }

  @override
  void attachReaper(Object owner) => _reaper.attach(owner, this, detach: owner);

  @override
  void detachReaper(Object owner) => _reaper.detach(owner);

  // One router and one id sequence per fake, as on native: channel plumbing
  // delegates whole.
  @override
  int openObject(
    List<StreamItemHandler> methods,
    StreamErrorHandler onError,
    StreamDoneHandler onDone, {
    String? label,
    List<StreamReclaim?>? reclaim,
  }) =>
      _rt.openObject(methods, onError, onDone, label: label, reclaim: reclaim);

  @override
  int openStream(
    StreamItemHandler onItem,
    StreamErrorHandler onError,
    StreamDoneHandler onDone, {
    String? label,
    List<StreamReclaim?>? reclaim,
  }) => _rt.openStream(onItem, onError, onDone, label: label, reclaim: reclaim);

  @override
  int openFunction(
    BinaryWriter Function(BinaryReader args) onInvoke, {
    String? label,
    BinaryWriter? Function(Object error)? onDeclaredError,
    StreamReclaim? reclaim,
  }) => _rt.openFunction(
    onInvoke,
    label: label,
    onDeclaredError: onDeclaredError,
    reclaim: reclaim,
  );

  @override
  void cancelStream(int id) => _rt.cancelStream(id);

  @override
  void pauseStream(int id) => _rt.pauseStream(id);

  @override
  void resumeStream(int id) => _rt.resumeStream(id);

  @override
  void failStream(int id, Object error, StackTrace st) =>
      _rt.failStream(id, error, st);
}

final class _FakeJob {
  final int callId;
  final int fnId;
  final BinaryReader request;
  final bool deferred;
  _FakeJob(this.callId, this.fnId, this.request, this.deferred);
}

/// The fake's stand-in for the Rust handle registry: raw → the object the
/// harness registered for it.
///
/// **Attachments are counted**, which the real registry has no need to do. A
/// real transport mints a raw per response and Dart never decodes a handle out
/// of a *request*, so one raw has exactly one Dart handle object. A fake
/// inverts that: it decodes requests, and a handle nested in a struct
/// parameter reaches the harness through the ordinary generated `_dec` — which
/// constructs a second `TextDoc` for a raw the caller still holds, with its own
/// finalizer. Retiring on the first finalization would then pull the object out
/// from under the live handle. So an entry survives until every Dart object
/// attached to it is gone, while an explicit `drop` retires it at once —
/// `dispose()` means free it, whoever else is looking.
final class _FakeHandles {
  final Map<int, Object> _objects = {};
  final Map<int, int> _attached = {};

  late final Finalizer<int> finalizer = Finalizer<int>(_release);

  int mint(int raw, Object object) {
    _objects[raw] = object;
    return raw;
  }

  Object resolve(int raw) =>
      _objects[raw] ??
      (throw StateError(
        'frustrate: the fake was handed handle $raw, which it never minted '
        'or has already dropped. On a real transport this is the value a '
        'dangling pointer would have; here it is a fake that returned a '
        'handle it did not mint, or a test that used a handle after '
        'dispose().',
      ));

  void retire(int raw) {
    _objects.remove(raw);
    _attached.remove(raw);
  }

  void attach(int raw) => _attached[raw] = (_attached[raw] ?? 0) + 1;

  void _release(int raw) {
    final n = _attached[raw];
    if (n == null) return; // already retired
    if (n > 1) {
      _attached[raw] = n - 1;
    } else {
      retire(raw);
    }
  }
}

final class _FakeHandleDrop implements HandleDrop {
  final _FakeHandles _handles;

  _FakeHandleDrop(this._handles);

  @override
  void attach(Object owner, int raw) {
    _handles.attach(raw);
    _handles.finalizer.attach(owner, raw, detach: owner);
  }

  @override
  void detach(Object owner) => _handles.finalizer.detach(owner);

  @override
  void drop(int raw) => _handles.retire(raw);
}
