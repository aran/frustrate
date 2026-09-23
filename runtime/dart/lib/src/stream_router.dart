/// Id-routed stream-event delivery. There is one
/// router per isolate (native) or per page (web), and every channel that
/// carries bridge completions delivers into it: the transport itself and
/// each actor host.
///
/// Routing is by id alone, which requires every id on the wire — call ids on
/// any channel, stream and callback ids opened against any of them — to come
/// from ONE sequence (`NativeRuntime._allocId`, isolate-tagged;
/// `WebRuntime._allocId`, tag-0). Registered ids consume their events here;
/// everything else is a call completion and stays on the channel's completer
/// path. Per-channel sequences would collide across channels and this router
/// would swallow one channel's call completion as another's stream event.
///
/// Not part of the public package surface.
library;

import 'dart:async';
import 'dart:typed_data';

import 'binary_codec.dart';
import 'envelope.dart';
import 'runtime_core.dart';

/// One registered Dart object: the selector-keyed dispatch table for the
/// methods Rust may call on it, plus the terminal handlers.
///
/// [methods] is indexed by selector, so a lookup is an array read. The
/// single-method endpoints (a `StreamSink`, a `DartCallback`) are simply the
/// length-1 case — there is no separate code path for them.
final class _Registration {
  final List<StreamItemHandler> methods;

  /// Per-selector reclaim, parallel to [methods]: what to run over an item's
  /// payload when the item is **absorbed** instead of dispatched. Non-null only
  /// where that method's payload can carry an opaque handle — a handle the Rust
  /// side has already minted, whose only Dart wrapper would have been built by
  /// [methods]. Absorbing without it would strand the object.
  ///
  /// Null, and the whole list is null, for every channel of value types, which
  /// is the overwhelming majority: no cost, and nothing kept past cancel.
  final List<StreamReclaim?>? reclaim;
  final StreamErrorHandler onError;
  final StreamDoneHandler onDone;

  /// Where this channel was opened (`'TextDoc.watch'`), supplied by the
  /// generated binding. Carried for attribution only — nothing routes on it.
  /// One canonicalized string literal per open channel; registrations are
  /// per-channel and long-lived, so this is not a per-item cost.
  final String? label;

  _Registration(
    this.methods,
    this.reclaim,
    this.onError,
    this.onDone,
    this.label,
  );
}

final class StreamRouter {
  /// Allocates from the owning channel's call-id sequence, so stream ids,
  /// callback ids, and call ids never collide on one channel.
  final int Function() _nextId;

  /// Deliver one callback response to the Rust side — the mpsc wake for a
  /// blocking `call`, or the waker `Slot` fill for an awaited `call_async`.
  /// Every transport that can carry a DartFunction member provides it; a
  /// function event on a channel without it is a bridge bug.
  final void Function(int invocationId, Uint8List response)? _respond;

  /// Run *every* delivery — stream items, void mirror methods, returning
  /// closures, terminals, leak reports — on a microtask instead of
  /// synchronously (web).
  ///
  /// On wasm there is no port and no queue: `frustrate.post` is an import the
  /// Rust producer calls **inside** its own export (runtime/rust/src/post.rs),
  /// so without this every delivery runs arbitrary user Dart on the Rust
  /// stack, while that call's borrows are live. A user `Sink.add` that calls
  /// back into the same Confined handle then gets a *second*
  /// `handle::confined_mut` aliasing the first — demonstrated as a
  /// use-after-free on both web configs (tests/dart_integration/test/
  /// sink_reentrancy_test.dart). Deferral makes the contract identical to
  /// native, whose `Dart_PostCObject` delivery is already a later turn.
  ///
  /// Native keeps everything synchronous: delivery already arrives on a later
  /// turn, and a blocking `DartFunction::call` has a worker parked, waiting.
  final bool _deferDelivery;

  /// Cancel the Rust producer for [id] — supplied by the transport, which
  /// owns the cancel export. Used when a Dart method throws: the channel is
  /// terminated rather than left half-live (see [_dispatch]).
  final void Function(int id)? _cancel;

