/// Native transport: dynamic library loading, sync calls over FFI, async
/// completion delivery through this isolate's `RawReceivePort`.
///
/// Completions arrive as `[callId, payload]` messages posted by Rust worker
/// threads with `Dart_PostCObject`. A raw port rather than a
/// `NativeCallable.listener` — the same transport underneath, chosen for what it
/// does when this isolate is already gone; the reasoning lives in
/// runtime/rust/src/post.rs.
library;

import 'dart:async';
import 'dart:ffi';
import 'dart:io' show Platform;
import 'dart:isolate';
import 'dart:typed_data';

import 'package:ffi/ffi.dart';

import 'binary_codec.dart';
import 'envelope.dart';
import 'pending_calls.dart';
import 'runtime_core.dart';
import 'stream_router.dart';

typedef _InitDlC = Int64 Function(Pointer<Void>, Int64);
typedef _InitDlDart = int Function(Pointer<Void>, int);
typedef _RuntimeAbiC = Uint64 Function();
typedef _RuntimeAbiDart = int Function();
// `Handle` carries a real Dart object across FFI — here the `SendPort` Rust
// minted with `Dart_NewSendPort` for the port it owns. It is the only way to
// get one: a native port id is an `int`, and Dart cannot build a `SendPort`
// from an int.
typedef _ExitPortC = Handle Function();
typedef _ExitPortDart = Object Function();
// (fn_id, request, request length, response buffer, its capacity) -> the
// response length, or -1 if it did not fit and was leased instead (the
// (ptr, len, cap) triple then sits at the front of the response buffer).
// See frustrate::envelope::respond_out.
typedef _CallSyncC = Int32 Function(
  Uint32,
  Pointer<Uint8>,
  Uint64,
  Pointer<Uint8>,
  Uint64,
);
typedef _CallSyncDart = int Function(
  int,
  Pointer<Uint8>,
  int,
  Pointer<Uint8>,
  int,
);
typedef _CallAsyncC = Void Function(Uint32, Pointer<Uint8>, Uint64, Uint64);
typedef _CallAsyncDart = void Function(int, Pointer<Uint8>, int, int);
typedef _CallCancelC = Uint8 Function(Uint64);
typedef _CallCancelDart = int Function(int);
typedef _BufferFreeC = Void Function(Pointer<Uint8>, Uint64, Uint64);
typedef _BufferFreeDart = void Function(Pointer<Uint8>, int, int);
typedef _DropC = Void Function(Pointer<Void>);
typedef _DropDart = void Function(Pointer<Void>);
typedef _ResidentAttachC = Void Function(Uint64, Uint32);
typedef _ResidentAttachDart = void Function(int, int);
typedef _ResidentReclaimableC = Uint8 Function(Uint64);
typedef _ResidentReclaimableDart = int Function(int);
typedef _ResidentLeakedC = Uint64 Function();
typedef _ResidentLeakedDart = int Function();
typedef _ActorSpawnC = Uint64 Function();
typedef _ActorSpawnDart = int Function();
typedef _ActorCallC = Void Function(
  Uint64,
  Uint64,
  Uint32,
  Pointer<Uint8>,
  Uint64,
);
typedef _ActorCallDart = void Function(int, int, int, Pointer<Uint8>, int);
typedef _ActorShutdownC = Void Function(Uint64);
typedef _ActorShutdownDart = void Function(int);
typedef _ActorCancelDeferredC = Uint8 Function(Uint64, Uint64);
typedef _ActorCancelDeferredDart = int Function(int, int);
typedef _StreamCancelC = Void Function(Uint64);
typedef _StreamCancelDart = void Function(int);
typedef _CallbackRespondC = Void Function(Uint64, Pointer<Uint8>, Uint64);
typedef _CallbackRespondDart = void Function(int, Pointer<Uint8>, int);
typedef _SchemaHashC = Uint64 Function();
typedef _SchemaHashDart = int Function();
typedef _ManualDrainC = Uint8 Function();
typedef _ManualDrainDart = int Function();

/// Native entry point: initialize once, then use the generated API.
///
/// Both entry points follow the repeated-init contract stated on [Frustrate]:
/// naming the same library again does nothing, naming a different one throws.
/// The no-op is idempotent in the *strong* sense — a second call does not even
/// open the library — because `Frustrate.install` alone is not enough: its
/// argument is evaluated before it decides to discard it, and a discarded
/// `NativeRuntime` has already re-registered this isolate's exit listener
/// (see [Frustrate.initializedFrom]).
///
/// The two entry points share the one transport slot, so they share the guard:
/// an `init` followed by an `initWithLibrary` is a disagreement like any other.
class FrustrateNative {
  /// The transport this class installed, and how to name it in an error.
  static NativeRuntime? _runtime;
  static String? _description;

  /// Open the bridge dynamic library at [libraryPath] and register the
  /// completion callback.
  ///
  /// A repeated call with the same [libraryPath] is a no-op; one with a
  /// different path throws a [StateError] naming both, because the first
  /// library stays installed and would keep serving every call.
  ///
  /// **Paths are compared as written.** Two spellings of one file
  /// (`libbridge.dylib` and `./libbridge.dylib`) are reported as a
  /// disagreement rather than resolved to the same library: resolving would
  /// mean opening the second library to find out what it is, and a guard whose
  /// job is to *avoid* loading a second bridge must not load one to decide.
  /// Naming the bridge one way is the fix, and the message quotes both.
  static void init(String libraryPath) {
    final description = 'the bridge library at "$libraryPath"';
    if (Frustrate.initializedFrom(libraryPath, description)) return;
    _install(
      NativeRuntime(DynamicLibrary.open(libraryPath)),
      libraryPath,
      description,
    );
  }

