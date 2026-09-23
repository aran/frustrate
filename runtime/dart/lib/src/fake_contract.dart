/// The vocabulary a generated fake harness is written against — and nothing
/// else. See the library doc on package:frustrate/fake_contract.dart for why
/// this is a separate library from the fake that implements it.
library;

import 'dart:typed_data';

import 'binary_codec.dart';
import 'envelope.dart';
import 'exceptions.dart';

/// The generated half of a fake bridge: it answers requests as bytes.
///
/// One of these is emitted into each per-platform binding surface, wrapping
/// the user's typed fake. It decodes a request with the same walkers the
/// client encoder uses, calls the user's method, and encodes the answer as a
/// **response envelope** — the identical bytes the real library would send —
/// so nothing downstream of [decodeEnvelope] has a second code path for a
/// fake. Statuses are [statusOk], [statusError], [statusPanic],
/// [statusContention] and [statusTypedError].
///
/// Its two methods split on the calling convention, not on where the work
/// runs: [answerSync] serves the `fn_id`s of `#[bridge(sync)]` members and
/// must return in the same turn (that is what a sync member *is*), and
/// [answerAsync] serves everything else — pool functions, Rust `async fn`s,
/// and every actor method including the synthetic drop. The two id sets are
/// disjoint by construction.
///
/// Neither takes the call's `deferred` flag, its cancel token, or the actor
/// host: deferral and cancellation are the fake transport's business (it owns
/// the queue and the pending calls), and an actor method carries its receiver
/// handle in the request like any other method, so one handle registry
/// resolves opaques, actors and trait objects alike.
abstract base class FakeBridge {
  /// The generated `frustrateSchemaHash` of the surface this harness came
  /// from. A `FrustrateRuntime` built on this bridge accepts exactly this
  /// value from `checkFrustrateSchema()`, so a harness generated from crate A
  /// cannot sit under crate B's bindings and answer plausible-looking bytes.
  final BigInt schemaHash;

  FakeBridge(this.schemaHash);

  FakeWire? _wire;

  /// The inbound half of the fake transport: how this harness delivers stream
  /// items, invokes Dart closures the request registered, and mints or
  /// resolves handles.
  ///
  /// Set once, when this bridge is installed behind a fake runtime. Reading it
  /// before that — or installing one bridge behind two runtimes — is a loud
  /// error rather than a silently half-wired harness.
  FakeWire get wire =>
      _wire ??
      (throw StateError(
        'frustrate: this FakeBridge is not installed behind a fake runtime '
        'yet, so it has no wire to answer on. Construct the runtime with it '
        '(FakeRuntime(bridge)) before anything calls into it.',
      ));

  /// Called once by the fake runtime that takes ownership of this bridge.
  /// Not application API.
  void bindWire(FakeWire wire) {
    if (_wire != null) {
      throw StateError(
        'frustrate: this FakeBridge is already installed behind a fake '
        'runtime. One bridge answers for one runtime — its handle registry '
        'and its open channels belong to that runtime. Build a second bridge '
        'for a second runtime.',
      );
    }
    _wire = wire;
  }

  /// Answer a synchronous call. Returns the full response envelope.
  Uint8List answerSync(int fnId, BinaryReader request);

  /// Answer an asynchronous call — a pool function, a Rust `async fn`, or an
  /// actor method. Returns the full response envelope.
  Future<Uint8List> answerAsync(int fnId, BinaryReader request);
}

/// The fake transport as a generated harness sees it: the Rust→Dart direction,
/// plus the handle registry.
///
/// Implemented by the fake runtime in package:frustrate/testing.dart. It is
/// declared *here*, apart from that implementation, so that a binding surface
/// can carry its harness without importing a second `FrustrateRuntime` — see
/// the library doc on package:frustrate/fake_contract.dart.
abstract interface class FakeWire {
  /// Deliver one item to the Dart object registered under [channelId], on
  /// method [selector] (0 is the primary method — `add`, or a callback's
  /// invocation).
  ///
  /// Returns false when the channel is no longer open — the consumer
  /// cancelled it, or it was already closed — which is exactly what Rust's
  /// non-blocking `StreamSink::add` answers, and the signal a fake producer
  /// should stop on. Delivery itself is never synchronous with this call: it
  /// lands on a microtask, as it does on both real platforms.
  bool deliver(int channelId, int selector, BinaryWriter item);