  /// The zone the transport was installed in, supplied by the transport, which
  /// captures it in its own constructor. Not `Zone.current` read here: routers
  /// are `late final` fields, so that would capture whatever zone happened to
  /// make the first bridge call rather than the zone that installed the bridge.
  ///
  /// Terminals arrive on a bare port callback, where `Zone.current` is the root
  /// zone, so an app's `runZonedGuarded` around init would never see a report
  /// that had nowhere else to go — it would abort the isolate instead. Running
  /// terminal handlers here is what makes that report catchable, and it is the
  /// same reasoning (and the same fix) as the web transport's captured zone for
  /// pool degradation (runtime_web.dart).
  final Zone _zone;

  final Map<int, _Registration> _streams = {};

  /// Value-returning callback registrations: the handler decodes the
  /// argument, runs the user closure, and returns
  /// the encoded result payload. Retired by the end event when the Rust
  /// side drops its handle.
  ///
  /// Handler and label ride in one record — the `_streams` side keeps both in
  /// [_Registration] for the same reason — so a retire cannot drop one and
  /// leave the other.
  /// [onDeclaredError] is the fallible half: the binding hands it the thrown
  /// object and gets
  /// back the encoded payload when that object is the error the signature
  /// **declared**, or null when it is not. Null is what keeps an undeclared
  /// throw loud — the router then answers `STATUS_ERROR` exactly as before, and
  /// the Rust side panics attributably. Absent for an infallible closure, whose
  /// every throw is undeclared by definition.
  final Map<
    int,
    ({
      BinaryWriter Function(BinaryReader) onInvoke,
      BinaryWriter? Function(Object error)? onDeclaredError,
      StreamReclaim? reclaim,
      String? label,
    })
  >
  _functions = {};

  /// Ids cancelled locally whose Rust producer may still have events in
  /// flight, mapped to the reclaim the registration carried. Items for a
  /// tombstoned id are dropped; a late terminal retires the tombstone. (The
  /// producer suppresses everything after it observes the cancel flag, so a
  /// tombstone that never sees a terminal costs one entry for the session.)
  ///
  /// **The reclaim outlives the registration on purpose.** Cancel is exactly
  /// the moment items stop being decoded and start being dropped, so it is
  /// exactly the moment a handle in one would be stranded. The entry — and with
  /// it the reclaim — goes when the tombstone does, on the producer's terminal.
  final Map<int, List<StreamReclaim?>?> _tombstones = {};

  /// Whether this router holds any live registration — an open stream or a
  /// Rust-held callback. The native transport reads this to keep the isolate
  /// alive while a producer might still post: a tombstoned/cancelled id is
  /// already out of `_streams`, so it
  /// correctly does not count. Also the debug lever for asserting channel
  /// cleanliness — the leak's only other symptom is a silent non-exit.
  bool get hasOpenRegistrations => _streams.isNotEmpty || _functions.isNotEmpty;

  /// How many live registrations this router holds (open streams + Rust-held
  /// callbacks). The diagnostic behind `FrustrateRuntime.openChannelCount`:
  /// assert `== 0` to catch a leaked registration, whose only other symptom
  /// is an isolate that silently never exits.
  int get openRegistrationCount => _streams.length + _functions.length;

  /// Where each live registration was opened, for the case the leak terminal
  /// cannot reach: an isolate already pinned by a leak has gone idle, so no
  /// collection runs and no finalizer fires. When a
  /// run hangs or a teardown assertion trips, this is what turns "one channel
  /// is open" into "`TextDoc.watch` is open". Unlabelled entries — a binding
  /// that passed none, or a hand-built registration — read as their id.
  List<String> get openChannelLabels => [
    for (final e in _streams.entries) e.value.label ?? 'stream#${e.key}',
    for (final e in _functions.entries) e.value.label ?? 'callback#${e.key}',
  ];

  StreamRouter(
    this._nextId, {
    void Function(int invocationId, Uint8List response)? respond,
    void Function(int id)? cancel,
    bool deferDelivery = false,
    Zone? zone,
  }) : _respond = respond,
       _cancel = cancel,
       _deferDelivery = deferDelivery,
       _zone = zone ?? Zone.current;