  /// Initialize from an already-loaded library (e.g. DynamicLibrary.process()
  /// when the bridge is statically linked into the embedder).
  ///
  /// Repeated calls follow the same contract as [init]. Here the comparison is
  /// exact rather than textual: `DynamicLibrary` equality is defined as "loads
  /// the same library" (dart:ffi), so handing this the same library through two
  /// separate `DynamicLibrary.open` calls is correctly a no-op.
  static void initWithLibrary(DynamicLibrary library) {
    const description = 'an already-loaded library (initWithLibrary)';
    if (Frustrate.initializedFrom(library, description)) return;
    _install(NativeRuntime(library), library, description);
  }

  static void _install(
    NativeRuntime runtime,
    Object source,
    String description,
  ) {
    Frustrate.install(runtime, source: source, description: description);
    _runtime ??= runtime;
    _description ??= description;
  }

  /// The drain of the installed bridge's manual scheduler, for a test harness
  /// that decides when Rust tasks run.
  ///
  /// Only a bridge built with the manual scheduler has one
  /// (`frustrate_manual_scheduler_library` in
  /// `@frustrate//bazel:manual_scheduler.bzl`). In that build an `async fn`
  /// body or a `frustrate::runtime::spawn` task runs only when the returned
  /// function is called, so the harness chooses how Rust work interleaves with
  /// its own events and a seeded run replays exactly. Sync members, and async
  /// calls to a plain `fn`, still run as usual.
  ///
  /// Each call polls until the run queue is empty and returns whether anything
  /// ran. `false` means quiet, not finished: a task waiting for an answer from
  /// the host is off the queue too. Never call it from inside a Rust task it
  /// is polling.
  ///
  /// Throws a [StateError] when no bridge was installed through this class, or
  /// when the installed one was built without the manual scheduler: a harness
  /// that asked for manual scheduling and got the pool would replay nothing.
  static bool Function() manualDrain() {
    final runtime = _runtime;
    if (runtime == null) {
      throw StateError(
        'frustrate: manualDrain() needs an installed bridge; '
        'call FrustrateNative.init first.',
      );
    }
    final _ManualDrainDart drain;
    try {
      drain = runtime.lib.lookupFunction<_ManualDrainC, _ManualDrainDart>(
        'frustrate_manual_drain',
      );
    } on ArgumentError {
      throw StateError(
        'frustrate: $_description was built without the '
        'manual scheduler, so its Rust tasks run on the thread pool and '
        'there is nothing to drain. Build it through '
        'frustrate_manual_scheduler_library '
        '(@frustrate//bazel:manual_scheduler.bzl).',
      );
    }
    return () => drain() != 0;
  }
}

final class NativeRuntime implements FrustrateRuntime {
  final DynamicLibrary lib;

  /// Where request blocks come from. `malloc` in every shipping configuration;
  /// injectable so a test can count allocations and frees.
  ///
  /// That is not a hook looking for a use. The request block is allocated
  /// before the generated encoder runs and freed after the call returns, with a
  /// user-reachable throw possible in between — so "exactly one allocation and
  /// exactly one free per call, including when the encoder throws" is a real
  /// contract with no other way to observe it. `request_block_lifetime_test`
  /// is what observes it.
  final Allocator _alloc;

  late final _CallSyncDart _callSync = lib
      .lookupFunction<_CallSyncC, _CallSyncDart>('frustrate_call_sync');
  late final _CallAsyncDart _callAsync = lib
      .lookupFunction<_CallAsyncC, _CallAsyncDart>('frustrate_call_async');
  late final _BufferFreeDart _bufferFree = lib
      .lookupFunction<_BufferFreeC, _BufferFreeDart>('frustrate_buffer_free');

  /// `frustrate_call_cancel` (executor.rs) — claims one in-flight `async fn`
  /// call out of the cooperative executor's registry so its future is dropped.
  /// A runtime export since ABI 4, so every loaded bridge has it and the lookup
  /// cannot fail on a module this transport already accepted.
  late final _CallCancelDart _callCancel = lib
      .lookupFunction<_CallCancelC, _CallCancelDart>('frustrate_call_cancel');

  // Per-isolate call-id sequence. The full id an isolate puts on the wire is
  // `_isolateTag | _nextSeq++` (see [_allocId]): the tag makes ids globally
  // unique across isolates so Rust's post::respond can route completions back
  // to the isolate that owns them. Without it, two isolates both start their
  // sequence at 1 and their completions are indistinguishable — silent
  // cross-isolate misdelivery. Kept in sync with post.rs's ISOLATE_SHIFT.
  static const int _isolateShift = 48;
  int _nextSeq = 1;
  late final int _isolateTag;

  /// The same id, unshifted. Rust routes completions by the tag inside a
  /// call id; the resident registry is told the plain id, because a resident
  /// is minted on a sync call and there is no call id to carry it.
  late final int _isolateId;

  /// Every async call on this transport, including every actor call: a native
  /// host dispatches through `_issueAsync`, so there is one registry to count.
  final InFlightCalls _inFlight = InFlightCalls();

  late final PendingCalls _pending = PendingCalls(
    _allocId,
    onChanged: _updateKeepAlive,
    cancelCall: (id) => _callCancel(id) != 0,
    tally: _inFlight,
  );

  /// The zone `FrustrateNative.init` was called in — an ordinary field, so it
  /// is captured when this runtime is constructed rather than whenever a `late`
  /// field first runs. Terminal reports that have no handler to go to are
  /// delivered here (stream_router.dart); on a bare port callback `Zone.current`
  /// is the root zone, where an unhandled report aborts the isolate instead of
  /// reaching an app's `runZonedGuarded`. Web's equivalent is `_zone` in
  /// runtime_web.dart, for the same reason.
  final Zone _zone = Zone.current;