  /// Close the channel normally (`onDone` on the consumer's side).
  void endChannel(int channelId);

  /// End the channel with an error the consumer sees as a [BridgeException] —
  /// the terminal a Rust body that returned `Err` produces.
  void failChannel(int channelId, String message);

  /// Whether [channelId] is still registered.
  bool isChannelOpen(int channelId);

  /// Whether the consumer has paused its subscription — a `StreamController`
  /// parameter whose subscription is paused (`pauseStream`). A fake producer
  /// honours it the way a Rust one awaiting `StreamSink::send` does; the
  /// non-blocking [deliver] is unaffected, as `add` is on the real side.
  bool isChannelPaused(int channelId);

  /// Invoke the value-returning Dart closure registered under [channelId] and
  /// wait for its reply envelope (decode it with [decodeEnvelope]).
  ///
  /// A `Future`, where the Rust side blocks a pool worker: a fake has no
  /// worker to park. That is a declared difference from native physics, and
  /// the reason a returning mirror reaches a fake as
  /// `Future<R> Function(T)` rather than `R Function(T)`.
  Future<Uint8List> invokeChannel(int channelId, BinaryWriter args);

  /// Register [object] under a fresh raw handle value and return it. The raw
  /// is what the response encodes where a handle sits; the client's decode
  /// turns it back into a generated handle class.
  int mintHandle(Object object);

  /// The object [raw] was minted for.
  ///
  /// Throws a [StateError] for a raw this wire never issued or has already
  /// retired — the fake's stand-in for the dereference a real transport would
  /// perform, and loud for the same reason.
  Object resolveHandle(int raw);

  /// Retire [raw] — what the synthetic actor drop does. Opaque handles retire
  /// through their `HandleDrop` instead, which the fake runtime owns.
  void retireHandle(int raw);
}

/// One request being answered: which wire the handles it carries belong to,
/// what to label them, and which channels it has opened so far.
///
/// The fake's counterpart to `FrustrateOpenScope`, and it exists for the
/// mirror-image reason. On the real bridge a channel ends when the Rust body
/// *drops* the sink or closure it was given: a body that uses one and returns
/// ends it, a body that stores one keeps it. Dart has no `Drop`, so the
/// harness cannot see which the fake did — it ends every channel the request
/// opened when the call answers, and a fake that means to keep feeding one
/// says so with [FakeStreamSink.retain], which is the same declaration storing
/// the value is in Rust.
///
/// **A Dart *closure* cannot be retained**, because a closure has nowhere to
/// hang the declaration. Calling one after its call has answered throws rather
/// than silently going nowhere; a test that needs the stored-callback shape
/// runs against the real library.
final class FakeRequest {
  /// The transport the handles in this request belong to.
  final FakeWire wire;

  /// The member being answered (`'TextDoc.watch'`), for attribution.
  final String label;

  final List<int> _opened = [];
  final Set<int> _retained = {};

  FakeRequest(this.wire, this.label);

  /// Record [id] as opened by this request and return it, so a generated
  /// decode can note it inline.
  int track(int id) {
    _opened.add(id);
    return id;
  }

  /// Keep [id] open past this call. [FakeStreamSink.retain] is the way a fake
  /// says this; nothing else should need to.
  void retain(int id) => _retained.add(id);

  /// End every channel this request opened and nobody retained. Called by the
  /// generated arm when the call answers, however it answers.
  void retire() {
    for (final id in _opened) {
      if (!_retained.contains(id)) wire.endChannel(id);
    }
  }
}

/// The write end of a Rust → Dart stream, as a fake producer holds it.
///
/// The Dart twin of `frustrate::testing::StreamProbe`: the client passed a
/// `Sink`/`EventSink`/`StreamController` and the generated harness hands the
/// fake one of these bound to the channel that parameter opened.
final class FakeStreamSink<T> {
  final FakeRequest _req;
  final int _id;
  final void Function(BinaryWriter w, T value) _encode;

  /// Whether the mirror the client passed declares `addError` (selector 1) —
  /// an `EventSink` or a `StreamController`, not a plain `Sink` and not a
  /// closure.
  ///
  /// Carried so [addError] can refuse *here*, synchronously, at the fake's
  /// own call site. Sending selector 1 to a one-method registration throws
  /// inside the router's dispatch instead, on a microtask, where there is no
  /// caller left to attribute it to.
  final bool _hasAddError;

