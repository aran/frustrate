/// `Deferred<T>`: an actor method that hands off its completion.
///
/// The headline pair is order-based and deterministic — no clocks:
///
///  - **The fence**: while `deferredWait`'s completion is outstanding, a
///    later method completes, and a later `release()` — a message on the
///    *same actor* — is what lets the deferred completion resolve at all. If
///    a deferred method held the instance the way a plain one does, `release`
///    could never run and the await below would time the suite out.
///  - **The positive control**: the same shape through a *plain* method
///    (`nthPrime`) must wedge — the later call's completion proves the
///    earlier one already finished — an actor is serial, so a 5-second dial
///    makes `ticket()` wait 5 seconds. Pinned as the contract it still is
///    for non-deferred methods. If this control ever
///    fails, the fence above proves nothing.
///
/// Runs on every fixture. On web this suite is the first thing that ever
/// makes an actor's worker instance post a completion *outside* its dispatch
/// turn (the pump's microtask drain) — a path that did not exist before
/// deferred methods, and that only a browser run exercises.
@Timeout(Duration(minutes: 2))
library;

import 'dart:async';

import 'package:frustrate/frustrate.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

void main() {
  setUpAll(initBridge);

  tearDown(() {
    expect(
      Frustrate.instance.openChannelCount,
      0,
      reason:
          'a disposed actor must leave no channel registered. Open: '
          '${Frustrate.instance.openChannelLabels}',
    );
  });

  test(
    'a deferred method releases the instance while its completion waits',
    () async {
      final miner = await Miner.new_(label: 'deferred');
      var waitDone = false;
      final wait = miner.deferredWait(token: 5)
        ..whenComplete(() => waitDone = true).ignore();

      // A later call completes while the deferred completion is outstanding —
      // the instance is not held. Deterministic: the gate is closed, so `wait`
      // CANNOT have completed, whatever the scheduling.
      expect(await miner.label(), 'deferred');
      expect(
        waitDone,
        isFalse,
        reason:
            'the deferred completion must still be outstanding — label() '
            'completing first IS the released instance',
      );

      // And the releasing call is itself a message on this same actor: the
      // only way it can run is that deferredWait is not holding the executor.
      await miner.release(value: 100);
      expect(
        await wait,
        105,
        reason:
            'gate value + token: this await got THIS '
            'call\'s answer — the correlation apps did by hand with events',
      );
      await miner.dispose();
    },
  );

  test(
    'positive control: the same shape through a plain method wedges',
    () async {
      // Actor serialization, pinned as the still-standing contract for
      // non-deferred methods. `nthPrime` is submitted first, so FIFO means
      // label()'s completion proves nthPrime's already happened. If this fails,
      // the fence above proves nothing — serialization is what Deferred opts
      // out of, so it must be observable when nothing opts out.
      final miner = await Miner.new_(label: 'wedge');
      var primeDone = false;
      final prime = miner.nthPrime(n: 500)
        ..whenComplete(() => primeDone = true).ignore();
      expect(await miner.label(), 'wedge');
      expect(
        primeDone,
        isTrue,
        reason:
            'a plain method serializes: nothing after it may complete '
            'first. (If this ever fails, the deferred fence is meaningless.)',
      );
      expect(await prime, greaterThan(0));
      await miner.dispose();
    },
  );

  test(
    'two deferred completions ride out of order, each to its own caller',
    () async {
      final miner = await Miner.new_(label: 'pair');
      // Two in flight at once — impossible under one-at-a-time completion —
      // and `release` queues behind both prefixes (FIFO), so no sleeps needed.
      final first = miner.deferredWait(token: 1);
      final second = miner.deferredWait(token: 2);
      await miner.release(value: 10);
      expect(await first, 11);
      expect(await second, 12);
      await miner.dispose();
    },
  );

  test('a typed error crosses the deferred completion path', () async {
    final miner = await Miner.new_(label: 'typed');
    await expectLater(
      miner.deferredWithdraw(amount: 200),
      throwsA(isA<WithdrawErrorException>()),
    );
    // The decoder was consumed, not leaked, and the actor still works.
    expect(await miner.deferredWithdraw(amount: 30), 70);
    await miner.dispose();
  });

  test(
    'dispose cancels an outstanding deferred completion with a named error',
    () async {
      final miner = await Miner.new_(label: 'doomed');
      final wait = miner.deferredWait(token: 9);
      // Listener attached BEFORE dispose: the cancellation fails `wait` during
      // dispose(), and an error landing on a listenerless future is an
      // unhandled async error — which is a test-harness fact, not the contract.
      final cancelled = expectLater(
        wait,
        throwsA(
          isA<StateError>().having(
            (e) => e.message,
            'message',
            allOf(contains('Miner'), contains('deferred call outstanding')),
          ),
        ),
      );
      // The gate never opens: without the dispose contract this future would
      // hang forever. dispose() must cancel it — never drain it (draining
      // would hold dispose for as long as the slow work takes, which is the
      // wedge Deferred exists to remove).
      await miner.dispose();
      await cancelled;
    },
  );

  test(
    'a completion that already answered is not cancelled by dispose',
    () async {
      final miner = await Miner.new_(label: 'answered');
      final wait = miner.deferredWait(token: 3);
      await miner.release(value: 40);
      expect(await wait, 43);
      // Disposing after the answer must not disturb anything — the claim
      // protocol refuses an answered call (native), and web has nothing left
      // pending. This is the other half of exactly-once.
      await miner.dispose();
    },
  );

  test('a panic in the prefix still answers the call', () async {
    final miner = await Miner.new_(label: 'flawed-prefix');
    await expectLater(
      miner.deferredFlawed(inPrefix: true),
      throwsA(isA<BridgePanicException>()),
    );
    await miner.dispose();
  });

  test(
    'a panic inside the deferred future is attributed, not a hung future',
    () async {
      // Native: the executor's poll catch turns it into a panic envelope. Web:
      // the trap escapes the pump's *microtask* drain — not a dispatch frame —
      // where an earlier glue swallowed it (`catch (_) {}`); the pump now
      // attributes it through frustrate_current_drain_call. A dedicated
      // instance, because under panic=abort the web instance is degraded
      // afterwards.
      final miner = await Miner.new_(label: 'flawed-future');
      await expectLater(
        miner.deferredFlawed(inPrefix: false),
        throwsA(isA<BridgePanicException>()),
      );
      // Best-effort teardown: after a wasm trap the instance may refuse the
      // drop call (loudly). The executor is released either way.
      try {
        await miner.dispose();
      } on Object {
        // The drop call trapping on a degraded instance is acceptable here;
        // dispose's finally has already released the host.
      }
    },
  );
}