  late final StreamRouter _router = StreamRouter(
    _allocId,
    respond: _respondToCallback,
    cancel: cancelStream,
    zone: _zone,
  );

  /// Where every async completion lands: call responses, stream events, and
  /// callback invocations, on ONE port.
  ///
  /// One port is load-bearing, not tidiness. Message order is FIFO per port but
  /// carries no guarantee *across* ports, and the routing design depends on
  /// cross-kind ordering — a stream's items arriving before its terminal and
  /// before the opening call's completion. A second port would be a second
  /// ordering domain.
  late final RawReceivePort _completions;

  NativeRuntime(this.lib, {Allocator? allocator})
    : _alloc = allocator ?? malloc {
    _completions = RawReceivePort(_onMessage, 'frustrate');
    // Keep the isolate alive exactly while there is bridge work it must still
    // service: an in-flight call, OR an open stream / stored callback whose
    // Rust producer may still post. An idle port does NOT pin (or the process
    // would never exit); `_updateKeepAlive` recomputes this from `_pending` and
    // the router's open registrations at every change. This is the same
    // contract an open `ReceivePort` has — and it applies to the ROOT isolate
    // too: a `main()` that returns with an open registration stays up until it
    // closes, exactly like a leaked port.
    //
    // Liveness only: it decides when the isolate may exit, not whether a
    // producer is safe to post. Nothing here is load-bearing for safety — an
    // exit this fails to prevent costs a refused post, not a crash.
    _completions.keepIsolateAlive = false;
    _checkRuntimeAbi();
    // Hand Rust the dart_api_dl table and this isolate's port. The returned id
    // is stamped into every id we allocate so completions route back here and
    // not to another isolate; a negative return is a failed handshake.
    final id = lib.lookupFunction<_InitDlC, _InitDlDart>('frustrate_init_dl')(
      NativeApi.initializeApiDLData,
      _completions.sendPort.nativePort,
    );
    if (id < 0) {
      _completions.close();
      throw StateError(_handshakeFailure(id));
    }
    try {
      _registerExitNotice(id);
    } catch (_) {
      // Symmetric with the failed-handshake path above: this runtime is not
      // going to be returned, so nothing else will ever close the port.
      _completions.close();
      rethrow;
    }
    _isolateTag = id << _isolateShift;
    _isolateId = id;
  }

  /// Tell Rust when this isolate dies, so it can settle whatever this isolate
  /// still owes.
  ///
  /// The one death mode nothing else catches: a callback invocation this
  /// isolate **accepted** and then died holding. Rust discovers every other
  /// death by having a post refused, but a producer parked waiting for this
  /// isolate's answer is by definition not posting — so the isolate has to say
  /// so itself. `response` is the isolate id, which is how the sweep on the
  /// other side knows whose invocations to fail.
  ///
  /// **Synchronous, in the constructor, right after the handshake.** Not for
  /// atomicity — `Isolate.immediate` is taken at loop back-edges and can land
  /// mid-statement, so "has an id but no listener" is a reachable state. It is
  /// harmless because an isolate that dies there has allocated no ids and so
  /// owes nothing. What synchrony buys is ordering: this registration is an OOB
  /// message to our own control port, so any later kill queues behind it and
  /// finds the listener installed.
  void _registerExitNotice(int id) {
    final port = lib.lookupFunction<_ExitPortC, _ExitPortDart>(
      'frustrate_exit_port',
    )();
    Isolate.current.addOnExitListener(port as SendPort, response: id);
  }

  /// Compare against the loaded dylib before calling anything else, so a
  /// runtime/package skew is a named error rather than a mismatched call.
  /// [checkSchemaHash] is the generated-bindings equivalent; this guards the
  /// layer underneath it. [frustrateRuntimeAbi] is the one number both
  /// transports compare against — the web half is in runtime_web.dart.
  void _checkRuntimeAbi() {
    final int abi;
    try {
      abi = lib.lookupFunction<_RuntimeAbiC, _RuntimeAbiDart>(
        'frustrate_runtime_abi',
      )();
    } on ArgumentError {
      throw StateError(
        'frustrate: the bridge library predates the runtime ABI guard — it '
        'was built against a frustrate runtime older than the one this '
        'package (ABI $frustrateRuntimeAbi) speaks. Rebuild the bridge crate.',
      );
    }
    if (abi != frustrateRuntimeAbi) {
      throw StateError(
        'frustrate: runtime ABI mismatch — the bridge library speaks ABI '
        '$abi, this package speaks $frustrateRuntimeAbi. Rebuild the bridge '
        'crate and regenerate its bindings.',
      );
    }
  }

  /// Explain a negative `frustrate_init_dl`. The codes are defined in
  /// runtime/rust/src/post.rs.
  String _handshakeFailure(int code) => switch (code) {
    -1 =>
      'frustrate: Dart handed the runtime a null dart_api_dl table '
          '(NativeApi.initializeApiDLData); this is a bridge bug',
    -2 =>
      'frustrate: this Dart SDK exposes an unsupported DART_API_DL '
          'major version — the runtime was written against major 2. Update '
          'frustrate, or pin an SDK whose dart_api_dl major version is 2.',
    -3 =>
      'frustrate: Dart_PostCObject is missing from this SDK\'s '
          'dart_api_dl table, so async completions cannot be delivered',
    -4 =>
      'frustrate: Dart_NewNativePort is missing from this SDK\'s '
          'dart_api_dl table, so an isolate cannot report its own death and '
          'a callback awaiting a dead isolate would never settle',
    -5 =>
      'frustrate: Dart_NewSendPort is missing from this SDK\'s '
          'dart_api_dl table, so this isolate cannot be handed the port it '
          'must report its own death to',
    -6 =>
      'frustrate: Dart_NewNativePort refused to create the '
          'isolate-exit port, so an isolate cannot report its own death',
    _ => 'frustrate: frustrate_init_dl failed with code $code',
  };

