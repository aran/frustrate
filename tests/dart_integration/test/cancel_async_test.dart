/// `FrustrateCancelToken`: cancelling an in-flight bridged `async fn`.
///
/// The load-bearing assertion is not that the Dart future rejects — a wrapper
/// around `Future.any` could do that — but that the **Rust future was
/// dropped**, which is what cancellation is here. `awaits_until_cancelled`
/// holds a drop sensor across its suspension point, so
/// `cancelledFutureWasDropped()` distinguishes a real cancel from a runtime
/// that merely stopped listening and leaked the task.
///
/// The drop lands on a later drain — a pool worker on native and threaded web,
/// the `schedule_drain` microtask on single-threaded web — never on the
/// canceller's thread, so the sensor is polled rather than read once.
///
/// Runs on every fixture: a Rust `async fn` is on the cooperative executor on
/// all three, from the same source.
@Timeout(Duration(minutes: 2))
library;

import 'dart:async';

import 'package:frustrate/frustrate.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

/// Wait for the Rust future's drop, which happens on a drain this thread does
/// not run. Bounded, and reported as a failure rather than a hang.
Future<void> awaitDropped() async {
  final deadline = DateTime.now().add(const Duration(seconds: 10));
  while (!cancelledFutureWasDropped() && DateTime.now().isBefore(deadline)) {
    await Future<void>.delayed(const Duration(milliseconds: 5));
  }
  expect(
    cancelledFutureWasDropped(),
    isTrue,
    reason:
        'a claimed cancel must drop the Rust future — the task is still '
        'in the executor registry, which is a leak, not a cancel',
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

  test('cancelling a suspended async fn rejects the future and drops it', () async {
    final token = FrustrateCancelToken();
    final call = awaitsUntilCancelled(cancel: token);
    // Give the call a turn to reach Rust and park. Not required for the claim
    // — the id is registered synchronously — but **required for the
    // assertion**, and that is worth stating rather than reading as politeness:
    // `awaits_until_cancelled` constructs its `DropSensor` in the body, so the
    // sensor does not exist until the task has been polled once. Cancel before
    // the executor reaches it and the future is dropped exactly as it should be
    // with nothing inside it to report the drop — a false negative, not a leak.
    // (The window is widest on threaded web, where the first drain waits on the
    // pool's workers booting. A `Deferred` actor method can do better: its
    // sensor is built in the prefix — see cancel_deferred_test.dart.)
    await Future<void>.delayed(const Duration(milliseconds: 20));
    expect(
      cancelledFutureWasDropped(),
      isFalse,
      reason: 'the future must still be alive before the cancel',
    );

    token.cancel();
    expect(token.isCancelled, isTrue);

    await expectLater(call, throwsA(isA<CancelledCallException>()));
    await awaitDropped();
  });

  test('a cancelled token refuses a later call before it is issued', () async {
    final token = FrustrateCancelToken()..cancel();
    // Nothing reaches Rust: the refusal happens ahead of encoding, so this is
    // also what keeps a cancelled scope from opening channels it would then
    // have to retire.
    await expectLater(
      awaitsUntilCancelled(cancel: token),
      throwsA(isA<CancelledCallException>()),
    );
    expect(
      cancelledFutureWasDropped(),
      isFalse,
      reason: 'no call was issued, so no future was ever created to drop',
    );
  });

  test('one token cancels every call bound to it', () async {
    final token = FrustrateCancelToken();
    final errors = <Object>[];
    // Handlers attached as each future is created, not after the cancel.
    // Cancelling rejects all three at once, and a rejection nobody is
    // listening to yet is an unhandled async error — ordinary Dart, and the
    // reason the token's doc says to attach first. (Awaiting them one at a
    // time *after* cancelling is exactly the shape that trips it: the first
    // await yields, and the other two sit unheard across that turn.)
    final settled = [
      for (var i = 0; i < 3; i++)
        awaitsUntilCancelled(cancel: token).then<void>(
          (_) => fail('a cancelled call must not answer'),
          onError: errors.add,
        ),
    ];
    await Future<void>.delayed(const Duration(milliseconds: 20));
    token.cancel();
    await Future.wait(settled);

    expect(errors, hasLength(3));
    expect(errors, everyElement(isA<CancelledCallException>()));
    await awaitDropped();
  });

  test('cancelling after the answer leaves the answer alone', () async {
    final token = FrustrateCancelToken();
    // Awaited first, so the call has certainly answered before the cancel.
    expect(await answersAtOnce(x: 41, cancel: token), 42);
    token.cancel();
    // The claim was refused, so nothing was failed — and a token that already
    // released this call has nothing to cancel. The proof it did not
    // double-settle is that this test does not die with "Future already
    // completed" in the zone.
    expect(token.isCancelled, isTrue);
  });

  test('a call with no token behaves exactly as before', () async {
    expect(await answersAtOnce(x: 1), 2);
  });
}
