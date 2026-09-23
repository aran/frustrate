/// The AWAITED twin of `callback_liveness_test`.
///
/// The hazard: an isolate that **accepts** an awaited invocation
/// (`post::deliver` returned true) and then dies holding it refuses nothing,
/// so nothing reaches `post::mark_gone` and `callback::fail_invocations_for`
/// never sweeps. What leaks is the executor task and with it the enclosing
/// bridge call — it never completes, the caller's `PendingCalls._pending`
/// never empties, and `keepIsolateAlive` stays latched, so a caller that is
/// itself a spawned isolate can no longer exit. Test 1 asserts that
/// consequence, which is why the caller here is a spawned isolate and not the
/// suite. `Isolate.current.addOnExitListener` (`post::handle_exit`) is what
/// closes it.
///
/// No pool worker is held (test 3 asserts that), which is why this file cannot
/// reuse the pool-narrowing shape.
///
/// Reaching this at all needs a member that is an `async fn` awaiting a
/// `DartFunction` owned by *another* isolate. The two shipped `call_async`
/// fixtures (`transform`, `Transforms::apply_transform`) take the closure as a
/// parameter, so its owner is the caller and cannot die independently; the
/// parked-slot fixture `call_parked_async` exists for this file.
///
/// Native-only by construction, like its blocking twin: `post::wasm::deliver`
/// is unconditionally `true` and the sweep is `#[cfg(not(wasm))]`. (The Rust
/// fixture is an `async fn`, hence portable, hence present on the web surface —
/// it is simply uncallable there, since nothing can fill a slot without the
/// native-only `park_function`.)
///
/// Its own target, and not an addition to `callback_liveness_test.dart`: one
/// parked awaited call latches `keepIsolateAlive` on whichever isolate made it,
/// so a regression here sharing that file would drag its three tests into an
/// empty-log Bazel TIMEOUT. For the same reason every isolate and every
/// `ReceivePort` this file opens is registered for teardown **before the first
/// await** — a `fail(...)` exits the test body early, and anything left latched
/// after that turns a legible failure into an unexplained target timeout
/// (rules_dart runs the suite in the child process's ROOT isolate and captures
/// its output with `Process.runSync`, so a process that never exits prints
/// nothing at all).
@TestOn('vm')
library;

import 'dart:async';
import 'dart:io';
import 'dart:isolate';

import 'package:frustrate/frustrate.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart';

/// Bound for anything that should settle promptly. Generous: these bounds
/// exist to turn a hang into a report, not to measure anything.
const _bound = Duration(seconds: 10);

/// A closure that takes far longer than any plausible internal cadence, so the
/// negative test cannot pass merely by finishing first. There is no cadence on
/// this path — the fix is an event, not a poll — so this guards against a
/// future one, and matches the blocking twin's constant so the two negatives
/// stay comparable.
const _slowClosure = Duration(milliseconds: 2600);