  /// Allocate a globally-unique id for a call, stream, or callback: this
  /// isolate's tag in the high bits, a per-isolate sequence in the low bits.
  int _allocId() => _isolateTag | (_nextSeq++);

  /// Recompute keep-alive from live work: an in-flight call, or an open stream
  /// / stored-callback registration whose Rust producer may still post. Called
  /// after every mutation of `_pending` or the router's registrations. Runs
  /// only on the isolate thread (request encoding, `_onPost`, user cancel), so
  /// it needs no synchronization; recomputing from state (rather than
  /// incrementing a counter) is robust against a handler that opens or closes
  /// registrations re-entrantly.
  void _updateKeepAlive() {
    _completions.keepIsolateAlive =
        !_pending.isEmpty || _router.hasOpenRegistrations;
  }

  /// One `[callId, payload]` completion message.
  ///
  /// `Dart_PostCObject` copied the payload into the message, so — unlike the
  /// sync path, which still leases a buffer — there is nothing to free and no
  /// second copy to make.
  void _onMessage(Object? message) {
    final m = message as List<Object?>;
    final callId = m[0]! as int;
    final bytes = m[1]! as Uint8List;
    // Stream events route by id. A terminal (end/error) retires the
    // registration *inside* `deliver()`, so recompute keep-alive before the
    // early return — once no call is pending and no registration is open, the
    // isolate is free to exit. Open registrations pin; in-flight calls pin;
    // nothing else does.
    if (_router.deliver(callId, bytes)) {
      _updateKeepAlive();
      return;
    }
    // NB: the call must be outside the assert — asserts are stripped in
    // release, and this one settles the future.
    final settled = _pending.complete(callId, bytes);
    // A completion for a call we no longer know about would indicate a
    // bridge bug (call ids are never reused within an isolate).
    assert(settled, 'frustrate: completion for unknown call id $callId');
  }

  @override
  BinaryReader callSync(
    int fnId,
    int sizeHint,
    void Function(BinaryWriter w) encode, {
    Object Function(BinaryReader)? typedError,
  }) {
    // ONE allocation carries the response slab AND the request: the slab at
    // [block], the request bytes immediately after it. This method used to
    // make four (`reqPtr`, and one cell each for the three out-params) and
    // free four; the shape it shares with the web transport makes it one and
    // one, and a response that fits the slab needs no `_bufferFree` either.
    //
    // The ABI is shared, so the slab is here because it is the ABI — but it
    // is not a native concession: fewer allocator round trips is fewer
    // allocator round trips. Nothing about the lifetimes changed. The block
    // belongs to one call, Rust writes it only after the body has returned,
    // and the `finally` that frees it is the frame that allocated it.
    //
    // `malloc`, not `calloc`: every byte read back out was written by Rust
    // (the response) or by us (the request), so zeroing it first is work
    // nothing observes.
    //
    // The generated encoder writes its request DIRECTLY into this block — the
    // reason it arrives as a closure rather than as a finished writer. There
    // used to be a Dart heap buffer here, filled by the encoder and then
    // memcpy'd in; on a 1 MiB argument that was a full payload copy and a
    // 1 MiB Dart allocation, per call, on the calling isolate's own thread
    // where nothing overlaps it.
    // The writer's own 64-byte floor, applied here too: a hint is a lower
    // bound, and undershooting it by a few bytes on a small call would trade a
    // 64-byte over-allocation for a whole realloc-and-copy.
    var block = _alloc.allocate<Uint8>(
      frustrateRespSlabBytes + (sizeHint < 64 ? 64 : sizeHint),
    );
    final w = BinaryWriter.external(
      _requestView(block, sizeHint < 64 ? 64 : sizeHint),
      // Growth: allocate the bigger block, carry the written prefix across,
      // free the old one, and republish the pointer this frame will free. Only
      // the WRITTEN prefix is copied — the slab holds nothing yet (Rust writes
      // it after the body returns) and the slack past `written` is by
      // definition unread.
      (newCap, written) {
        final next = _alloc.allocate<Uint8>(frustrateRespSlabBytes + newCap);
        if (written > 0) {
          _requestView(next, written).setAll(0, _requestView(block, written));
        }
        _alloc.free(block);
        block = next;
        return _requestView(next, newCap);
      },
    );
    try {
      encode(w);
      final reqLen = w.length;
      final n = _callSync(
        fnId,
        Pointer<Uint8>.fromAddress(block.address + frustrateRespSlabBytes),
        reqLen,
        block,
        frustrateRespSlabBytes,
      );
      if (n >= 0) {
        // The common case: the envelope is sitting in our own block. Copy it
        // out before the `finally` frees the block underneath it.
        return decodeEnvelope(
          Uint8List.fromList(block.asTypedList(n)),
          typedError: typedError,
        );
      }
      // Overflow: the lease triple is at the front of the slab.
      final lease = block.cast<Uint64>();
      final p = Pointer<Uint8>.fromAddress(lease[0]);
      final len = lease[1];
      final resp = Uint8List.fromList(p.asTypedList(len));
      _bufferFree(p, len, lease[2]);
      return decodeEnvelope(resp, typedError: typedError);
    } finally {
      // Close BEFORE the free, so a writer stashed by a codec hook reports a
      // named StateError rather than writing into memory that is already gone.
      w.close();
      // `block`, not the one allocated above: a grow republished it, and the
      // frame frees whichever block is current.
      _alloc.free(block);
    }
  }

