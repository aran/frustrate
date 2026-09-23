/// What a crash reporter is handed when a bridge call fails and nobody caught
/// it.
///
/// There is no Rust-side error listener: an uncaught failure already reaches
/// whatever the app installed — `runZonedGuarded`, or Flutter's
/// `PlatformDispatcher.onError`.
///
/// Reaching the reporter is not the same as being reportable. A **sync** call
/// throws in the caller's own frame, so its stack names the member and the
/// app's call site. An **async** call is completed from a port callback (on
/// web, from the post import) that has no relationship to the caller, and
/// `completeError` is given no `StackTrace` — so without the generated
/// bindings catching and restacking, every async failure arrives with
/// `StackTrace.empty`: zero frames, and two different members failing are
/// indistinguishable to a reporter that groups by frames.
///
/// So each row below asserts what is *in* the stack, not merely that the error
/// arrived. The rows are the async routes a call can take — the transport, an
/// actor method, a deferred actor method, an actor constructor — because each
/// completes by a different path and each had to be fixed separately.
///
/// **The decorator row is the one that could regress silently.** A
/// `DelegatingRuntime` can capture the stack at *issue* and supply a strictly
/// better one, naming the app's call site as well as the member. The binding's
/// restack is guarded on `identical(st, StackTrace.empty)` precisely so it
/// never overwrites that — and without the row below, a guard that stopped
/// working would make installing a reporter worse than not installing one, and
/// nothing would say so.
///
/// The last two rows are `testOn: 'vm'` because they raise Rust panics, and a
/// web panic traps under `panic=abort` and leaves the page unusable
/// (trap_attribution_test.dart) — the reason panic_listener_test.dart is its
/// own file with the panic last.
@Timeout(Duration(minutes: 2))
library;

import 'dart:async';

import 'package:frustrate/frustrate.dart';
import 'package:frustrate/intercept.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

/// This file's own name, as it appears in a frame. The app's call site, for the
/// rows that assert whether a reporter can see one.
const _callSite = 'uncaught_error_test.dart';

/// Run [body] in a guarded zone and answer what the zone's handler was given.
///
/// Polled rather than awaited one turn: an uncaught *async* failure is reported
/// to the zone only once the future has settled with nobody listening, which is
/// a later microtask. Bounded, and reported as a missing report rather than a
/// hang.
Future<(Object, StackTrace)> uncaught(void Function() body) async {
  final seen = <(Object, StackTrace)>[];
  runZonedGuarded(body, (e, st) => seen.add((e, st)));
  final deadline = DateTime.now().add(const Duration(seconds: 10));
  while (seen.isEmpty && DateTime.now().isBefore(deadline)) {
    await Future<void>.delayed(const Duration(milliseconds: 5));
  }
  expect(
    seen,
    hasLength(1),
    reason:
        'the failure never reached the zone guard, so an app relying on '
        'PlatformDispatcher.onError would never hear about it',
  );
  return seen.single;
}

/// The recipe from package:frustrate/intercept.dart: capture where the call was
/// issued, and give a stackless failure that stack instead of the one-frame one
/// the binding would supply.
final class _Reporting extends DelegatingRuntime {
  _Reporting(super.inner);

  @override
  Future<BinaryReader> aroundAsync(
    int fnId,
    Future<BinaryReader> Function() next, {
    bool deferred = false,
    FrustrateCancelToken? cancel,
    ActorHost? host,
  }) async {
    final issued = StackTrace.current;
    try {
      return await next();
    } catch (e) {
      Error.throwWithStackTrace(e, issued);
    }
  }
}

void main() {
  setUpAll(initBridge);

  test('a sync failure already names its member and the call site', () async {
    // The control. Nothing was changed to make this true — a synchronous throw
    // carries the frames it was thrown from — and it is the bar the async rows
    // are measured against.
    final (error, stack) = await uncaught(() {
      parseNumber(s: '');
    });
    expect(error, isA<BridgeException>());
    expect('$stack', contains('parseNumber'));
    expect('$stack', contains(_callSite));
  });

  test('an async failure names its member', () async {
    final (error, stack) = await uncaught(() {
      parseNumberAsync(s: '');
    });
    expect(error, isA<BridgeException>());
    expect(
      '$stack',
      isNot(isEmpty),
      reason:
          'an async failure reached the reporter with no frames at all, '
          'so every async member failing looks the same to it',
    );
    expect('$stack', contains('parseNumberAsync'));
  });

  test('an actor method failure names its member', () async {
    final miner = await Miner.new_(label: 'reporter');
    final (error, stack) = await uncaught(() {
      miner.checkedDiv(a: 1, b: 0);
    });
    await miner.dispose();
    expect(error, isA<BridgeException>());
    expect('$stack', contains('Miner.checkedDiv'));
  });

  test('a deferred actor failure names its member', () async {
    // A different completion path again: the answer is produced by a detached
    // future on the cooperative executor, outside any dispatch turn.
    final miner = await Miner.new_(label: 'reporter');
    final (error, stack) = await uncaught(() {
      miner.deferredWithdraw(amount: 1 << 40);
    });
    await miner.dispose();
    expect(error, isA<WithdrawErrorException>());
    expect('$stack', contains('Miner.deferredWithdraw'));
  });

  test('a decorator that captured the issuing stack keeps it', () async {
    // The guard's whole job. Without it the binding would replace this richer
    // stack with its own one-frame one, and installing a reporter would make
    // reports worse rather than better.
    Frustrate.activate(_Reporting(Frustrate.instance));
    try {
      final (_, stack) = await uncaught(() {
        parseNumberAsync(s: '');
      });
      expect(
        '$stack',
        contains('parseNumberAsync'),
        reason: 'the member is still named',
      );
      expect(
        '$stack',
        contains(_callSite),
        reason:
            "the decorator's issuing stack was overwritten by the "
            "binding's, losing the app call site that is the reason to "
            'install one',
      );
    } finally {
      Frustrate.reset();
    }
  });

  test('a failing actor constructor names itself', () async {
    // VM-only: the only constructor in the fixture that fails does it by
    // panicking, and a web panic traps. The route is still worth a row — a
    // constructor is emitted by its own path, with its own handler (the one
    // that tears down the executor it just spawned).
    final (error, stack) = await uncaught(() {
      Miner.flawed(label: 'boom');
    });
    expect(error, isA<BridgePanicException>());
    expect('$stack', contains('Miner.flawed'));
  }, testOn: 'vm');

  test('an uncaught Rust panic names the member it came out of', () async {
    // A panic is not an `Err`, and `frustrate::panic::register` is what
    // carries its location and backtrace on the Rust side. What this row
    // holds is the other half: the Dart-side report is attributable too,
    // rather than a bare message.
    final (error, stack) = await uncaught(() {
      poolPanic();
    });
    expect(error, isA<BridgePanicException>());
    expect('$stack', contains('poolPanic'));
  }, testOn: 'vm');
}