void main() {
  setUpAll(initBridge);

  // The same ledger guard `callback_liveness_test` runs, and load-bearing for
  // the same reason: a parked closure owned by THIS isolate is an open
  // registration, which pins the suite and hangs process exit after "All
  // tests passed!".
  //
  // It does not (and must not) settle test 1's leak. `clear_parked_functions`
  // drops the registry's `DartFunction`, but drop-retire posts from the LAST
  // `Arc<Inner>`, and the leaked executor task still holds two (the local `f`
  // captured across the await, and `CallFuture.end`) — so no end event is
  // posted to the dead isolate, nothing is refused, and nothing is swept. The
  // pin depends on that: if task cancellation on caller death is ever added,
  // dropping those clones WOULD post to the dead owner and sweep, and this
  // file needs re-deriving rather than deleting.
  tearDown(() async {
    clearParkedFunctions();
    for (var i = 0; i < 200 && Frustrate.instance.openChannelCount != 0; i++) {
      await Future<void>.delayed(const Duration(milliseconds: 5));
    }
    expect(
      Frustrate.instance.openChannelCount,
      0,
      reason:
          'a parked callback outlived its test and pins this isolate. '
          'Still open: ${Frustrate.instance.openChannelLabels}',
    );
  });

  test('an awaited call the owner accepted before dying settles, and its '
      'caller isolate can exit', () async {
    // Everything that could latch an isolate is registered for teardown BEFORE
    // anything is spawned. On the red path `fail(...)` throws out of the body
    // and none of the later statements run; the caller isolate is latched by
    // the very defect under test, so without this the target cannot report its
    // own failure.
    final ready = ReceivePort();
    final invoked = ReceivePort();
    final exitedOwner = ReceivePort();
    final report = ReceivePort();
    final exitedCaller = ReceivePort();
    final errors = ReceivePort();
    final childErrors = <Object?>[];
    errors.listen(childErrors.add);
    Isolate? owner;
    Isolate? caller;
    addTearDown(() {
      owner?.kill(priority: Isolate.immediate);
      caller?.kill(priority: Isolate.immediate);
      for (final port in [
        ready,
        invoked,
        exitedOwner,
        report,
        exitedCaller,
        errors,
      ]) {
        port.close();
      }
    });

    // Deterministic in both halves, exactly as the blocking twin: the owner's
    // closure reports that it is running, and it can only run by being
    // dequeued from the owner's port — strictly stronger than "the post was
    // accepted". By the time anything is killed the invocation is provably
    // registered, delivered, dispatched, and unanswered.
    owner = await Isolate.spawn(
      _parkAndNeverAnswer,
      [ready.sendPort, invoked.sendPort],
      onExit: exitedOwner.sendPort,
      onError: errors.sendPort,
    );
    final slot = await ready.first.timeout(
      _bound,
      onTimeout: () =>
          fail('the owner isolate never parked a closure${_ctx(childErrors)}'),
    ) as int;

    // `onExit`/`onError` at spawn rather than `addOnExitListener` after it:
    // post-fix this isolate exits on its own, and a listener attached after
    // the fact can miss an exit that already happened.
    caller = await Isolate.spawn(
      _callParkedAsyncAndReport,
      [report.sendPort, slot],
      onExit: exitedCaller.sendPort,
      onError: errors.sendPort,
    );

    await invoked.first.timeout(
      _bound,
      onTimeout: () => fail(
        'the parked closure never ran, so no invocation '
        'was ever accepted${_ctx(childErrors)}',
      ),
    );
    expect(
      parkedAsyncCallState(),
      1,
      reason: 'the bridged async fn must be suspended inside call_async',
    );

    // The one way an isolate ends while it still owes a callback response.
    // Ordering is load-bearing: kill first and the first poll's `deliver`
    // would refuse, panic DEAD_CONSUMER immediately, and this test would pass
    // without ever engaging the defect.
    owner.kill(priority: Isolate.immediate);
    await exitedOwner.first.timeout(
      _bound,
      onTimeout: () => fail('the owner isolate did not die'),
    );

    // Pre-fix neither of the next two ever happens: nothing posts to the dead
    // isolate again, so nothing refuses, so nothing sweeps the `Waiter::Async`.
    final outcome = await report.first.timeout(
      _bound,
      onTimeout: () => fail(
        'the bridge call never completed — the executor '
        'task is still parked on a response from a dead isolate. '
        'parkedAsyncCallState()=${parkedAsyncCallState()} '
        '(1 = suspended in DartFunction::call_async, 2 = returned)'
        '${_ctx(childErrors)}',
      ),
    );
    expect(
      outcome,
      'BridgePanicException',
      reason:
          'a dead owner must surface the same way it does on the '
          'blocking path — attributably, on the enclosing call. If a fix '
          'deliberately chose another shape, this is the line to review.',
    );

    await exitedCaller.first.timeout(
      _bound,
      onTimeout: () => fail(
        'the caller isolate can no longer exit: its '
        'bridge call never completed, so keepIsolateAlive stays '
        'latched',
      ),
    );
  });

  test('a slow-but-alive closure still returns — liveness is not a deadline', () async {
    // The control, and what gives the pin teeth: without it, a fix that simply
    // failed every awaited invocation on a timer would pass test 1. The
    // closure is registered in THIS isolate and nothing is killed, so the
    // owner is alive throughout — and answers late.
    //
    // Not a self-deadlock even though this isolate is both caller and owner:
    // the first poll runs on a pool worker, posts, and returns `Pending`
    // (freeing the worker), the closure then runs synchronously on this
    // isolate's event loop, and `frustrate_callback_respond` wakes the task
    // onto the pool from here. Nothing on the Rust side waits on this thread.
    final slot = await parkFunction(
      f: (v) {
        final until = DateTime.now().add(_slowClosure);
        while (DateTime.now().isBefore(until)) {
          // Synchronous by contract: `StreamRouter._runInvocation` calls the
          // closure and envelopes its return value, so a `Future` would not be
          // awaited. "Take a long time" therefore has no yielding form.
        }
        return v * 2;
      },
    ).timeout(_bound);

    expect(
      await callParkedAsync(
        slot: slot,
        value: 21,
      ).timeout(const Duration(seconds: 30)),
      42,
      reason: 'a slow-but-alive owner must still be awaited to completion',
    );
    expect(
      parkedAsyncCallState(),
      2,
      reason: 'and the bridged async fn resumed past its await',
    );
  });

  test('parked awaited calls hold no pool worker, even once every owner is '
      'dead', () async {
    // The discriminator, and the guard against the fixture regressing from
    // `call_async` to the blocking `call`: that variant would pass tests 1 and
    // 2 unchanged (the blocking path's cure has already shipped) while parking
    // one pool worker per call — and then ordinary pool work would never run.
    //
    // Width from Rust (`poolWidth`), never `Platform.numberOfProcessors`: the
    // pool sizes itself from `available_parallelism()`, which a cgroup quota
    // clamps below the processor count under Bazel.
    final width = poolWidth();
    expect(width, greaterThan(0));

    final ready = ReceivePort();
    final invoked = ReceivePort();
    final exits = ReceivePort();
    final errors = ReceivePort();
    final childErrors = <Object?>[];
    errors.listen(childErrors.add);
    final owners = <Isolate>[];
    Isolate? caller;
    addTearDown(() {
      for (final owner in owners) {
        owner.kill(priority: Isolate.immediate);
      }
      caller?.kill(priority: Isolate.immediate);
      for (final port in [ready, invoked, exits, errors]) {
        port.close();
      }
    });

    // Listeners attach now, so nothing sent before the awaits is missed.
    final allSlots = _collect(ready, width);
    final allInvoked = _collect(invoked, width);
    final allExited = _collect(exits, width);

    for (var i = 0; i < width; i++) {
      owners.add(
        await Isolate.spawn(
          _parkAndNeverAnswer,
          [ready.sendPort, invoked.sendPort],
          onExit: exits.sendPort,
          onError: errors.sendPort,
        ),
      );
    }
    final slots = (await allSlots.timeout(
      _bound,
      onTimeout: () => fail(
        'not every owner parked a closure'
        '${_ctx(childErrors)}',
      ),
    )).cast<int>();

    // ONE caller for all of them, and a spawned one: pre-fix these calls never
    // settle, so whoever makes them is latched for good. Putting them in a
    // throwaway isolate keeps that away from the suite.
    caller = await Isolate.spawn(_callAllParkedAsync, [
      slots,
    ], onError: errors.sendPort);
    await allInvoked.timeout(
      _bound,
      onTimeout: () =>
          fail('not every parked closure was invoked${_ctx(childErrors)}'),
    );
    expect(parkedAsyncCallState(), 1);

    // Kill every owner, so this is the *post-death* state and not merely
    // "a `Pending` future parks no thread" (which executor.rs already unit
    // pins). Pre-fix the invocations stay parked forever after this; that is
    // the point — the claim is that the pool is unaffected regardless.
    for (final owner in owners) {
      owner.kill(priority: Isolate.immediate);
    }
    await allExited.timeout(
      _bound,
      onTimeout: () => fail('not every owner isolate died'),
    );

    expect(
      await sumSquares(n: 4).timeout(
        _bound,
        onTimeout: () => fail(
          'ordinary pool work no longer runs while '
          '$width awaited callback(s) are parked on dead isolates — a '
          'suspended call_async must hold no pool worker. Check whether '
          'call_parked_async regressed to the blocking call.',
        ),
      ),
      30,
    );
  });
}