  /// Register a single-method target — a stream sink or a fire-and-forget
  /// callback. Shorthand for [openObject] with just the primary selector.
  int open(
    StreamItemHandler onItem,
    StreamErrorHandler onError,
    StreamDoneHandler onDone, {
    String? label,
    StreamReclaim? reclaim,
  }) => openObject(
    [onItem],
    onError,
    onDone,
    label: label,
    reclaim: reclaim == null ? null : [reclaim],
  );

  /// Register a Dart object under a fresh id, with [methods] indexed by
  /// method selector (0 is the primary method — `add`, or the invocation of
  /// a callback). The object is kept alive by this map entry, and released
  /// when the Rust side drops its handle and the terminal retires it.
  int openObject(
    List<StreamItemHandler> methods,
    StreamErrorHandler onError,
    StreamDoneHandler onDone, {
    String? label,
    List<StreamReclaim?>? reclaim,
  }) {
    final id = _nextId();
    _streams[id] = _Registration(methods, reclaim, onError, onDone, label);
    return id;
  }

  int openFunction(
    BinaryWriter Function(BinaryReader) onInvoke, {
    String? label,
    BinaryWriter? Function(Object error)? onDeclaredError,
    StreamReclaim? reclaim,
  }) {
    final id = _nextId();
    _functions[id] = (
      onInvoke: onInvoke,
      onDeclaredError: onDeclaredError,
      reclaim: reclaim,
      label: label,
    );
    return id;
  }

  /// Dispose every handle an **absorbed** item carries.
  ///
  /// Called where an item is dropped rather than dispatched — a tombstoned or
  /// unknown id. `bytes` is the whole event, so the frame
  /// (`[status][selector]`) is read here and the reclaim sees the payload, in
  /// the same position the item handler would have.
  ///
  /// Guarded: a reclaim is generated code over bytes, and a codec failure in
  /// one must not unwind into the transport's delivery callback, where nothing
  /// could attribute it. Reported instead — the handles past the failure point
  /// are genuinely stranded, and saying so is the only honest outcome.
  void _reclaimAbsorbed(List<StreamReclaim?>? reclaim, Uint8List bytes) {
    if (reclaim == null) return;
    try {
      final r = BinaryReader(bytes)..readU8();
      final selector = r.readU8();
      if (selector >= reclaim.length) return;
      final f = reclaim[selector];
      if (f == null) return;
      f(r);
      // The reclaim walks the whole payload or it has not freed the whole
      // payload; trailing bytes mean handles left behind.
      r.assertConsumed();
    } catch (e, st) {
      _zone.handleUncaughtError(e, st);
    }
  }

  /// Route one delivered envelope. Returns true when [bytes] was a stream
  /// or callback event and has been consumed; false means it is a call
  /// completion for the transport to handle.
  ///
  /// The *classification* — which of those two it is — always happens here,
  /// synchronously: it is the transport's answer, and the transport is
  /// mid-`post` import with nowhere to park. The *routing* (lookup, tombstone
  /// absorption, dispatch, terminals, retirement) is what [_deferDelivery]
  /// moves off the Rust stack.
  bool deliver(int id, Uint8List bytes) {
    if (!_routes(id, bytes[0])) return false;
    if (_deferDelivery) {
      scheduleMicrotask(() => _route(id, bytes));
    } else {
      _route(id, bytes);
    }
    return true;
  }

  /// Whether [id]/[status] is a router event at all — the half of [deliver]
  /// that cannot be deferred.
  ///
  /// Stable across a deferral: the only mutations that can land in the window
  /// are this router's own ([cancelLocal], [fail]), and both move an id from
  /// `_streams`/`_functions` into `_tombstones` — router-owned either way. A
  /// call completion (an unregistered id with a non-stream status) can never
  /// become a router event, because ids are never reused.
  bool _routes(int id, int status) =>
      status == statusCallbackCall ||
      _functions.containsKey(id) ||
      _streams.containsKey(id) ||
      _tombstones.containsKey(id) ||
      // A stream-status event for an unknown id is the legal race of a
      // concurrent add against close/cancel; it is ours to drop. Anything else
      // belongs to the call-completion path. A leak terminal counts as a
      // stream status: it can arrive for an id this router already retired (a
      // cancel that dropped the tombstone, say), and letting it fall through
      // would land in the call-completion path as an unknown call id.
      status == statusStreamItem ||
      status == statusStreamEnd ||
      status == statusLeaked;