  /// A writer-shaped view of [len] bytes of the request region of [block] —
  /// i.e. immediately after the response slab.
  ///
  /// `Pointer.asTypedList` returns a list at offset 0 of its own buffer, which
  /// is exactly what [BinaryWriter.external] requires (and asserts): every
  /// write indexes by length alone, with no slab bias to remember.
  static Uint8List _requestView(Pointer<Uint8> block, int len) =>
      Pointer<Uint8>.fromAddress(block.address + frustrateRespSlabBytes)
          .asTypedList(len);

  @override
  Future<BinaryReader> callAsync(
    int fnId,
    int sizeHint,
    void Function(BinaryWriter w) encode, {
    Object Function(BinaryReader)? typedError,
    FrustrateCancelToken? cancel,
  }) => _issueAsync(
    sizeHint,
    encode,
    (callId, reqPtr, len) => _callAsync(fnId, reqPtr, len, callId),
    typedError: typedError,
    cancel: cancel,
  );

  /// Shared async issue path: registers the completer, pins the listener,
  /// leases the request buffer for exactly the duration of [issue] (Rust
  /// consumes the request before the call returns), and completes via the
  /// post callback.
  ///
  /// Everything that can fail runs inside [PendingCalls.issue] — including the
  /// allocation, the **encode**, and the lazy `lookupFunction` that [issue]
  /// triggers on its first call — so a failure retires the registration and
  /// rejects the future rather than leaking a completer and latching
  /// `keepIsolateAlive` on.
  ///
  /// Encoding inside `issue` is what keeps `callAsync`'s "never throws
  /// synchronously" promise across the closure protocol: a throwing encoder is
  /// now just another issue failure, handled by the same path that already
  /// handled a failed allocation.
  Future<BinaryReader> _issueAsync(
    int sizeHint,
    void Function(BinaryWriter w) encode,
    void Function(int callId, Pointer<Uint8> req, int len) issue, {
    Object Function(BinaryReader)? typedError,
    void Function()? preflight,
    FrustrateCancelToken? cancel,
  }) {
    return _pending.issue(typedError: typedError, cancel: cancel, (callId) {
      // Anything that can refuse the call outright runs BEFORE the block is
      // allocated and the encoder runs, so a refused call does no encoding —
      // the order the web host already used. Still inside the guarded region,
      // so a refusal rejects the future rather than escaping a
      // Future-returning method.
      preflight?.call();
      // No response slab on this path: Rust answers through the post callback,
      // so the block is the request and nothing else. Otherwise identical to
      // `callSync` — the encoder writes straight into it, and growth is a
      // realloc the frame republishes.
      //
      // `malloc`, not `calloc`, for the reason `callSync` already states:
      // zeroing bytes the encoder is about to write is work nothing observes.
      // Rust reads exactly the length we pass and the codec is length-prefixed
      // throughout, so no reader can reach the slack past it.
      final cap = sizeHint < 64 ? 64 : sizeHint;
      var reqPtr = _alloc.allocate<Uint8>(cap);
      final w = BinaryWriter.external(reqPtr.asTypedList(cap), (
        newCap,
        written,
      ) {
        final next = _alloc.allocate<Uint8>(newCap);
        if (written > 0) {
          next.asTypedList(written).setAll(0, reqPtr.asTypedList(written));
        }
        _alloc.free(reqPtr);
        reqPtr = next;
        return next.asTypedList(newCap);
      });
      try {
        encode(w);
        // An empty request goes as a null pointer, as it always has: the
        // generated `request_slice` short-circuits on a zero length and never
        // dereferences it.
        final len = w.length;
        issue(callId, len == 0 ? nullptr.cast<Uint8>() : reqPtr, len);
      } finally {
        w.close();
        _alloc.free(reqPtr);
      }
    });
  }

  @override
  int openObject(
    List<StreamItemHandler> methods,
    StreamErrorHandler onError,
    StreamDoneHandler onDone, {
    String? label,
    List<StreamReclaim?>? reclaim,
  }) {
    // Pin before the producer can post: the registration exists during request
    // encoding, before Rust ever decodes the id.
    final id = _router.openObject(
      methods,
      onError,
      onDone,
      label: label,
      reclaim: reclaim,
    );
    _updateKeepAlive();
    return id;
  }

  @override
  int openStream(
    StreamItemHandler onItem,
    StreamErrorHandler onError,
    StreamDoneHandler onDone, {
    String? label,
    List<StreamReclaim?>? reclaim,
  }) {
    final id = _router.open(
      onItem,
      onError,
      onDone,
      label: label,
      reclaim: reclaim == null || reclaim.isEmpty ? null : reclaim[0],
    );
    _updateKeepAlive();
    return id;
  }

  // Lazy like the actor exports: a bridge crate with no stream members has
  // no generated reference to frustrate::stream, so the export may be
  // absent — and never needed. pause/resume share the cancel signature
  // (`void(u64)`).
  late final _StreamCancelDart _streamCancel = lib
      .lookupFunction<_StreamCancelC, _StreamCancelDart>(
        'frustrate_stream_cancel',
      );
  late final _StreamCancelDart _streamPause = lib
      .lookupFunction<_StreamCancelC, _StreamCancelDart>(
        'frustrate_stream_pause',
      );
  late final _StreamCancelDart _streamResume = lib
      .lookupFunction<_StreamCancelC, _StreamCancelDart>(
        'frustrate_stream_resume',
      );

