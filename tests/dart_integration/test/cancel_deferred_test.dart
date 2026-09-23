/// `FrustrateCancelToken` on a **`Deferred` actor completion**.
///
/// The load-bearing assertion is the same one `cancel_async_test.dart` makes,
/// for the same reason: **the Rust future must be dropped**, not merely
/// unlistened-to. `Sensor.deferredUntilCancelled` holds a drop sensor across
/// its suspension point, so a runtime that claimed the call and leaked the task
/// fails here while passing any test that only watches the Dart future.
///
/// Two things differ from the plain-`async fn` case, and both are actor
/// physics:
///
///  - The sensor is read through an **actor method**. On web the future lives
///    in that actor's own worker instance, so a `static` in the page instance
///    is a different `static` and would answer for the wrong memory.
///  - The sensor is built in the **prefix** and moved into the future, not
///    constructed by the future's own body. That is what lets these tests fence
///    on the actor's FIFO instead of a clock: an `async` body does not run
///    until its first poll, so a body-built sensor cannot report a cancel that
///    beat the executor to the task — the future is dropped correctly and there
///    is simply nothing inside it to say so. `Deferred` makes the better shape
///    available, because a prefix is ordinary synchronous code.
///
/// No clocks in the preconditions: the actor's FIFO is the fence. Awaiting
/// `startedCount()` proves the deferred prefix has run and its task is on the
/// executor, which is what makes the "not dropped yet" assertion mean
/// something. Only the drop itself is polled, because it lands on a later
/// drain that this isolate does not run.
///
/// One test breaks both patterns and says so at its own site: the queued-cancel
/// one is `testOn: 'vm'` and blocks the host thread for a fixed time, because
/// "queued, not yet started" is a state only the native FIFO has — a web cancel
/// is a message behind the call it would claim, so the call has always started.
@Timeout(Duration(minutes: 2))
library;

import 'dart:async';

import 'package:frustrate/frustrate.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

/// Wait for the deferred future's drop, which happens on a drain this isolate
/// does not run — a pool worker on native, the worker's `schedule_drain`
/// microtask on web. Bounded, and reported as a failure rather than a hang.
Future<void> awaitDropped(Sensor sensor) async {
  final deadline = DateTime.now().add(const Duration(seconds: 10));
  var dropped = await sensor.futureWasDropped();
  while (!dropped && DateTime.now().isBefore(deadline)) {
    await Future<void>.delayed(const Duration(milliseconds: 5));
    dropped = await sensor.futureWasDropped();
  }
  expect(
    dropped,
    isTrue,
    reason:
        'a claimed cancel must drop the Rust future — the task is still '
        'in the actor executor\'s registry, which is a leak, not a cancel',
  );
}