  /// The routing half of [deliver]. Runs inline on native and on a microtask
  /// on web ([_deferDelivery]) — so every re-lookup here is deliberate: the
  /// registration this event was classified against may have been cancelled
  /// or failed in the window, exactly as it may have been on native between
  /// `Dart_PostCObject` and the port drain. Nothing is captured across the
  /// gap but the id and the (Dart-owned) bytes.
  void _route(int id, Uint8List bytes) {
    final status = bytes[0];
    if (status == statusCallbackCall) {
      _invoke(id, bytes);
      return;
    }
    final f = _functions[id];
    if (f != null) {
      // The other events a function id carries are its two terminals: the end
      // event retiring the registration when Rust drops its handle, and the
      // leak terminal when Rust dropped it only because the Dart holder was
      // collected undisposed. A callback has no error channel of its own — the
      // Rust side is the caller — so the leak goes to the zone, which is the
      // same treatment a plain `Sink` gets for the same reason (emit_dart.rs).
      assert(
        status == statusStreamEnd || status == statusLeaked,
        'frustrate: unexpected status $status for callback $id',
      );
      _functions.remove(id);
      if (status == statusLeaked) {
        final r = BinaryReader(bytes)..readU8();
        _zone.handleUncaughtError(
          envelopeException(status, r, label: f.label),
          StackTrace.current,
        );
      }
      return;
    }
    final s = _streams[id];
    if (s == null) {
      // Tombstoned, or an event for an id nobody holds — either way ours to
      // absorb (see [_routes], which already decided that).
      //
      // An absorbed *item* may carry handles the producer already minted, so
      // it is walked back before the bytes go. An id with no tombstone at all
      // has no reclaim to run — that is a terminal for a registration this
      // router already retired (the case [_routes] admits), and a terminal
      // carries no item payload.
      if (status == statusStreamItem) {
        _reclaimAbsorbed(_tombstones[id], bytes);
      } else {
        _tombstones.remove(id);
      }
      return;
    }
    final r = BinaryReader(bytes)..readU8();
    switch (status) {
      case statusStreamItem:
        _dispatch(id, s, r);
      case statusStreamEnd:
        _retire(id, s);
        s.onDone();
      default:
        // Terminal error (error/panic/leaked; an ok status here would be a
        // bridge bug and surfaces as the loud unknown-status StateError).
        //
        // Guarded, and in the install zone: a mirror with nowhere to put an
        // error reports it to `Zone.current` (emit_dart.rs), which on a bare
        // port callback is the root zone — an abort rather than something the
        // app can handle. `runBinaryGuarded` also keeps a throwing handler from
        // unwinding into the port callback, where nothing could attribute it.
        _retire(id, s);
        _zone.runBinaryGuarded(
          s.onError,
          envelopeException(status, r, label: s.label),
          StackTrace.current,
        );
    }
  }

  /// Retire a registration a **producer terminal** ended, keeping its reclaim
  /// on the tombstone.
  ///
  /// The terminal does not mean no more items are coming. `close()` on one sink
  /// clone takes the terminal and posts it while an `add` on another clone is
  /// already past its liveness tests and inside the post; the router's own
  /// classification calls a late item for an unknown id "the legal race of a
  /// concurrent add against close". Legal for data — the bytes are dropped —
  /// and for a handle it is the leak this whole mechanism exists to prevent,
  /// because the registration is gone and with it the only thing that knew how
  /// to free the item.
  ///
  /// So a channel that can carry handles leaves its reclaim behind, exactly as
  /// a cancel does. A channel of value types leaves nothing: `reclaim` is null
  /// there and the entry is not made at all.
  void _retire(int id, _Registration s) {
    _streams.remove(id);
    if (s.reclaim != null) _tombstones[id] = s.reclaim;
  }