  @override
  void cancelStream(int id) {
    _router.cancelLocal(id);
    // Retiring the local registration may leave the isolate with no live work
    // (a cancel from a spawned isolate that has already returned from `main`);
    // recompute so it can exit. NB: the *producer* may still post once more
    // before it observes the flag — the narrow "posts racing isolate death"
    // window closed separately by a post.rs dead-isolate net.
    _updateKeepAlive();
    // The registry is process-global shared memory: the flag is visible to
    // whatever thread runs the producer (pool, actor executor, or a later
    // call holding a stored sink) immediately.
    _streamCancel(id);
  }

  // Backpressure signals the shared registry too; unlike cancel they retire no
  // registration, so keep-alive is untouched (the stream stays open — an open
  // registration already pins the isolate).
  @override
  void pauseStream(int id) => _streamPause(id);

  @override
  void resumeStream(int id) => _streamResume(id);

  @override
  void failStream(int id, Object error, StackTrace st) {
    // `fail()` removes the registration, so keep-alive must recompute — else an
    // opening-call error that retires the sink before the Rust sink is even
    // constructed would pin the isolate forever.
    _router.fail(id, error, st);
    _updateKeepAlive();
  }

  @override
  int openFunction(
    BinaryWriter Function(BinaryReader) onInvoke, {
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
    _updateKeepAlive();
    return id;
  }

  // Lazy like the stream/actor exports: present exactly when the bridge
  // crate has a member that needs it.
  late final _CallbackRespondDart _callbackRespond = lib
      .lookupFunction<_CallbackRespondC, _CallbackRespondDart>(
        'frustrate_callback_respond',
      );

  /// Wake the Rust worker blocked in DartFunction::call. Rust copies the
  /// buffer during the call, so the lease ends when it returns.
  ///
  /// `malloc`, not `calloc`, for the reason `callSync` already states: `setAll`
  /// below writes every byte, so zeroing first is work nothing observes.
  void _respondToCallback(int invocationId, Uint8List response) {
    final ptr = malloc<Uint8>(response.length);
    try {
      ptr.asTypedList(response.length).setAll(0, response);
      _callbackRespond(invocationId, ptr, response.length);
    } finally {
      malloc.free(ptr);
    }
  }

  // Always present (every generated glue emits it), so looked up eagerly. The
  // u64 return comes back signed-reinterpreted in the Dart int; normalize to
  // an unsigned 64-bit BigInt to compare with the compiled-in fingerprint.
  late final _SchemaHashDart _schemaHash = lib
      .lookupFunction<_SchemaHashC, _SchemaHashDart>('frustrate_schema_hash');

  @override
  void checkSchemaHash(BigInt expected) {
    final native = BigInt.from(_schemaHash()).toUnsigned(64);
    final want = expected.toUnsigned(64);
    if (native != want) {
      throw StateError(
        'frustrate: generated Dart bindings are stale — regenerate '
        '(schema 0x${want.toRadixString(16)} != '
        'native 0x${native.toRadixString(16)})',
      );
    }
  }

  /// One [HandleDrop] per drop symbol, for the life of the transport.
  ///
  /// Memoized because the hooks have to *stay alive*, not merely to save a
  /// `dlsym`. Generated classes read this at every mint (so a handle's drop is
  /// bound to the transport that issued its raw, rather than to whichever
  /// transport happened to be active when the class was first touched), and a
  /// `NativeFinalizer` that is itself collected finalizes nothing — the same
  /// reason `_actorReaper` below is a field.
  final Map<String, HandleDrop> _drops = {};

  @override
  HandleDrop handleDrop(String symbol) =>
      _drops[symbol] ??= _lookupHandleDrop(symbol);

  /// Memoized separately from [_drops], and for the same reason that map
  /// exists: the hook owns the [Finalizer] that backs the GC path and has to
  /// outlive every handle it was attached to.
  final Map<String, HandleDrop> _residentDrops = {};

  @override
  HandleDrop residentHandleDrop(String symbol) =>
      _residentDrops[symbol] ??= ResidentHandleDrop(
        lib.lookupFunction<_DropC, _DropDart>(
          symbol.replaceFirst('frustrate_drop_', 'frustrate_finalize_'),
        ),
        lib.lookupFunction<_DropC, _DropDart>(symbol),
        _residentAttach,
        _residentReclaimable,
        _isolateId,
      );

  /// Looked up lazily: a bridge crate with no resident types never calls them.
  late final _ResidentAttachDart _residentAttach = lib
      .lookupFunction<_ResidentAttachC, _ResidentAttachDart>(
        'frustrate_resident_attach',
      );
  late final _ResidentReclaimableDart _residentReclaimable = lib
      .lookupFunction<_ResidentReclaimableC, _ResidentReclaimableDart>(
        'frustrate_resident_reclaimable',
      );

  HandleDrop _lookupHandleDrop(String symbol) {
    // The GC finalizer uses the drop export's `frustrate_finalize_*` sibling.
    // Both retire whatever channels the object holds; they differ in what the
    // terminal says. Eager dispose() closes them normally, while the finalize
    // variant ends them as *leaked* and names the type — reaching it proves
    // dispose() was never called, because dispose() detaches this finalizer
    // before dropping (`stream::finalize_scope`).
    final finalizeSymbol = symbol.replaceFirst(
      'frustrate_drop_',
      'frustrate_finalize_',
    );
    return NativeHandleDrop(
      NativeFinalizer(lib.lookup<NativeFunction<_DropC>>(finalizeSymbol)),
      lib.lookupFunction<_DropC, _DropDart>(symbol),
    );
  }

  // Actor exports are looked up lazily: a bridge crate with no actor types
  // doesn't have them, and never needs them.
  late final _ActorSpawnDart _actorSpawn = lib
      .lookupFunction<_ActorSpawnC, _ActorSpawnDart>('frustrate_actor_spawn');
  late final _ActorCallDart _actorCall = lib
      .lookupFunction<_ActorCallC, _ActorCallDart>('frustrate_actor_call');
  late final _ActorShutdownDart _actorShutdown = lib
      .lookupFunction<_ActorShutdownC, _ActorShutdownDart>(
        'frustrate_actor_shutdown',
      );
  late final _ActorCancelDeferredDart _actorCancelDeferred = lib
      .lookupFunction<_ActorCancelDeferredC, _ActorCancelDeferredDart>(
        'frustrate_actor_cancel_deferred',
      );

  /// The GC backstop shared by every actor on this runtime.
  ///
  /// One finalizer, not one per host: `frustrate_actor_reap` is a single
  /// runtime export taking the host id as its token, so unlike
  /// [handleDrop]'s per-type `frustrate_finalize_<Type>` there is nothing to
  /// vary. A field so it stays reachable for the life of the runtime — a
  /// `NativeFinalizer` that is itself collected finalizes nothing.
  late final NativeFinalizer _actorReaper = NativeFinalizer(
    lib.lookup<NativeFunction<_DropC>>('frustrate_actor_reap'),
  );

  @override
  Future<ActorHost> spawnActorHost({String? debugName}) async =>
      _NativeActorHost(this, _actorSpawn(), debugName);

  @override
  bool get asyncIsParallel => true;

  @override
  int get hardwareParallelism => Platform.numberOfProcessors;

  @override
  int get openChannelCount => _router.openRegistrationCount;

  /// Looked up on demand rather than as a `late final`: a bridge crate with no
  /// resident types does not export this, and reading the count is not worth
  /// making such a bridge fail to load. An absent export is an honest 0 —
  /// nothing can have been lost.
  @override
  int get residentLeakCount {
    try {
      return lib.lookupFunction<_ResidentLeakedC, _ResidentLeakedDart>(
        'frustrate_resident_leaked',
      )();
    } on ArgumentError {
      return 0;
    }
  }

  @override
  List<String> get openChannelLabels => _router.openChannelLabels;

  /// A platform transport *is* its bridge: its raws, its `frustrate_drop_*`
  /// exports and its router are all its own.
  @override
  Object get bridgeIdentity => this;

  @override
  int get inFlightCallCount => _inFlight.count;
}

/// A dedicated Rust thread (see runtime/rust/src/actor.rs). Completions
/// arrive through the runtime's single post listener; only the issue path
/// differs from a pool call.
final class _NativeActorHost implements ActorHost {
  final NativeRuntime _rt;
  final int _hostId;