  FakeStreamSink(this._req, this._id, this._encode, {required bool hasAddError})
    : _hasAddError = hasAddError;

  /// The channel id this sink feeds, as it appears in the request and in
  /// `openChannelLabels`.
  int get channelId => _id;

  /// Whether the consumer cancelled, or this sink was closed. The negation of
  /// [FakeWire.isChannelOpen], named for the producer's question.
  bool get isCancelled => !_req.wire.isChannelOpen(_id);

  /// Keep this stream open past the call that handed it over — what a Rust
  /// body does by *storing* the sink rather than dropping it at the end
  /// (`TextDoc::watch`). Without it the harness ends the channel when the call
  /// answers, which is what a body that used the sink and returned does.
  void retain() => _req.retain(_id);

  /// Whether the consumer paused its subscription ([FakeWire.isChannelPaused]).
  bool get isPaused => _req.wire.isChannelPaused(_id);

  /// Deliver one item. Returns false once the channel is gone, exactly as
  /// Rust's non-blocking `add` does — a fake producer should stop rather than
  /// keep feeding.
  ///
  /// **Cancellation is tested before the encode**, as `StreamSink::add` tests
  /// it before encoding and for the same reason: an item that reaches an opaque
  /// handle is *minted* by `_encode` (`_fakeWire.mintHandle`), so encoding into
  /// a channel that is already gone would register an object with nothing left
  /// to retire it. The fake needs no ledger beyond this — it is synchronous, so
  /// there is nothing that could cancel between this test and the delivery —
  /// which is the window the real runtime's mint ledger covers. What the fake
  /// *does* share is the other absorb path: its router defers delivery, so an
  /// item routed after a cancel reaches the tombstone reclaim exactly as the
  /// real one does.
  bool add(T value) {
    if (isCancelled) return false;
    final w = BinaryWriter();
    _encode(w, value);
    return _req.wire.deliver(_id, 0, w);
  }

  /// Deliver a non-terminal error (`addError`), which the consumer receives as
  /// a [BridgeException]. Only for a mirror that declares one.
  bool addError(String message) {
    if (!_hasAddError) {
      throw StateError(
        'frustrate: ${_req.label} was passed a Sink (or a callback), which has no '
        'addError — only an EventSink or a StreamController does. A fake '
        'cannot deliver one where the real bridge could not either; end the '
        'stream with fail() if the producer is reporting a failure.',
      );
    }
    final w = BinaryWriter();
    w.writeString(message);
    return _req.wire.deliver(_id, 1, w);
  }

  /// Close the stream normally. Idempotent.
  void close() => _req.wire.endChannel(_id);

  /// End the stream with an error — what a Rust body that returned `Err` after
  /// handing out a sink produces. Idempotent.
  void fail(String message) => _req.wire.failChannel(_id, message);
}

/// The response envelope for a throw that came out of a fake's method body,
/// where the member did not declare that error as a value.
///
/// The three bridge exceptions map back to the status that carries them, so a
/// fake that wants to exercise a caller's `on ContentionException` throws one
/// and the caller catches the same type it would from the real library.
/// Anything else — an `UnimplementedError` from a fake member nobody
/// overrode, a bug in the fake — becomes a panic envelope **naming the
/// member**, which is where a fake's mistakes would otherwise arrive as an
/// anonymous decode failure.
///
/// A member's *declared* typed error never reaches here: the generated arm
/// catches that exception itself, because only it knows how to encode the
/// value.
Uint8List fakeThrown(Object error, String member) {
  final (int status, String message) = switch (error) {
    // Checked before BridgeException: the generated typed-error classes
    // extend it, and a panic/contention is neither.
    BridgePanicException e => (statusPanic, e.message),
    ContentionException e => (statusContention, e.message),
    BridgeException e => (statusError, e.message),
    _ => (
      statusPanic,
      'frustrate: the fake for $member threw, and $member does not declare '
          'that failure as a value, so it crosses as a panic would: $error',
    ),
  };
  final w = BinaryWriter(message.length + 9)
    ..writeU8(status)
    ..writeString(message);
  return w.takeBytes();
}