/// Context for a failure message: a child isolate that died on an unhandled
/// error explains a timeout far better than the timeout does.
String _ctx(List<Object?> errors) =>
    errors.isEmpty ? '' : ' (child isolate error: ${errors.first})';

/// A future completing with the first [n] messages [port] delivers. Listening
/// starts now, so nothing sent before the await is missed.
Future<List<Object?>> _collect(ReceivePort port, int n) {
  final done = Completer<List<Object?>>();
  final seen = <Object?>[];
  port.listen((message) {
    seen.add(message);
    if (seen.length == n && !done.isCompleted) done.complete(seen);
  });
  return done.future;
}

/// Registers a closure that reports it is running and then never answers,
/// parks it where another isolate can invoke it, and waits to be killed.
///
/// Copied rather than shared with `callback_liveness_test.dart` (a test file's
/// private members are not importable), and unchanged from it: it cannot
/// simply return, because the registration pins it, and the
/// closure cannot await, because it is synchronous by contract — so "never
/// answer" means "never return".
Future<void> _parkAndNeverAnswer(List<SendPort> ports) async {
  final reply = ports[0];
  final invoked = ports[1];
  await initBridge();
  final slot = await parkFunction(
    f: (v) {
      invoked.send('invoked');
      // `sleep`, not a spin: one of these runs per pool worker in the last test,
      // and saturating every core with busy-waiting isolates starves the parent
      // badly enough that its own timeouts stop firing — a test that cannot
      // report its own failure. Blocking in short sleeps costs no CPU and still
      // honours `Isolate.immediate`, taken at the loop's back edge.
      while (true) {
        sleep(const Duration(milliseconds: 5));
      }
    },
  );
  reply.send(slot);
  await Completer<void>().future;
}

