/// ActorPool: fan stateless jobs across N actor instances.
///
/// A convenience layer, not core: generated code never references this
/// file, and everything here is expressible in application code over the
/// public actor surface. It exists so the policy everyone needs — who is
/// free, what a panic does to the pool, how teardown drains — is written
/// once, against the contracts, and tested once.
library;

import 'dart:async';
import 'dart:collection';

// The same conditional pair frustrate.dart exports: on native `ActorHandle`
// carries dart:ffi's `Finalizable`, which a pool's type bound must not lose.
import 'actor_handle_native.dart'
    if (dart.library.js_interop) 'actor_handle_web.dart';
import 'exceptions.dart';
import 'runtime_core.dart';

/// A fixed-width pool of one actor type: [run] leases an instance
/// exclusively for one job, idle-first, FIFO-queued past the width.
///
/// Each instance is a real executor (a thread on native, a worker-hosted
/// wasm instance on web), so a pool delivers
/// genuine parallelism on every platform, including single-threaded web
/// where plain async calls run inline on the caller.
///
/// Width is an invariant: a job that fails with [BridgePanicException]
/// fails alone — the instance it leased is retired and respawned through
/// the same spawn closure. Pool instances are therefore replaceable by
/// definition: keep state a job may not survive losing in a bare actor,
/// not a pool.
final class ActorPool<T extends ActorHandle> {
  final Future<T> Function(int index) _spawn;

  /// One slot per instance; null while the slot respawns after a panic.
  final List<T?> _slots;

  final Queue<int> _idle;
  final Queue<Completer<int>> _waiters;

  /// Slots granted (or respawning) and not yet back in [_idle].
  int _leased = 0;

  Future<void>? _disposing;
  Completer<void>? _drained;

  /// A failed respawn: the pool is deterministically broken — queued and
  /// future jobs all surface this, loudly, instead of a silent width shrink.
  StateError? _failure;

  ActorPool._(this._spawn, List<T> instances)
    : _slots = List<T?>.of(instances),
      _idle = Queue.of(Iterable<int>.generate(instances.length)),
      _waiters = Queue();

  /// Spawns [size] instances concurrently — [spawn] is called once per
  /// slot index, `0..size-1` (and again for a slot being respawned after
  /// a panic) — and completes when every instance is ready. If any spawn
  /// fails, the instances that did spawn are disposed and the first
  /// failure rethrown.
  ///
  /// [size] defaults to `Frustrate.instance.hardwareParallelism`. On web
  /// every instance carries its own wasm linear memory — pass [size]
  /// explicitly for actors with large resident state.
  static Future<ActorPool<T>> spawn<T extends ActorHandle>(
    Future<T> Function(int index) spawn, {
    int? size,
  }) async {
    final width = size ?? Frustrate.instance.hardwareParallelism;
    if (width < 1) {
      throw ArgumentError.value(size, 'size', 'an ActorPool needs >= 1');
    }
    final outcomes = await Future.wait(
      List<Future<(T?, Object?, StackTrace?)>>.generate(width, (i) async {
        try {
          return (await spawn(i), null, null);
        } catch (e, st) {
          return (null, e, st);
        }
      }),
    );
    for (final (_, e, st) in outcomes) {
      if (e == null) continue;
      await Future.wait([
        for (final (t, _, _) in outcomes)
          if (t != null) t.dispose(),
      ]);
      Error.throwWithStackTrace(e, st!);
    }
    return ActorPool._(spawn, [for (final (t, _, _) in outcomes) t as T]);
  }

  /// The pool's width. Fixed at [spawn]; an invariant across panics.
  int get size => _slots.length;