void main() {
  setUpAll(initBridge);

  setUp(resetCancelSensor);

  tearDown(() {
    expect(
      Frustrate.instance.openChannelCount,
      0,
      reason:
          'cancelling must leave no channel registered. Open: '
          '${Frustrate.instance.openChannelLabels}',
    );
  });

  test(
    'cancelling a suspended deferred completion rejects it and drops it',
    () async {
      final sensor = await Sensor.new_();
      final token = FrustrateCancelToken();
      final call = sensor.deferredUntilCancelled(token: 5, cancel: token);

      // The fence, and it is an ordering fact rather than a delay: this actor
      // is a FIFO, so `startedCount` answering 1 proves the deferred prefix has
      // already run and handed its future to the executor. Without it the
      // assertion below could pass against a call that had not reached Rust.
      expect(await sensor.startedCount(), 1);
      expect(
        await sensor.futureWasDropped(),
        isFalse,
        reason:
            'the future must still be alive, parked on the latch, before '
            'the cancel',
      );

      token.cancel();
      expect(token.isCancelled, isTrue);

      await expectLater(call, throwsA(isA<CancelledCallException>()));
      await awaitDropped(sensor);
      await sensor.dispose();
    },
  );

  test(
    'the actor keeps serving: a cancel is a drop, not a lifecycle',
    () async {
      final sensor = await Sensor.new_();
      final token = FrustrateCancelToken();
      final abandoned = sensor.deferredUntilCancelled(token: 1, cancel: token);
      expect(await sensor.startedCount(), 1);

      token.cancel();
      await expectLater(abandoned, throwsA(isA<CancelledCallException>()));
      await awaitDropped(sensor);

      // The whole reason this exists rather than `dispose()`: the instance is
      // still there, still serving, and a later deferred call still completes.
      final kept = sensor.deferredUntilCancelled(token: 7);
      expect(await sensor.startedCount(), 2);
      await sensor.release(value: 100);
      expect(await kept, 107, reason: 'latch value + token');
      await sensor.dispose();
    },
  );

  test(
    'one token cancels one call, not every deferred call on the actor',
    () async {
      final sensor = await Sensor.new_();
      final token = FrustrateCancelToken();
      final cancelled = sensor.deferredUntilCancelled(token: 1, cancel: token);
      // Bound to no token at all — the contrast with `dispose()`, which cancels
      // both.
      final untouched = sensor.deferredUntilCancelled(token: 2);
      expect(await sensor.startedCount(), 2);

      token.cancel();
      await expectLater(cancelled, throwsA(isA<CancelledCallException>()));
      await awaitDropped(sensor);

      await sensor.release(value: 50);
      expect(
        await untouched,
        52,
        reason:
            'the other completion was never claimed, so it answers '
            'normally — per-call, which dispose() cannot be',
      );
      await sensor.dispose();
    },
  );

  test('a cancelled token refuses a later deferred call before it is issued', () async {
    final sensor = await Sensor.new_();
    final token = FrustrateCancelToken()..cancel();
    await expectLater(
      sensor.deferredUntilCancelled(token: 1, cancel: token),
      throwsA(isA<CancelledCallException>()),
    );
    // Nothing reached Rust: the latch refuses ahead of encoding, so the prefix
    // never ran. Deterministic on the actor, where a queued call would still
    // have been counted by the time this answers.
    expect(
      await sensor.startedCount(),
      0,
      reason: 'a refused call must not be dispatched to the executor',
    );
    await sensor.dispose();
  });

  test('cancelling after the answer leaves the answer alone', () async {
    final sensor = await Sensor.new_();
    final token = FrustrateCancelToken();
    // Awaited first, so this deferred completion has certainly answered
    // before the cancel.
    expect(await sensor.deferredAtOnce(value: 41, cancel: token), 42);
    token.cancel();
    // The call was no longer in flight, so nothing was failed. The proof it
    // did not double-settle is that this test does not die with "Future
    // already completed" in the zone.
    expect(token.isCancelled, isTrue);
    await sensor.dispose();
  });

  test('one token spans a plain async fn and a deferred actor call', () async {
    // The "one vocabulary" claim, made executable. These two calls run on two
    // different cooperative executors — the page/process one and the actor's,
    // which on web is another Worker's wasm instance — and a single
    // `cancel()` drops both futures.
    final sensor = await Sensor.new_();
    // Warm the transport's own executor before measuring it. An answered
    // `async fn` proves a drain has run — which on threaded web means the pool
    // workers have finished booting — and that matters for what `awaitsUntil
    // Cancelled` can observe: its sensor is constructed *by the body*, so it
    // exists only once the task has been polled at least once. Cancel a task
    // the executor has not reached yet and the future is dropped correctly
    // while having no sensor in it to say so. (`Sensor`'s own drop sensor is
    // built in the prefix and moved into the future, so it is immune — see
    // this file's header.)
    expect(await answersAtOnce(x: 1), 2);
    final token = FrustrateCancelToken();
    final errors = <Object>[];
    // Handlers attached as each future is created: cancelling rejects both at
    // once, and a rejection nobody is listening to yet is an unhandled async
    // error (see FrustrateCancelToken's doc).
    final settled = [
      awaitsUntilCancelled(cancel: token).then<void>(
        (_) => fail('a cancelled call must not answer'),
        onError: errors.add,
      ),
      sensor
          .deferredUntilCancelled(token: 1, cancel: token)
          .then<void>(
            (_) => fail('a cancelled call must not answer'),
            onError: errors.add,
          ),
    ];
    // The actor's fence is the FIFO; the transport's has no equivalent, so
    // give it a turn to poll and park, as cancel_async_test.dart does.
    await Future<void>.delayed(const Duration(milliseconds: 20));
    expect(await sensor.startedCount(), 1);

    token.cancel();
    await Future.wait(settled);

    expect(errors, hasLength(2));
    expect(errors, everyElement(isA<CancelledCallException>()));
    // Both drops, each read from the memory that owns it: a free function for
    // the page instance, an actor method for the worker's.
    await awaitDropped(sensor);
    final deadline = DateTime.now().add(const Duration(seconds: 10));
    while (!cancelledFutureWasDropped() && DateTime.now().isBefore(deadline)) {
      await Future<void>.delayed(const Duration(milliseconds: 5));
    }
    expect(
      cancelledFutureWasDropped(),
      isTrue,
      reason: 'the plain async fn half of the same token must drop too',
    );
    await sensor.dispose();
  });

  test('cancel-then-dispose settles as cancelled, not as disposed', () async {
    // The two levers racing, with no turn given to the cancel in between. Both
    // platforms must answer with the *cancellation*, because that is what
    // happened first — and on web that is an ordering claim worth pinning
    // rather than asserting in a comment: the claim travels to the worker as a
    // message, and `dispose()`'s drop call queues behind it, so the worker
    // cannot answer the drop until it has replied to the cancel.
    final sensor = await Sensor.new_();
    final token = FrustrateCancelToken();
    Object? failure;
    final settled = sensor
        .deferredUntilCancelled(token: 1, cancel: token)
        .then<void>(
          (_) => fail('a cancelled call must not answer'),
          onError: (Object e) => failure = e,
        );
    expect(await sensor.startedCount(), 1);

    token.cancel();
    await sensor.dispose();
    await settled;
    expect(
      failure,
      isA<CancelledCallException>(),
      reason:
          'the caller cancelled this call before disposing the actor, so '
          'it must not be reported as a disposed-with-a-completion-'
          'outstanding StateError',
    );
  });

  test('a cancel claims a call that is still queued behind a slow one', () async {
    // Native only, because the state under test has no web analogue: there
    // the claim travels as a `cancel-call` message, `postMessage` is FIFO, and
    // the worker therefore always finishes the call ahead of it — a web
    // deferred call has started by the time any cancel is seen. Natively the
    // call sits in its host's FIFO, and until this window was closed a token
    // cancelled here claimed nothing and the call answered normally.
    //
    // The fence is the FIFO, not a clock: `sleepOnExecutor` owns the host
    // thread while the next two statements run in this same event-loop turn,
    // so the deferred call is certainly still queued when the cancel lands.
    // A machine slow enough to break that assumption fails on `calls`, not by
    // hanging: the cancel would then claim the spawned task instead, reject
    // the future, and leave `calls` at 2.
    //
    // Red fence: with the reservation removed from `actor::submit` the cancel
    // claims nothing, `queued` never settles, and this test times out.
    final m = await Miner.new_(label: 'queued-cancel');
    final token = FrustrateCancelToken();
    final slow = m.sleepOnExecutor(millis: 300);
    final queued = m.deferredWait(token: 1, cancel: token);
    token.cancel();

    await expectLater(queued, throwsA(isA<CancelledCallException>()));
    await slow;
    // The load-bearing assertion, and it is Rust-side state rather than the
    // rejected future above: `deferredWait`'s prefix increments `calls`, so
    // only the sleep counted. The job was dropped in the queue, never run —
    // which a runtime that merely stopped listening could not produce (it
    // would run the prefix, count 2, and answer a call nobody owns).
    expect(
      await m.calls(),
      1,
      reason: 'the cancelled call must never have reached its prefix',
    );

    // And the actor is untouched: still serving, still able to complete a
    // later deferred call.
    final kept = m.deferredWait(token: 7);
    await m.release(value: 100);
    expect(await kept, 107, reason: 'gate value + token');
    await m.dispose();
  }, testOn: 'vm');

  test('dispose still cancels an outstanding deferred completion', () async {
    // The all-at-once lever is unchanged by the per-call one: a completion
    // nobody cancelled is still the actor's to cancel at dispose(), with the
    // named StateError.
    final sensor = await Sensor.new_();
    Object? failure;
    final settled = sensor
        .deferredUntilCancelled(token: 1)
        .then<void>(
          (_) => fail('dispose must cancel an outstanding completion'),
          onError: (Object e) => failure = e,
        );
    expect(await sensor.startedCount(), 1);
    await sensor.dispose();
    await settled;
    expect(failure, isA<StateError>());
  });
}