  /// The actor's type name (generated constructors pass it), so
  /// host-attributed errors — the disposed-deferred StateError above all —
  /// name the actor rather than an anonymous host.
  final String? _debugName;
  bool _stopped = false;

  /// Deferred calls in flight on this host (`ActorHost.call(deferred:)`).
  /// Added at issue, removed when the future settles; what remains at
  /// [shutdown] is exactly the set the dispose contract cancels.
  final Set<int> _deferredInFlight = {};

  _NativeActorHost(this._rt, this._hostId, this._debugName);

  @override
  Object get bridgeIdentity => _rt.bridgeIdentity;

  /// Refusing here rather than letting the call reach Rust is what makes the
  /// two platforms agree: web's worker is already gone after [shutdown], so it
  /// has to answer locally, and Rust's own dead-host reply is a panic envelope
  /// (actor.rs) — a different exception for the same caller mistake. Both
  /// platforms now reject with the same StateError, and Rust's envelope stays
  /// as the backstop for a host id that was never ours.
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
    // `cancel:` needs no per-host plumbing here, and that is a fact about
    // native rather than a shortcut. A deferred completion is spawned on the
    // *process-global* cooperative executor (`deferred.dart`'s Rust side:
    // `spawn_deferred` → `executor::spawn`), the same registry a plain
    // `async fn` lands in — so the runtime's own `frustrate_call_cancel` is
    // already the right claim, under the same answer-once gate.
    // `frustrate_actor_cancel_deferred` adds only the host-registry
    // bookkeeping `shutdown()` needs to enumerate what is left, and that entry
    // is retired by the future's own drop guard a drain later; the sweep is
    // documented as idempotent against exactly this (actor.rs).
    final future = _rt._issueAsync(
      sizeHint,
      encode,
      typedError: typedError,
      cancel: cancel,
      preflight: () {
        if (_stopped) {
          throw StateError('frustrate: actor host was shut down');
        }
      },
      (callId, reqPtr, len) {
        issued = callId;
        _rt._actorCall(_hostId, callId, fnId, reqPtr, len);
      },
    );
    // The send closure ran synchronously inside issue, so `issued` is set
    // unless the call was refused before it got an id (then there is nothing
    // to track — the future is already failed).
    final id = issued;
    if (deferred && id != null) {
      _deferredInFlight.add(id);
      // .ignore(), not unawaited(): whenComplete's derived future re-raises
      // the call's own error, which the caller handles on the ORIGINAL
      // future — the derived one must swallow it or every failed deferred
      // call double-reports as an unhandled async error.
      future.whenComplete(() => _deferredInFlight.remove(id)).ignore();
    }
    return future;
  }

  // Natively there is one channel: actor completions and stream events all
  // arrive through the runtime's post listener, and the cancel flag lives
  // in process-global memory — so stream plumbing delegates whole.
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
  void cancelStream(int id) => _rt.cancelStream(id);

  @override
  void pauseStream(int id) => _rt.pauseStream(id);

  @override
  void resumeStream(int id) => _rt.resumeStream(id);