  /// Runs [job] against one instance, leased exclusively for the whole
  /// closure — no other job's calls interleave with it. Grants an idle
  /// instance immediately, else queues FIFO (unbounded).
  ///
  /// Whatever [job] throws propagates unchanged. A [BridgePanicException]
  /// additionally retires the leased instance and respawns its slot.
  /// [job] must not dispose the actor — the pool owns instance lifecycle.
  Future<R> run<R>(Future<R> Function(T actor) job) async {
    final slot = await _acquire();
    var retired = false;
    try {
      return await job(_slots[slot] as T);
    } on BridgePanicException {
      retired = true;
      _retireAndRespawn(slot);
      rethrow;
    } finally {
      if (!retired) _release(slot);
    }
  }

  /// [run] for a streaming job: the lease is acquired when the returned
  /// stream is listened to and held until it completes, errors, or the
  /// listener cancels. Panic errors retire the instance like [run].
  Stream<R> stream<R>(Stream<R> Function(T actor) job) async* {
    final slot = await _acquire();
    var retired = false;
    try {
      yield* job(_slots[slot] as T);
    } on BridgePanicException {
      retired = true;
      _retireAndRespawn(slot);
      rethrow;
    } finally {
      if (!retired) _release(slot);
    }
  }

  /// Drain-then-stop, like actor dispose: new jobs throw StateError,
  /// jobs already accepted (running and queued) complete, then every
  /// instance is disposed. Idempotent; concurrent calls share one drain.
  Future<void> dispose() => _disposing ??= _dispose();

  Future<void> _dispose() async {
    if (_leased > 0 || _waiters.isNotEmpty) {
      final drained = _drained = Completer<void>();
      await drained.future;
    }
    final instances = _slots.whereType<T>().toList();
    for (var i = 0; i < _slots.length; i++) {
      _slots[i] = null;
    }
    await Future.wait(instances.map((t) => t.dispose()));
  }

  /// `async` so the two refusals below reject the returned future rather than
  /// throwing synchronously — the same contract the transports hold for
  /// `callAsync`/`ActorHost.call`.
  Future<int> _acquire() async {
    if (_disposing != null) {
      throw StateError('frustrate: this ActorPool was disposed');
    }
    final failure = _failure;
    if (failure != null) {
      throw failure;
    }
    if (_idle.isNotEmpty) {
      _leased++;
      return _idle.removeFirst();
    }
    final waiter = Completer<int>();
    _waiters.add(waiter);
    return waiter.future;
  }

  /// Hand the slot to the oldest waiter (the lease transfers), else idle
  /// it. Also the drain signal for [dispose].
  void _release(int slot) {
    if (_waiters.isNotEmpty) {
      _waiters.removeFirst().complete(slot);
      return;
    }
    _leased--;
    _idle.add(slot);
    _maybeDrained();
  }

  /// Retire a panicked instance and respawn its slot, detached from the
  /// failing job (which is busy rethrowing the panic to its caller). The
  /// lease stays held until the replacement is ready, so no waiter can be
  /// granted an empty slot.
  void _retireAndRespawn(int slot) {
    final old = _slots[slot];
    _slots[slot] = null;
    unawaited(
      Future(() async {
        try {
          await old?.dispose();
        } catch (_) {
          // The drop call may itself panic (the instance just trapped);
          // generated dispose() still releases the executor (its shutdown
          // runs in a finally), so swallowing here strands nothing.
        }
        try {
          final fresh = await _spawn(slot);
          _slots[slot] = fresh;
          _release(slot);
        } catch (e) {
          _poison(e);
        }
      }),
    );
  }

  void _poison(Object cause) {
    final failure = _failure ??= StateError(
      'frustrate: this ActorPool is broken — respawning a panicked '
      'instance failed ($cause); jobs cannot be scheduled',
    );
    while (_waiters.isNotEmpty) {
      _waiters.removeFirst().completeError(failure);
    }
    _leased--; // The retired slot's lease ends here; it never comes back.
    _maybeDrained();
  }

  void _maybeDrained() {
    if (_disposing != null &&
        _leased == 0 &&
        _waiters.isEmpty &&
        !(_drained?.isCompleted ?? true)) {
      _drained!.complete();
    }
  }
}