  /// Run one void method on a registered object: `[status][selector][args]`.
  ///
  /// The method body is user code — a `StreamController.add`, or an
  /// arbitrary Dart class's method. A throw has no reply frame to carry it,
  /// so the policy is: terminate the channel (Rust's next send returns
  /// false, so the producer stops rather than shouting into a broken
  /// target) and report the error, which is loud and debuggable. Swallowing
  /// would be the one unacceptable option.
  ///
  /// Reported to the **install zone**, not `Zone.current`, for exactly the
  /// reason the `_deliverError` comment above already gives: this runs from a
  /// bare `RawReceivePort` callback, and `Zone.current` there is the *root*
  /// zone. Reporting into the root zone makes the throw unhandleable — a
  /// `runZonedGuarded` wrapped around the whole app never sees it and the
  /// isolate dies.
  void _dispatch(int id, _Registration s, BinaryReader r) {
    final selector = r.readU8();
    if (selector >= s.methods.length) {
      throw StateError(
        'frustrate: method selector $selector on channel $id, which has '
        '${s.methods.length} method(s) — generated Dart and Rust disagree '
        'about this type (this is a bridge bug)',
      );
    }
    try {
      s.methods[selector](r);
    } catch (e, st) {
      _cancel?.call(id);
      _zone.handleUncaughtError(e, st);
    }
  }

  /// One DartFunction invocation: run the closure, envelope the outcome
  /// (a throw becomes the error envelope — the Rust side turns it into an
  /// attributable panic), respond.
  ///
  /// No deferral of its own: [deliver] already moved this whole body off the
  /// Rust stack on web ([_deferDelivery]). A second hop here would only make
  /// a returning closure land a microtask later than the sink items it is
  /// ordered against.
  ///
  /// **Accepting an invocation obliges this router to answer it.** Rust is
  /// waiting on the other end — `DartFunction::call` has parked a worker
  /// thread, `call_async` a task — and the runtime's death detection
  /// (runtime/rust/src/post.rs `handle_exit`) is structurally blind to this
  /// case: it learns that an isolate *died*, and an isolate that took a message
  /// and threw is very much alive. Worse, on Flutter a root-zone throw is
  /// swallowed by `PlatformDispatcher.onError`, so the isolate survives and the
  /// waiter never learns anything. So every path out of here that has an
  /// invocation id answers with the error envelope instead of throwing.
  void _invoke(int id, Uint8List bytes) {
    final respond = _respond;
    if (respond == null) {
      // The one exception, and it is unavoidable: with no responder there is
      // nothing to answer *with*. A transport that carries no DartFunction
      // member cannot receive an invocation, so this is a bridge bug.
      throw StateError(
        'frustrate: callback invocation on a channel that cannot respond '
        '(this transport carries no DartFunction member; this is a bridge bug)',
      );
    }
    final f = _functions[id];
    // An invocation for an id this router no longer holds. Reachable when
    // `fail` retired a function registration while Rust still held the handle,
    // and otherwise a bridge bug — either way, reporting it *through the
    // response* is what unparks the caller. Routing it through `_runInvocation`
    // reuses the same invocation-id parse, so the error reaches the right
    // waiter; [deliver]'s deferral already keeps that response off the Rust
    // stack on web.
    final retired = _tombstones[id];
    final BinaryWriter Function(BinaryReader) onInvoke = f == null
        ? (r) {
            // The argument is ours to free: this invocation is answered with an
            // error, so nothing will ever decode it into Dart wrappers. `r` is
            // already positioned past the invocation id, exactly where the
            // closure's own decode would have started.
            (retired == null || retired.isEmpty ? null : retired[0])?.call(r);
            throw StateError(
              'frustrate: invocation for unknown callback $id (its registration '
              'was retired while the Rust handle was still live)',
            );
          }
        : f.onInvoke;
    // A miss has no declared-error hook, and must not: the StateError above is
    // a bridge failure, never the closure's declared refusal.
    _runInvocation(onInvoke, f?.onDeclaredError, bytes, respond);
  }