  @override
  void failStream(int id, Object error, StackTrace st) =>
      _rt.failStream(id, error, st);

  @override
  int openFunction(
    BinaryWriter Function(BinaryReader) onInvoke, {
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
  Future<void> shutdown() async {
    _stopped = true;
    // Each still-outstanding deferred call is *cancelled*, not drained —
    // draining would hang dispose() for as long as the slow work takes, which
    // is the wedge Deferred exists to remove.
    //
    // The cancel FFI claims the call from the executor's registry under the
    // same lock its completion path uses (executor.rs, the answer-once
    // gate), so the two outcomes are exact: 1 = claimed, that completion
    // will never be posted, we own failing the future here; 0 = the call
    // already answered (its response is delivered or in the port queue) and
    // must be left to complete normally.
    for (final id in _deferredInFlight.toList()) {
      if (_rt._actorCancelDeferred(_hostId, id) != 0) {
        _rt._pending.fail(
          id,
          disposedDeferredError(_debugName),
          StackTrace.current,
        );
      }
    }
    _rt._actorShutdown(_hostId);
  }

  /// The token is the host id, which is all the reap path needs — the object
  /// pointer is already held by the executor's own teardown slot
  /// (`handle::actor_new`). Host ids start at 1, so the token is never null.
  @override
  void attachReaper(covariant Finalizable owner) => _rt._actorReaper.attach(
    owner,
    Pointer<Void>.fromAddress(_hostId),
    detach: owner,
  );

  @override
  void detachReaper(Object owner) => _rt._actorReaper.detach(owner);
}

/// The GC path for a `#[bridge(resident)]` handle: a `dart:core` [Finalizer],
/// never a `NativeFinalizer`.
///
/// That is the whole difference, and it is the difference the model is built
/// on. A `NativeFinalizer`'s callback runs on an arbitrary VM thread; this
/// one's runs on the isolate that attached it, which for a resident is the
/// isolate — and, in a pinned isolate, the thread — that built the object. A
/// resident has no `Send` bound, so no other thread may run its `Drop`.
///
/// The weaker promise that comes with it is the price and is worth stating:
/// `dart:core`'s [Finalizer] guarantees only that a callback *may* run, where
/// a `NativeFinalizer` runs at the latest when the isolate group shuts down.
/// So a resident that is neither disposed nor collected is not reclaimed —
/// which is the same outcome an exiting isolate produces, and is reported the
/// same way (`frustrate::resident`).
///
/// [attach] is also where Rust learns which isolate owns the handle. It cannot
/// work that out: a resident is minted on a sync call, and `frustrate_call_sync`
/// carries no call id and so no isolate id.
final class ResidentHandleDrop implements HandleDrop {
  final _DropDart _finalizeFn;
  final _DropDart _dropFn;
  final _ResidentAttachDart _attachFn;
  final _ResidentReclaimableDart _reclaimableFn;
  final int _isolateId;

  /// The callback is [_finalize] rather than [drop] so the two keep the
  /// meanings they have for every other model: an eager `dispose()` closes the
  /// object's channels normally, and reaching the GC path at all proves
  /// dispose() was never called, so it ends them as *leaked* and names the
  /// type (`frustrate::stream::finalize_scope`).
  late final Finalizer<int> _finalizer = Finalizer(_finalize);

  ResidentHandleDrop(
    this._finalizeFn,
    this._dropFn,
    this._attachFn,
    this._reclaimableFn,
    this._isolateId,
  );

  void _finalize(int raw) => _finalizeFn(Pointer<Void>.fromAddress(raw));

  @override
  void attach(Object owner, int raw) {
    _attachFn(raw, _isolateId);
    _finalizer.attach(owner, raw, detach: owner);
  }

  @override
  void detach(Object owner) => _finalizer.detach(owner);

  /// **A `dispose()` this thread cannot serve throws rather than passing
  /// quietly.** Only the thread that built a resident object may run its
  /// `Drop`, so a `dispose()` from anywhere else cannot free it — the object
  /// is stranded, and the runtime records it in the same report an exiting
  /// isolate writes. That has to reach the caller: the alternative is a
  /// `dispose()` that returns normally having freed nothing, which is the
  /// silent failure this bridge does not ship.
  ///
  /// The drop export runs first regardless, because the accounting is its job
  /// and the object is stranded either way. The handle has already read as
  /// disposed since before this was called (`OpaqueHandleBase.dispose` clears
  /// its raw first), which is correct: it names an object nothing can reach.
  @override
  void drop(int raw) {
    final ok = _reclaimableFn(raw) != 0;
    _dropFn(Pointer<Void>.fromAddress(raw));
    if (!ok) {
      throw StateError(
        'frustrate: this resident object was disposed from a thread other '
        'than the one that built it, so it could not be freed — only that '
        'thread may run its Drop. It is leaked, and named in the runtime\'s '
        'resident leak report. A Dart isolate is not pinned to an OS thread: '
        'it can run each event-loop turn on a different one, which an '
        '`Isolate.spawn`ed isolate routinely does. Use a resident type from '
        'one isolate that does not migrate, or declare it confined (which '
        'requires Send) or actor (which owns its own thread).',
      );
    }
  }
}

final class NativeHandleDrop implements HandleDrop {
  final NativeFinalizer _finalizer;
  final _DropDart _dropFn;

  NativeHandleDrop(this._finalizer, this._dropFn);

  @override
  void attach(covariant Finalizable owner, int raw) =>
      _finalizer.attach(owner, Pointer<Void>.fromAddress(raw), detach: owner);

  @override
  void detach(Object owner) => _finalizer.detach(owner);

  @override
  void drop(int raw) => _dropFn(Pointer<Void>.fromAddress(raw));
}