/// Awaits one parked closure through the bridged `async fn`, reports what came
/// back, and then falls off the end of its entry.
///
/// Nothing keeps it alive past that point and its exit listener fires. If the
/// bridge call never completes, `_pending` never empties and
/// `keepIsolateAlive` holds this isolate here for the session — what test 1's
/// last expectation measures.
Future<void> _callParkedAsyncAndReport(List<Object?> args) async {
  final report = args[0]! as SendPort;
  final slot = args[1]! as int;
  await initBridge();
  try {
    report.send('returned:${await callParkedAsync(slot: slot, value: 1)}');
  } catch (e) {
    report.send(e.runtimeType.toString());
  }
}

/// Awaits every parked closure at once. Each call is guarded as it is created:
/// post-fix they all settle by throwing, and an errored future with no handler
/// is an unhandled async error, which (`errorsAreFatal` being the default)
/// would kill this isolate mid-test.
Future<void> _callAllParkedAsync(List<Object?> args) async {
  final slots = (args[0]! as List).cast<int>();
  await initBridge();
  final calls = [
    for (final slot in slots) _guard(callParkedAsync(slot: slot, value: 1)),
  ];
  await Future.wait(calls);
}

Future<int> _guard(Future<int> call) async {
  try {
    return await call;
  } catch (_) {
    return -1;
  }
}
