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
import 'dart:convert';
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
// See frustrate::__private::envelope::respond_out.
// The last argument is the isolate's own owner id (its tag): what the body
// spawns belongs to the zone frustrate was initialised in (uncaught.rs).
typedef _CallSyncC = Int32 Function(
  Uint32,
  Pointer<Uint8>,
  Uint64,
  Pointer<Uint8>,
  Uint64,
  Uint64,
);
typedef _CallSyncDart = int Function(
  int,
  Pointer<Uint8>,
  int,
  Pointer<Uint8>,
  int,
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
typedef _ActorSpawnC = Pointer<Void> Function(Uint64);
typedef _ActorSpawnDart = Pointer<Void> Function(int);
typedef _ActorCallC = Void Function(
  Pointer<Void>,
  Uint64,
  Uint32,
  Pointer<Uint8>,
  Uint64,
);
typedef _ActorCallDart = void Function(
  Pointer<Void>,
  int,
  int,
  Pointer<Uint8>,
  int,
);
typedef _ActorShutdownC = Void Function(Pointer<Void>);
typedef _ActorShutdownDart = void Function(Pointer<Void>);
typedef _ActorRetainC = Pointer<Void> Function(Pointer<Void>);
typedef _ActorRetainDart = Pointer<Void> Function(Pointer<Void>);
typedef _StreamCancelC = Void Function(Uint64);
typedef _StreamCancelDart = void Function(int);
typedef _CallbackRespondC = Void Function(Uint64, Pointer<Uint8>, Uint64);
typedef _CallbackRespondDart = void Function(int, Pointer<Uint8>, int);
typedef _SchemaHashC = Uint64 Function();
typedef _SchemaHashDart = int Function();
typedef _ManualDrainC = Uint8 Function();
typedef _ManualDrainDart = int Function();
typedef _ManualCountsC = Uint64 Function(Pointer<Uint8>, Uint64);
typedef _ManualCountsDart = int Function(Pointer<Uint8>, int);
typedef _IdleNotifyC = Void Function(Int64);
typedef _IdleNotifyDart = void Function(int);
typedef _IdleReadC = Uint64 Function();
typedef _IdleReadDart = int Function();

/// The Rust runtime's own exports, by name: one each per library, however
/// many bridges it links. Generated bridge libraries name these in their
/// `@Native` declarations, so each symbol is spelled once, here.
abstract final class RuntimeSymbol {
  static const callCancel = 'frustrate_call_cancel';
  static const bufferFree = 'frustrate_buffer_free';
  static const initDl = 'frustrate_init_dl';
  static const exitPort = 'frustrate_exit_port';
  static const runtimeAbi = 'frustrate_runtime_abi';
  static const callbackRespond = 'frustrate_callback_respond';
  static const streamCancel = 'frustrate_stream_cancel';
  static const streamPause = 'frustrate_stream_pause';
  static const streamResume = 'frustrate_stream_resume';
  static const residentAttach = 'frustrate_resident_attach';
  static const residentReclaimable = 'frustrate_resident_reclaimable';
  static const residentLeaked = 'frustrate_resident_leaked';
  static const actorSpawn = 'frustrate_actor_spawn';
  static const actorShutdown = 'frustrate_actor_shutdown';
  static const actorRetain = 'frustrate_actor_retain';
  static const actorRelease = 'frustrate_actor_release';
  static const actorReap = 'frustrate_actor_reap';
  static const manualDrain = 'frustrate_manual_drain';
  static const manualUnscheduled = 'frustrate_manual_unscheduled';
  static const manualHandles = 'frustrate_manual_handles';
  static const idleNotify = 'frustrate_idle_notify';
  static const idlePending = 'frustrate_idle_pending';
  static const idleGeneration = 'frustrate_idle_generation';
}

/// The fixed exports the native runtime resolves through a [BridgeLibrary]:
/// the Rust runtime's own ([RuntimeSymbol]) and the entries every bridge's
/// glue defines under its crate name ([FrustrateBridge.symbol]).
///
/// A generated bridge library answers [BridgeLibrary.address] with an
/// exhaustive `switch` over these, so an export added here and not to codegen
/// fails to compile in the bindings rather than at first use.
enum NativeExport {
  callCancel(RuntimeSymbol.callCancel),
  bufferFree(RuntimeSymbol.bufferFree),
  initDl(RuntimeSymbol.initDl),
  exitPort(RuntimeSymbol.exitPort),
  runtimeAbi(RuntimeSymbol.runtimeAbi),
  callbackRespond(RuntimeSymbol.callbackRespond),
  streamCancel(RuntimeSymbol.streamCancel),
  streamPause(RuntimeSymbol.streamPause),
  streamResume(RuntimeSymbol.streamResume),
  residentAttach(RuntimeSymbol.residentAttach),
  residentReclaimable(RuntimeSymbol.residentReclaimable),
  residentLeaked(RuntimeSymbol.residentLeaked),
  actorSpawn(RuntimeSymbol.actorSpawn),
  actorShutdown(RuntimeSymbol.actorShutdown),
  actorRetain(RuntimeSymbol.actorRetain),
  actorRelease(RuntimeSymbol.actorRelease),
  actorReap(RuntimeSymbol.actorReap),
  manualDrain(RuntimeSymbol.manualDrain),
  manualUnscheduled(RuntimeSymbol.manualUnscheduled),
  manualHandles(RuntimeSymbol.manualHandles),
  idleNotify(RuntimeSymbol.idleNotify),
  idlePending(RuntimeSymbol.idlePending),
  idleGeneration(RuntimeSymbol.idleGeneration),
  callSync.bridge('call_sync'),
  callAsync.bridge('call_async'),
  actorCall.bridge('actor_call'),
  schemaHash.bridge('schema_hash');

  const NativeExport(String this._runtimeSymbol) : _entry = null;
  const NativeExport.bridge(String this._entry) : _runtimeSymbol = null;

  final String? _runtimeSymbol;
  final String? _entry;

  /// The name [bridge]'s library exports this under.
  String symbolFor(FrustrateBridge bridge) =>
      _runtimeSymbol ?? bridge.symbol(_entry!);
}

/// A handle class's two exports: the drop `dispose()` calls, and the finalize
/// the GC path calls, which ends the object's channels as leaked.
typedef HandleExports = ({Pointer<Void> drop, Pointer<Void> finalize});

/// The native library that serves a bridge's exports.
///
/// Generated bindings declare one `const` (`<crate>Library`) that resolves the
/// bridge as a Dart code asset, so the build decides which library file backs
/// it; pass it to [FrustrateNative.init]. [BridgeLibrary.dynamic] adapts an
/// already-open [DynamicLibrary] for a library loaded by hand.
abstract class BridgeLibrary {
  /// Names the library in errors: the code-asset id for a generated bridge.
  String get name;

  /// The bridge whose exports this resolves: the generated bindings'
  /// `<crate>Bridge`.
  FrustrateBridge get bridge;

  /// The address of [export].
  ///
  /// Throws an [ArgumentError] when the library has no such export. The
  /// runtime relies on that for the exports a bridge may lack (the manual
  /// scheduler, actors, streams, resident types).
  Pointer<Void> address(NativeExport export);

  /// The exports of the handle class whose drop export is [dropSymbol]: the
  /// name generated code passes to `FrustrateRuntime.handleDrop`.
  HandleExports handle(String dropSymbol);

  /// [library]'s exports for [bridge], named [name] in errors.
  ///
  /// Two of these are equal when their libraries and bridges are:
  /// `DynamicLibrary` equality is "loads the same library" (dart:ffi), so
  /// naming one library through two separate `DynamicLibrary.open` calls is
  /// one library.
  factory BridgeLibrary.dynamic(
    DynamicLibrary library, {
    required String name,
    required FrustrateBridge bridge,
  }) = _DynamicBridgeLibrary;
}

final class _DynamicBridgeLibrary implements BridgeLibrary {
  final DynamicLibrary _library;

  @override
  final String name;

  @override
  final FrustrateBridge bridge;

  _DynamicBridgeLibrary(
    this._library, {
    required this.name,
    required this.bridge,
  });

  @override
  Pointer<Void> address(NativeExport export) =>
      _library.lookup<Void>(export.symbolFor(bridge));

  /// A library opened by name has only names to go on, so the finalize
  /// export is found by the glue's naming scheme: `drop_<Type>` beside
  /// `finalize_<Type>`.
  @override
  HandleExports handle(String dropSymbol) {
    final prefix = bridge.symbol('drop_');
    if (!dropSymbol.startsWith(prefix)) {
      throw ArgumentError.value(dropSymbol, 'dropSymbol', 'not $bridge\'s');
    }
    final type = dropSymbol.substring(prefix.length);
    return (
      drop: _library.lookup<Void>(dropSymbol),
      finalize: _library.lookup<Void>(bridge.symbol('finalize_$type')),
    );
  }

  @override
  bool operator ==(Object other) =>
      other is _DynamicBridgeLibrary &&
      other._library == _library &&
      other.bridge == bridge;

  @override
  int get hashCode => Object.hash(_library, bridge);
}

/// Native entry point: initialize each bridge once, then use the generated
/// API.
///
/// Repeated calls follow the contract stated on [Frustrate]: naming the same
/// library again does nothing, naming another bridge in the same library
/// attaches it to the running transport, and naming a different library
/// throws. The no-op is idempotent in the *strong* sense — a second call does
/// not even resolve the library — because `Frustrate.install` alone is not
/// enough: its argument is evaluated before it decides to discard it, and a
/// discarded transport has already re-registered this isolate's exit listener
/// (see [Frustrate.initializedFrom]).
class FrustrateNative {
  /// The native transport [Frustrate.install] recorded, if one is installed.
  static _NativeTransport? get _transport {
    final installed = Frustrate.installedTransport;
    return installed is _NativeTransport ? installed : null;
  }

  static String _describe(BridgeLibrary library) =>
      'the bridge library "${library.name}"';

  /// Serve [library]'s bridge from this isolate's Rust runtime, building the
  /// transport on the first call.
  ///
  /// A later call naming another bridge attaches it when its library is the
  /// one already loaded — several bridges composed into one library with
  /// `frustrate_shared_library` — and throws a [StateError] naming the fix when it
  /// is a different library, whose Rust runtime would be a second one. The
  /// same [library] again is a no-op; generated libraries are `const`, so the
  /// same one is always the same object.
  static void init(BridgeLibrary library) {
    final description = _describe(library);
    final transport = _transport;
    if (transport != null) {
      transport.attach(library, description);
      Frustrate.install(
        transport.runtimeFor(library),
        source: library,
        description: description,
      );
      return;
    }
    if (Frustrate.initializedFrom(
      library,
      description,
      bridge: library.bridge,
    )) {
      return;
    }
    final built = _NativeTransport(library);
    Frustrate.install(
      built.runtimeFor(library),
      source: library,
      description: description,
    );
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
  /// The returned function throws a [StateError], polling nothing more, once
  /// a handle nobody disposed put work on the queue: it was dropped by its
  /// finalizer when the garbage collector chose, or by its actor's executor
  /// as that stopped, and its `Drop` spawned or woke a task. That timing is
  /// not the run's, so the run would not replay. The error names the handle
  /// types; dispose those handles. The next drain runs the work as usual.
  ///
  /// Throws a [StateError] when no bridge was installed through this class, or
  /// when the installed one was built without the manual scheduler: a harness
  /// that asked for manual scheduling and got the pool would replay nothing.
  static bool Function() manualDrain() {
    final transport = _manualTransport('manualDrain()');
    final drain = _manualExport<_ManualDrainC, _ManualDrainDart>(
      transport,
      NativeExport.manualDrain,
      (f) => f.asFunction(),
    );
    final unscheduled = _manualExport<_ManualCountsC, _ManualCountsDart>(
      transport,
      NativeExport.manualUnscheduled,
      (f) => f.asFunction(),
    );
    return () {
      final ran = drain();
      if (ran == 2) throw StateError(_unscheduledReport(unscheduled));
      return ran != 0;
    };
  }

  /// A reader of the handles made and disposed, for a harness that holds a
  /// run to disposing everything it makes. Each call returns, by each handle
  /// type's name (the Dart class's, as [manualDrain]'s error names it), how
  /// many handles Rust gave Dart since the
  /// previous call and how many Dart gave back, by `dispose()` or by a call
  /// that took the object, and starts the count again; types with neither are
  /// left out. A handle a finalizer dropped is not given back, so the counts
  /// depend on what the run did and not on when the collector ran: `made`
  /// above `disposed` is handles the run did not dispose.
  ///
  /// Kept only by a bridge built with the manual scheduler, as for
  /// [manualDrain], and it throws a [StateError] the same way when there is
  /// none.
  static Map<String, ({int made, int disposed})> Function() manualHandles() {
    final handles = _manualExport<_ManualCountsC, _ManualCountsDart>(
      _manualTransport('manualHandles()'),
      NativeExport.manualHandles,
      (f) => f.asFunction(),
    );
    return () => {
      for (final MapEntry(:key, value: [made, disposed]) in _takeRecords(
        handles,
      ).entries)
        key: (made: made, disposed: disposed),
    };
  }

  static _NativeTransport _manualTransport(String caller) {
    final transport = _transport;
    if (transport == null) {
      throw StateError(
        'frustrate: $caller needs an installed bridge; '
        'call FrustrateNative.init first.',
      );
    }
    return transport;
  }

  static D _manualExport<C extends Function, D extends Function>(
    _NativeTransport transport,
    NativeExport export,
    D Function(Pointer<NativeFunction<C>>) bind,
  ) {
    try {
      return bind(transport._primary.address(export).cast<NativeFunction<C>>());
    } on ArgumentError {
      throw StateError(
        'frustrate: ${_describe(transport._primary)} was built without the '
        'manual scheduler: its Rust tasks run on the thread pool, and it '
        'keeps no manual-scheduler records. Build it through '
        'frustrate_manual_scheduler_library '
        '(@frustrate//bazel:manual_scheduler.bzl).',
      );
    }
  }

  /// The [StateError] message for work finalizers scheduled, read (and so
  /// cleared) from the library's record.
  static String _unscheduledReport(_ManualCountsDart unscheduled) {
    final lines = _takeRecords(unscheduled).entries
        .map((e) => '  ${e.key}: ${e.value.single}');
    return 'frustrate: handles that were never disposed scheduled Rust '
        'work as the garbage collector or a stopping actor dropped them, '
        'so when it runs is not up to this run, which will not replay. '
        'Spawns and wakes, by handle type:\n${lines.join('\n')}\n'
        'Dispose these handles. The next drain runs the work.';
  }

  /// Reads (and so clears) a record of counts by handle type.
  static Map<String, List<int>> _takeRecords(_ManualCountsDart read) {
    var cap = 256;
    while (true) {
      final buf = malloc<Uint8>(cap);
      try {
        final len = read(buf, cap);
        if (len > cap) {
          cap = len;
          continue;
        }
        return {
          for (final line in utf8.decode(buf.asTypedList(len)).split('\n'))
            if (line.isNotEmpty)
              if (line.split('\t') case [final type, ...final counts])
                type: [for (final n in counts) int.parse(n)],
        };
      } finally {
        malloc.free(buf);
      }
    }
  }

  /// The installed bridge's idle barrier, for a test harness that waits for
  /// no Rust work to be queued or running.
  ///
  /// Only a bridge built with the idle barrier has one
  /// (`frustrate_idle_barrier_library` in `@frustrate//bazel:idle_barrier.bzl`).
  ///
  /// Throws a [StateError] when no bridge was installed through this class, or
  /// when the installed one was built without the barrier.
  static IdleBarrier idleBarrier() {
    final transport = _transport;
    if (transport == null) {
      throw StateError(
        'frustrate: idleBarrier() needs an installed bridge; '
        'call FrustrateNative.init first.',
      );
    }
    final library = transport._primary;
    try {
      return IdleBarrier._(
        library
            .address(NativeExport.idleNotify)
            .cast<NativeFunction<_IdleNotifyC>>()
            .asFunction<_IdleNotifyDart>(),
        library
            .address(NativeExport.idlePending)
            .cast<NativeFunction<_IdleReadC>>()
            .asFunction<_IdleReadDart>(),
        library
            .address(NativeExport.idleGeneration)
            .cast<NativeFunction<_IdleReadC>>()
            .asFunction<_IdleReadDart>(),
      );
    } on ArgumentError {
      throw StateError(
        'frustrate: ${_describe(library)} was built without the idle '
        'barrier, so '
        'it counts no work. Build it through frustrate_idle_barrier_library '
        '(@frustrate//bazel:idle_barrier.bzl).',
      );
    }
  }
}

/// How much Rust work the runtime has queued or running, and when none is.
///
/// The work counted is every job on the runtime's thread pool (an async call,
/// a poll of an `async fn`) and every call sent to an actor, from when it is
/// queued until it is answered. A task suspended on something outside Rust (a
/// Dart timer, an answer from Dart) is not counted until it is woken, and a
/// thread the bridge's own code starts is never counted.
///
/// Process-wide: work for every isolate is in the one count.
final class IdleBarrier {
  IdleBarrier._(this._notify, this._pending, this._generation);

  final _IdleNotifyDart _notify;
  final _IdleReadDart _pending;
  final _IdleReadDart _generation;

  /// Completes with the [generation] at a moment when no work was queued or
  /// running.
  ///
  /// Every answer Rust posted before that moment has been delivered, and its
  /// handler run, before this completes. Work may have started since: read
  /// [pending] and [generation] in the same turn to learn whether it has. A
  /// zero [pending] and an unchanged [generation] mean no job has started
  /// since the moment this reports.
  Future<int> whenIdle() {
    final completer = Completer<int>();
    late final RawReceivePort port;
    port = RawReceivePort((Object? generation) {
      port.close();
      completer.complete(generation! as int);
    }, 'frustrate.idle');
    _notify(port.sendPort.nativePort);
    return completer.future;
  }

  /// How many jobs are queued or running now.
  int get pending => _pending();

  /// How many jobs have ever been queued.
  int get generation => _generation();
}

/// The one native transport of an isolate: the completion port and the
/// call-id sequence, shared by every bridge linked into its library. Each
/// bridge reaches it through its own [NativeRuntime], which keeps that
/// bridge's pending calls and stream router.
final class _NativeTransport {
  /// The library the transport was built from. Every library it accepts
  /// later is the same image ([attach]), so the runtime's own exports resolve
  /// through this one.
  final BridgeLibrary _primary;

  /// Every library this transport has accepted, and the runtime each one's
  /// bridge is served by.
  final Map<BridgeLibrary, NativeRuntime> _runtimes = {};

  /// Where `frustrate_runtime_abi` resolves in [_primary]: how [attach] tells
  /// the same image from another one.
  late final int _image = _primary.address(NativeExport.runtimeAbi).address;

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

  late final _BufferFreeDart _bufferFree = _primary
      .address(NativeExport.bufferFree)
      .cast<NativeFunction<_BufferFreeC>>()
      .asFunction<_BufferFreeDart>();

  /// `frustrate_call_cancel` (executor.rs) — claims one in-flight `async fn`
  /// call out of the cooperative executor's registry so its future is dropped.
  /// A runtime export since ABI 4, so every loaded bridge has it and the lookup
  /// cannot fail on a module this transport already accepted.
  late final _CallCancelDart _callCancel = _primary
      .address(NativeExport.callCancel)
      .cast<NativeFunction<_CallCancelC>>()
      .asFunction<_CallCancelDart>();

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

  /// The zone `FrustrateNative.init` was called in — an ordinary field, so it
  /// is captured when this runtime is constructed rather than whenever a `late`
  /// field first runs. Terminal reports that have no handler to go to are
  /// delivered here (stream_router.dart); on a bare port callback `Zone.current`
  /// is the root zone, where an unhandled report aborts the isolate instead of
  /// reaching an app's `runZonedGuarded`. Web's equivalent is `_zone` in
  /// runtime_web.dart, for the same reason.
  final Zone _zone = Zone.current;

  /// The zone each live actor was built in, by the id its Rust executor
  /// reports a panic that answers no call under (`frustrate_actor_spawn`).
  /// An entry goes when its host object is collected; a report that finds
  /// none, or names the isolate itself (sequence zero), goes to [_zone].
  final Map<int, Zone> _actorZones = {};

  late final Finalizer<int> _actorZoneRelease = Finalizer(_actorZones.remove);

  /// Where every async completion lands: call responses, stream events, and
  /// callback invocations, on ONE port.
  ///
  /// One port is load-bearing, not tidiness. Message order is FIFO per port but
  /// carries no guarantee *across* ports, and the routing design depends on
  /// cross-kind ordering — a stream's items arriving before its terminal and
  /// before the opening call's completion. A second port would be a second
  /// ordering domain.
  late final RawReceivePort _completions;

  _NativeTransport(this._primary, {Allocator? allocator})
    : _alloc = allocator ?? malloc {
    _completions = RawReceivePort(_onMessage, 'frustrate');
    // Keep the isolate alive exactly while there is bridge work it must still
    // service: an in-flight call, OR an open stream / stored callback whose
    // Rust producer may still post. An idle port does NOT pin (or the process
    // would never exit); `_updateKeepAlive` recomputes this from each bridge's
    // pending calls and
    // the router's open registrations at every change. This is the same
    // contract an open `ReceivePort` has — and it applies to the ROOT isolate
    // too: a `main()` that returns with an open registration stays up until it
    // closes, exactly like a leaked port.
    //
    // Liveness only: it decides when the isolate may exit, not whether a
    // producer is safe to post. Nothing here is load-bearing for safety — an
    // exit this fails to prevent costs a refused post, not a crash.
    _completions.keepIsolateAlive = false;
    final int id;
    try {
      _checkRuntimeAbi();
      // Hand Rust the dart_api_dl table and this isolate's port. The returned
      // id is stamped into every id we allocate so completions route back here
      // and not to another isolate; a negative return is a failed handshake.
      id =
          _primary
              .address(NativeExport.initDl)
              .cast<NativeFunction<_InitDlC>>()
              .asFunction<_InitDlDart>()(
            NativeApi.initializeApiDLData,
            _completions.sendPort.nativePort,
          );
    } catch (_) {
      // No runtime is going to be returned (the library did not resolve, or
      // speaks another ABI), so nothing else will ever close the port.
      _completions.close();
      rethrow;
    }
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
    final port = _primary
        .address(NativeExport.exitPort)
        .cast<NativeFunction<_ExitPortC>>()
        .asFunction<_ExitPortDart>()();
    Isolate.current.addOnExitListener(port as SendPort, response: id);
  }

  /// Compare against the loaded library before calling anything else, so a
  /// runtime/package skew is a named error rather than a mismatched call.
  /// [checkSchemaHash] is the generated-bindings equivalent; this guards the
  /// layer underneath it. [frustrateRuntimeAbi] is the one number both
  /// transports compare against — the web half is in runtime_web.dart.
  void _checkRuntimeAbi() {
    final int abi;
    try {
      abi = _primary
          .address(NativeExport.runtimeAbi)
          .cast<NativeFunction<_RuntimeAbiC>>()
          .asFunction<_RuntimeAbiDart>()();
    } on ArgumentError catch (e) {
      throw StateError(
        'frustrate: the bridge library ${_primary.name} has no runtime ABI '
        'export. Either it predates the runtime ABI guard — it was built '
        'against a frustrate runtime older than the one this package (ABI '
        '$frustrateRuntimeAbi) speaks, so rebuild the bridge crate — or the '
        'library did not resolve at all, which the lookup reports as: $e',
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

  /// Accept [library] into this transport: a no-op for one already accepted,
  /// and a [StateError] for one that is not the image this transport runs.
  ///
  /// The same image is decided by where the runtime's own export resolves.
  /// Two tables naming one loaded library answer the same address; two
  /// copies of a library, even byte-identical ones, are two images with two
  /// Rust runtimes, and serving a bridge from the second would hand its raws
  /// to the first.
  void attach(BridgeLibrary library, String description) {
    if (_runtimes.containsKey(library)) return;
    final int theirs;
    try {
      theirs = library.address(NativeExport.runtimeAbi).address;
    } on ArgumentError catch (e) {
      throw StateError(
        'frustrate: $description has no runtime ABI export, or did not '
        'resolve at all: $e',
      );
    }
    if (theirs != _image) {
      throw StateError(
        'frustrate: $description is a different library from '
        '"${_primary.name}", which this isolate already runs. An isolate runs '
        'one Rust runtime, so every bridge it uses must be linked into one '
        'library: compose them with frustrate_shared_library '
        '(@frustrate//bazel:defs.bzl) and bind each bridge\'s code asset to '
        'the combined library.',
      );
    }
    runtimeFor(library);
  }

  /// The runtime [library]'s bridge is served by. Only for a library this
  /// transport was built from or has [attach]ed.
  NativeRuntime runtimeFor(BridgeLibrary library) =>
      _runtimes[library] ??= NativeRuntime._(this, library).._join();

  /// isolate's tag in the high bits, a per-isolate sequence in the low bits.
  int _allocId() => _isolateTag | (_nextSeq++);

  /// Recompute keep-alive from live work: an in-flight call, or an open stream
  /// / stored-callback registration whose Rust producer may still post. Called
  /// after every mutation of a bridge's pending calls or router registrations. Runs
  /// only on the isolate thread (request encoding, `_onPost`, user cancel), so
  /// it needs no synchronization; recomputing from state (rather than
  /// incrementing a counter) is robust against a handler that opens or closes
  /// registrations re-entrantly.
  void _updateKeepAlive() {
    var live = false;
    for (final view in _views) {
      if (!view._pending.isEmpty || view._router.hasOpenRegistrations) {
        live = true;
        break;
      }
    }
    _completions.keepIsolateAlive = live;
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
    //
    // Ids come from one sequence however many bridges share the port, so
    // whichever bridge's router or pending calls hold an id is the one it is
    // for.
    if (_routerFor(_views, callId).deliver(callId, bytes)) {
      _updateKeepAlive();
      return;
    }
    final views = _views;
    for (var i = 0; i < views.length; i++) {
      final pending = views[i]._pending;
      if (pending.owns(callId)) {
        pending.complete(callId, bytes);
        return;
      }
    }
    // No call owns it: a panic in work no call waits on (an actor's posted
    // closure, a detached task), reported under its owner's id. An uncaught
    // error, in the zone the work belongs to.
    if (bytes.isNotEmpty && bytes[0] == statusPanic) {
      final error = envelopeException(
        statusPanic,
        BinaryReader(bytes)..readU8(),
      );
      (_actorZones[callId] ?? _zone).handleUncaughtError(
        error,
        StackTrace.empty,
      );
      return;
    }
    // A completion for a call we no longer know about would indicate a
    // bridge bug (call ids are never reused within an isolate).
    assert(false, 'frustrate: completion for unknown call id $callId');
  }

  // Lazy like the actor exports: a bridge crate with no stream members has
  // no generated reference to frustrate::stream, so the export may be
  // absent — and never needed. pause/resume share the cancel signature
  // (`void(u64)`).
  late final _StreamCancelDart _streamCancel = _primary
      .address(NativeExport.streamCancel)
      .cast<NativeFunction<_StreamCancelC>>()
      .asFunction<_StreamCancelDart>();
  late final _StreamCancelDart _streamPause = _primary
      .address(NativeExport.streamPause)
      .cast<NativeFunction<_StreamCancelC>>()
      .asFunction<_StreamCancelDart>();
  late final _StreamCancelDart _streamResume = _primary
      .address(NativeExport.streamResume)
      .cast<NativeFunction<_StreamCancelC>>()
      .asFunction<_StreamCancelDart>();

  // Lazy like the stream/actor exports: present exactly when the bridge
  // crate has a member that needs it.
  late final _CallbackRespondDart _callbackRespond = _primary
      .address(NativeExport.callbackRespond)
      .cast<NativeFunction<_CallbackRespondC>>()
      .asFunction<_CallbackRespondDart>();

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

  /// Looked up lazily: a bridge crate with no resident types never calls them.
  late final _ResidentAttachDart _residentAttach = _primary
      .address(NativeExport.residentAttach)
      .cast<NativeFunction<_ResidentAttachC>>()
      .asFunction<_ResidentAttachDart>();
  late final _ResidentReclaimableDart _residentReclaimable = _primary
      .address(NativeExport.residentReclaimable)
      .cast<NativeFunction<_ResidentReclaimableC>>()
      .asFunction<_ResidentReclaimableDart>();

  // Actor exports are looked up lazily: a bridge crate with no actor types
  // doesn't have them, and never needs them.
  late final _ActorSpawnDart _actorSpawn = _primary
      .address(NativeExport.actorSpawn)
      .cast<NativeFunction<_ActorSpawnC>>()
      .asFunction<_ActorSpawnDart>();
  late final _ActorShutdownDart _actorShutdown = _primary
      .address(NativeExport.actorShutdown)
      .cast<NativeFunction<_ActorShutdownC>>()
      .asFunction<_ActorShutdownDart>();
  late final _ActorRetainDart _actorRetain = _primary
      .address(NativeExport.actorRetain)
      .cast<NativeFunction<_ActorRetainC>>()
      .asFunction<_ActorRetainDart>();
  late final Pointer<NativeFunction<_DropC>> _actorReleaseFn = _primary
      .address(NativeExport.actorRelease)
      .cast<NativeFunction<_DropC>>();
  late final _DropDart _actorRelease = _actorReleaseFn.asFunction<_DropDart>();

  /// The GC backstop shared by every actor on this runtime.
  ///
  /// One finalizer, not one per host: `frustrate_actor_reap` is a single
  /// runtime export taking a counted host pointer as its token, so unlike
  /// [handleDrop]'s per-type `frustrate_finalize_<Type>` there is nothing to
  /// vary. A field so it stays reachable for the life of the runtime — a
  /// `NativeFinalizer` that is itself collected finalizes nothing.
  late final NativeFinalizer _actorReaper = NativeFinalizer(
    _primary.address(NativeExport.actorReap).cast<NativeFunction<_DropC>>(),
  );

  /// Releases the count a `_NativeActorHost` holds on its Rust host when the
  /// Dart object is collected. What keeps the pointer it passes on every call
  /// valid for exactly as long as the object that passes it.
  late final NativeFinalizer _actorHostRelease = NativeFinalizer(
    _actorReleaseFn,
  );

  bool get asyncIsParallel => true;

  int get hardwareParallelism => Platform.numberOfProcessors;

  /// Looked up on demand rather than as a `late final`: a bridge crate with no
  /// resident types does not export this, and reading the count is not worth
  /// making such a bridge fail to load. An absent export is an honest 0 —
  /// nothing can have been lost.
  int get residentLeakCount {
    try {
      return _primary
          .address(NativeExport.residentLeaked)
          .cast<NativeFunction<_ResidentLeakedC>>()
          .asFunction<_ResidentLeakedDart>()();
    } on ArgumentError {
      return 0;
    }
  }

  /// Every bridge's runtime, for routing what arrives on the one port.
  final List<NativeRuntime> _views = [];

  /// The router an event for [id] belongs to: the one that registered it, or,
  /// when none did, any one — a router drops a late stream event for an id it
  /// does not know and leaves a call completion to the pending calls. Every
  /// router would claim such a stream event, which is why the owner is looked
  /// for first.
  static StreamRouter _routerFor(List<NativeRuntime> views, int id) {
    if (views.length == 1) return views[0]._router;
    for (var i = 0; i < views.length; i++) {
      final router = views[i]._router;
      if (router.owns(id)) return router;
    }
    return views[0]._router;
  }
}

/// One bridge's view of its isolate's native transport: what generated code
/// reaches through `Frustrate.of`.
///
/// The bridge's own exports — its dispatch entries, its schema fingerprint,
/// its drop and finalize exports — resolve through its own [library], and its
/// pending calls and stream router are its own; the completion port and the
/// call-id sequence are the transport's, shared with every other bridge linked
/// into that library.
final class NativeRuntime implements FrustrateRuntime {
  final _NativeTransport _t;

  /// The library this bridge's exports resolve through.
  final BridgeLibrary library;

  @override
  final FrustrateBridge servedBridge;

  /// A runtime for [library]'s bridge over a transport of its own.
  ///
  /// [FrustrateNative.init] is how an app gets one; this is for a test that
  /// needs a second transport, such as one with a counting [allocator].
  /// Where request blocks come from is `malloc` in every shipping
  /// configuration.
  factory NativeRuntime(BridgeLibrary library, {Allocator? allocator}) =>
      _NativeTransport(library, allocator: allocator).runtimeFor(library);

  NativeRuntime._(this._t, this.library) : servedBridge = library.bridge;

  /// Join the transport's routing: from here on, what arrives on its port for
  /// an id this runtime issued reaches this runtime.
  void _join() => _t._views.add(this);

  /// This bridge's async calls, every actor call included: a native host
  /// dispatches through [_issueAsync], so there is one registry to count. Its
  /// own, so [inFlightCallCount] answers for this bridge alone; the transport
  /// reads every bridge's for keep-alive.
  final InFlightCalls _inFlight = InFlightCalls();

  late final PendingCalls _pending = PendingCalls(
    _t._allocId,
    onChanged: _t._updateKeepAlive,
    cancelCall: (id) => _t._callCancel(id) != 0,
    tally: _inFlight,
  );

  /// This bridge's streams and callbacks. Ids come from the transport's one
  /// sequence and events arrive on its one port, so ordering across kinds is
  /// the port's; the router being this bridge's is what lets
  /// [openChannelCount] answer for it alone.
  late final StreamRouter _router = StreamRouter(
    _t._allocId,
    respond: _t._respondToCallback,
    cancel: cancelStream,
    zone: _t._zone,
  );

  late final _CallSyncDart _callSync = library
      .address(NativeExport.callSync)
      .cast<NativeFunction<_CallSyncC>>()
      .asFunction<_CallSyncDart>();
  late final _CallAsyncDart _callAsync = library
      .address(NativeExport.callAsync)
      .cast<NativeFunction<_CallAsyncC>>()
      .asFunction<_CallAsyncDart>();
  @override
  BinaryReader callSync(
    int fnId,
    int sizeHint,
    void Function(BinaryWriter w) encode, {
    Object Function(BinaryReader)? typedError,
  }) {
    // ONE allocation carries the response slab AND the request: the slab at
    // [block], the request bytes immediately after it: one allocation and
    // one free, the shape the web transport shares, and a response that fits
    // the slab needs no `_bufferFree`.
    //
    // The ABI is shared, so the slab is here because it is the ABI — but it
    // is not a native concession: fewer allocator round trips is fewer
    // allocator round trips. The block
    // belongs to one call, Rust writes it only after the body has returned,
    // and the `finally` that frees it is the frame that allocated it.
    //
    // `malloc`, not `calloc`: every byte read back out was written by Rust
    // (the response) or by us (the request), so zeroing it first is work
    // nothing observes.
    //
    // The generated encoder writes its request DIRECTLY into this block — the
    // reason it arrives as a closure rather than as a finished writer: a Dart
    // heap buffer memcpy'd in would cost a full payload copy and allocation
    // per call, on the calling isolate's own thread.
    // The writer's own 64-byte floor, applied here too: a hint is a lower
    // bound, and undershooting it by a few bytes on a small call would trade a
    // 64-byte over-allocation for a whole realloc-and-copy.
    var block = _t._alloc.allocate<Uint8>(
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
        final next = _t._alloc.allocate<Uint8>(frustrateRespSlabBytes + newCap);
        if (written > 0) {
          _requestView(next, written).setAll(0, _requestView(block, written));
        }
        _t._alloc.free(block);
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
        _t._isolateTag,
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
      _t._bufferFree(p, len, lease[2]);
      return decodeEnvelope(resp, typedError: typedError);
    } finally {
      // Close BEFORE the free, so a writer stashed by a codec hook reports a
      // named StateError rather than writing into memory that is already gone.
      w.close();
      // `block`, not the one allocated above: a grow republished it, and the
      // frame frees whichever block is current.
      _t._alloc.free(block);
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

  // Always present (every generated glue emits it), so looked up eagerly. The
  // u64 return comes back signed-reinterpreted in the Dart int; normalize to
  // an unsigned 64-bit BigInt to compare with the compiled-in fingerprint.
  late final _SchemaHashDart _schemaHash = library
      .address(NativeExport.schemaHash)
      .cast<NativeFunction<_SchemaHashC>>()
      .asFunction<_SchemaHashDart>();

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
      _residentDrops[symbol] ??= _lookupResidentHandleDrop(symbol);

  HandleDrop _lookupResidentHandleDrop(String symbol) {
    final exports = library.handle(symbol);
    return ResidentHandleDrop(
      exports.finalize.cast<NativeFunction<_DropC>>().asFunction<_DropDart>(),
      exports.drop.cast<NativeFunction<_DropC>>().asFunction<_DropDart>(),
      _t._residentAttach,
      _t._residentReclaimable,
      _t._isolateId,
    );
  }

  HandleDrop _lookupHandleDrop(String symbol) {
    // The GC finalizer uses the type's finalize export. Both retire whatever
    // channels the object holds; they differ in what the terminal says. Eager
    // dispose() closes them normally, while the finalize variant ends them as
    // *leaked* and names the type — reaching it proves dispose() was never
    // called, because dispose() detaches this finalizer before dropping
    // (`stream::finalize_scope`).
    final exports = library.handle(symbol);
    return NativeHandleDrop(
      NativeFinalizer(exports.finalize.cast<NativeFunction<_DropC>>()),
      exports.drop.cast<NativeFunction<_DropC>>().asFunction<_DropDart>(),
    );
  }

  late final _ActorCallDart _actorCall = library
      .address(NativeExport.actorCall)
      .cast<NativeFunction<_ActorCallC>>()
      .asFunction<_ActorCallDart>();

  @override
  Future<ActorHost> spawnActorHost({String? debugName}) async {
    final owner = _t._allocId();
    final host = _NativeActorHost(this, _t._actorSpawn(owner), debugName);
    _t._actorZones[owner] = Zone.current;
    _t._actorZoneRelease.attach(host, owner);
    return host;
  }

  /// The transport: it issues every raw this runtime hands out, and every
  /// bridge in the same library shares it.
  @override
  Object get bridgeIdentity => _t;

  @override
  int get inFlightCallCount => _inFlight.count;

  @override
  bool get asyncIsParallel => true;

  @override
  int get hardwareParallelism => _t.hardwareParallelism;

  @override
  int get openChannelCount => _router.openRegistrationCount;

  @override
  List<String> get openChannelLabels => _router.openChannelLabels;

  @override
  int get residentLeakCount => _t.residentLeakCount;

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
      var reqPtr = _t._alloc.allocate<Uint8>(cap);
      final w = BinaryWriter.external(reqPtr.asTypedList(cap), (
        newCap,
        written,
      ) {
        final next = _t._alloc.allocate<Uint8>(newCap);
        if (written > 0) {
          next.asTypedList(written).setAll(0, reqPtr.asTypedList(written));
        }
        _t._alloc.free(reqPtr);
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
        _t._alloc.free(reqPtr);
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
    _t._updateKeepAlive();
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
    _t._updateKeepAlive();
    return id;
  }

  @override
  void cancelStream(int id) {
    _router.cancelLocal(id);
    // Retiring the local registration may leave the isolate with no live work
    // (a cancel from a spawned isolate that has already returned from `main`);
    // recompute so it can exit. NB: the *producer* may still post once more
    // before it observes the flag — the narrow "posts racing isolate death"
    // window closed separately by a post.rs dead-isolate net.
    _t._updateKeepAlive();
    // The registry is process-global shared memory: the flag is visible to
    // whatever thread runs the producer (pool, actor executor, or a later
    // call holding a stored sink) immediately.
    _t._streamCancel(id);
  }

  // Backpressure signals the shared registry too; unlike cancel they retire no
  // registration, so keep-alive is untouched (the stream stays open — an open
  // registration already pins the isolate).
  @override
  void pauseStream(int id) => _t._streamPause(id);

  @override
  void resumeStream(int id) => _t._streamResume(id);

  @override
  void failStream(int id, Object error, StackTrace st) {
    // `fail()` removes the registration, so keep-alive must recompute — else an
    // opening-call error that retires the sink before the Rust sink is even
    // constructed would pin the isolate forever.
    _router.fail(id, error, st);
    _t._updateKeepAlive();
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
    _t._updateKeepAlive();
    return id;
  }
}

/// A dedicated Rust thread (see runtime/rust/src/actor.rs). Completions
/// arrive through the runtime's single post listener; only the issue path
/// differs from a pool call.
///
/// Holds a counted pointer to the Rust host and passes it on every call, so a
/// call reaches its executor's queue with no lookup and no lock. The count is
/// released by a finalizer when this object is collected, which is what keeps
/// the pointer valid for as long as anything can pass it: hence
/// [Finalizable].
final class _NativeActorHost implements ActorHost, Finalizable {
  final NativeRuntime _rt;
  final Pointer<Void> _host;

  /// The actor's type name (generated constructors pass it), so
  /// host-attributed errors — the disposed-deferred StateError above all —
  /// name the actor rather than an anonymous host.
  final String? _debugName;
  bool _stopped = false;

  /// Deferred calls in flight on this host (`ActorHost.call(deferred:)`).
  /// Added at issue, removed when the future settles; what remains at
  /// [shutdown] is exactly the set the dispose contract cancels.
  final Set<int> _deferredInFlight = {};

  /// Whether the GC backstop holds a count of its own ([attachReaper]).
  bool _reaperAttached = false;

  _NativeActorHost(this._rt, this._host, this._debugName) {
    _rt._t._actorHostRelease.attach(this, _host.cast());
  }

  @override
  Object get bridgeIdentity => _rt.bridgeIdentity;

  /// Refusing here rather than letting the call reach Rust is what makes the
  /// two platforms agree: web's worker is already gone after [shutdown], so it
  /// has to answer locally, and Rust's own dead-host reply is a panic envelope
  /// (actor.rs) — a different exception for the same caller mistake. Both
  /// platforms now reject with the same StateError, and Rust's envelope stays
  /// as the backstop.
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
    // The host's own registry of deferred calls is retired by each future's
    // drop guard, and swept by the host thread when it stops (actor.rs).
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
        _rt._actorCall(_host, callId, fnId, reqPtr, len);
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
      if (_rt._t._callCancel(id) != 0) {
        _rt._pending.fail(
          id,
          disposedDeferredError(_debugName),
          StackTrace.current,
        );
      }
    }
    _rt._t._actorShutdown(_host);
  }

  /// The token is a counted pointer to the host of its own
  /// (`frustrate_actor_retain`), which the reap releases: the host object and
  /// the actor handle may be collected in either order. The actor's objects
  /// are already held by the executor's own table (`handle::actor_new`), so
  /// the host is all the reap path needs.
  @override
  void attachReaper(covariant Finalizable owner) {
    _rt._t._actorReaper.attach(
      owner,
      _rt._t._actorRetain(_host).cast(),
      detach: owner,
    );
    _reaperAttached = true;
  }

  /// Detaching does not run the finalizer, so the backstop's count is
  /// released here instead.
  @override
  void detachReaper(Object owner) {
    _rt._t._actorReaper.detach(owner);
    if (_reaperAttached) {
      _reaperAttached = false;
      _rt._t._actorRelease(_host);
    }
  }
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