  /// Decode the argument, run the user closure, envelope the outcome (ok, the
  /// **declared** failure as a value, or the thrown error as prose), and
  /// respond.
  ///
  /// The three-way split is the whole of "a Dart closure whose failure Rust can
  /// handle as a value". The router does not and
  /// cannot know the declared error type — only the generated binding does — so
  /// it asks [onDeclaredError], which answers with the encoded payload or with
  /// null. **Null keeps the undeclared case loud**: an unexpected throw is
  /// still `STATUS_ERROR`, which the Rust side turns into an attributable panic
  /// on the enclosing call, so a Dart bug can never reach Rust dressed as a
  /// business failure.
  void _runInvocation(
    BinaryWriter Function(BinaryReader) onInvoke,
    BinaryWriter? Function(Object error)? onDeclaredError,
    Uint8List bytes,
    void Function(int invocationId, Uint8List response) respond,
  ) {
    // [status][selector][invocation id][argument]. A returning mirror
    // declares one method today, so the selector is always the primary slot;
    // read it rather than assuming, so a mismatch is loud here instead of
    // decoding the invocation id off by a byte.
    //
    // A declared Dart interface (`#[bridge(dart_interface)]`) gives each of
    // its methods its own channel id rather than one id with N selectors, so
    // a non-zero selector here is an invariant violation and nothing
    // generates one.
    final r = BinaryReader(bytes)..readU8();
    final selector = r.readU8();
    if (selector != 0) {
      throw StateError(
        'frustrate: returning-method selector $selector is not implemented '
        '(this is a bridge bug)',
      );
    }
    final invocationId = r.readHandle();
    int status;
    BinaryWriter body;
    try {
      body = onInvoke(r);
      status = 0; // ok
    } catch (e) {
      // Encoding the declared error is generated code and can itself throw (a
      // codec is allowed to be loud — the char codec rejects a lone surrogate,
      // say). Answering is still mandatory, so fall back to the prose path and
      // name BOTH failures: the refusal the author meant, and why it could not
      // be delivered as one.
      BinaryWriter? declared;
      try {
        declared = onDeclaredError?.call(e);
      } catch (encodeFailure) {
        return _respondWith(
          1,
          BinaryWriter()..writeString(
            'frustrate: the Dart closure threw its declared error ($e), '
            'but encoding it failed: $encodeFailure',
          ),
          invocationId,
          respond,
        );
      }
      if (declared != null) {
        body = declared;
        status = statusTypedError;
      } else {
        body = BinaryWriter()..writeString('$e');
        status = 1; // error
      }
    }
    return _respondWith(status, body, invocationId, respond);
  }

  /// Frame `[status][payload]` and hand it to the transport. Split out so the
  /// three outcomes above share one framing.
  void _respondWith(
    int status,
    BinaryWriter body,
    int invocationId,
    void Function(int invocationId, Uint8List response) respond,
  ) {
    final payload = body.takeBytes();
    final resp = Uint8List(payload.length + 1);
    resp[0] = status;
    resp.setRange(1, resp.length, payload);
    respond(invocationId, resp);
  }

  /// Local half of a cancel: delivery stops now, unconditionally. The
  /// transport separately signals the Rust flag.
  void cancelLocal(int id) {
    final r = _streams.remove(id);
    if (r != null) _tombstones[id] = r.reclaim;
  }

  /// Route an opening-call failure to the stream, if it is still open, and
  /// tombstone the id so a producer's late terminal is absorbed — exactly as
  /// [cancelLocal] does. `fail` and `cancelLocal` are the two ways a stream
  /// leaves the table while the Rust sink still lives, so both must leave a
  /// tombstone or the sink's late `error()`/drop falls through to the
  /// call-completion path (an unknown-id crash in debug, a silent drop in
  /// release). Tombstone before reporting, so a throwing consumer `onError`
  /// cannot skip it. (If the sink already delivered a terminal — e.g. it was
  /// dropped before the body failed — the removal is a no-op and nothing is
  /// tombstoned.)
  void fail(int id, Object error, StackTrace st) {
    // A function registration has no error handler of its own — Rust is the
    // caller — so failing one is purely a retire. It still tombstones: the Rust
    // handle may outlive the failed call and post its end event later.
    final fn = _functions.remove(id);
    if (fn != null) {
      // One selector on an invocation, so the tombstone carries the single
      // argument reclaim in slot 0.
      _tombstones[id] = fn.reclaim == null ? null : [fn.reclaim];
      return;
    }
    final r = _streams.remove(id);
    if (r == null) return;
    _tombstones[id] = r.reclaim;
    _zone.runBinaryGuarded(r.onError, error, st);
  }
}
