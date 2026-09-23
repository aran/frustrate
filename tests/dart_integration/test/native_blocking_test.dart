/// Native-only surface: members web cannot run — `on_contention = "block"`,
/// which waits to acquire (FR0008), and the DartFunction-taking members, whose
/// returning callback parks the invoking worker. This file is VM-only: it
/// references members the web variant deliberately omits and would not compile
/// for a browser platform. The absences are themselves pinned in codegen, by
/// emit_dart.rs's `web_surface_omits_the_blocking_contract_and_keeps_the_try_lock`.
///
/// The **try-lock** contract is not here: it neither waits to acquire nor
/// reaches a wait instruction on release, so it runs on every target and its
/// coverage is locked_sync_test.dart. What stays is the contention race a real
/// pool makes possible — one thread holding while another asks — which is a
/// different question from whether the main thread may ask at all.
@TestOn('vm')
library;

import 'dart:typed_data';

import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart';

void main() {
  setUpAll(initBridge);

  test('blocking contract: uncontended sync read succeeds', () async {
    final c = Counter.new_();
    await c.add(delta: 7);
    expect(c.blockingGet(), 7);
  });

  test('web-opt-in member behaves exactly like the un-opted one on native', () async {
    // `blockingRead` carries #[bridge(web = "runtime_fail")] — present on the
    // web surface as a throwing stub — but on native it is an ordinary blocking
    // read, identical to `blockingGet`. The opt-in changes nothing here.
    final c = Counter.new_();
    await c.add(delta: 9);
    expect(c.blockingRead(), 9);
  });

  test('blocking contract: waits out contention instead of throwing', () async {
    final c = Counter.new_();
    await c.add(delta: 1);
    // Occupy the write lock on a pool thread for a bounded window.
    final hold = c.holdWrite(millis: 200);
    // Wait until the lock is observably held (the error contract throws).
    var contended = false;
    final deadline = DateTime.now().add(const Duration(seconds: 2));
    while (!contended && DateTime.now().isBefore(deadline)) {
      try {
        c.tryGet();
        await Future<void>.delayed(const Duration(milliseconds: 5));
      } on ContentionException {
        contended = true;
      }
    }
    expect(
      contended,
      isTrue,
      reason: 'expected to observe contention while holdWrite held the lock',
    );
    // The block contract: same contention, but the call waits for the
    // holder to release and then succeeds — it never throws.
    expect(c.blockingGet(), 1);
    await hold;
  });

  test('contention throws ContentionException naming the contract', () async {
    // A *running* holder rather than a suspended one: `holdWrite` sleeps on a
    // pool thread with the guard, which needs real threads and so cannot be
    // written portably. The suspended-holder case is the portable one and
    // lives in locked_sync_test.dart; both must refuse, and they are different
    // states.
    final c = Counter.new_();
    // Occupy the write lock on a pool thread for a bounded window.
    final hold = c.holdWrite(millis: 500);
    // Poll until we observe contention (bounded by the hold duration).
    ContentionException? seen;
    final deadline = DateTime.now().add(const Duration(seconds: 2));
    while (seen == null && DateTime.now().isBefore(deadline)) {
      try {
        c.tryGet();
        await Future<void>.delayed(const Duration(milliseconds: 5));
      } on ContentionException catch (e) {
        seen = e;
      }
    }
    await hold;
    expect(
      seen,
      isNotNull,
      reason: 'expected to observe contention while holdWrite held the lock',
    );
    expect(seen!.message, contains('Counter::try_get'));
    expect(seen.message, contains('on_contention'));
    // After the hold completes, sync reads succeed again.
    expect(c.tryGet(), 0);
  });

  test('locked trait: contention throws ContentionException', () async {
    final store = await openStore(kind: 'mem');
    final hold = store.compact(millis: 500);
    ContentionException? seen;
    final deadline = DateTime.now().add(const Duration(seconds: 2));
    while (seen == null && DateTime.now().isBefore(deadline)) {
      try {
        store.size();
        await Future<void>.delayed(const Duration(milliseconds: 5));
      } on ContentionException catch (e) {
        seen = e;
      }
    }
    await hold;
    expect(
      seen,
      isNotNull,
      reason: 'expected contention while compact held the write lock',
    );
    expect(seen!.message, contains('Store::size'));
    expect(store.size(), 0);
  });

  test(
    'returning callback: the pool worker blocks, this thread answers',
    () async {
      final args = <int>[];
      final sum = await transformSum(
        values: Int64List.fromList([1, 2, 3]),
        f: (v) {
          args.add(v);
          return v * 10;
        },
      );
      expect(sum, 60);
      expect(args, [1, 2, 3], reason: 'invoked per item, in order');
    },
  );

  test(
    'a throwing closure surfaces as the call\'s panic, attributably',
    () async {
      await expectLater(
        transformSum(
          values: Int64List.fromList([1]),
          f: (v) => throw StateError('mapper broke'),
        ),
        throwsA(
          isA<BridgePanicException>().having(
            (e) => e.message,
            'message',
            allOf(contains('callback threw'), contains('mapper broke')),
          ),
        ),
      );
      // The pool survives: the panic was an unwind in the worker, not a
      // crash — and later calls still work.
      expect(
        await transformSum(values: Int64List.fromList([2]), f: (v) => v),
        2,
      );
    },
  );

  test('returning callback from an actor executor', () async {
    final m = await Miner.new_(label: 'refiner');
    expect(await m.refine(x: 20, f: (v) => v * 2), 41);
    await m.dispose();
  });

  // The blocking twin of bridge_test.dart's fallible-callback group. `call`
  // and `call_async` share one `decode_response` in the runtime, and this is
  // what proves the shared decode really is shared: a declared refusal is an
  // `Err` the Rust body handles on the blocking path too, while an undeclared
  // throw still unwinds the parked worker into this call's panic.
  //
  // Native-only by derivation, exactly like `transformSum`: `ask_dart_blocking`
  // is not a Rust `async fn`, so it cannot `.await` and must park a pool
  // worker while this thread runs the closure.
  test('fallible callback on the BLOCKING path: a refusal is a value', () async {
    // ask_dart_blocking sums per item: Ok(n) -> n, Busy{ms} -> -ms,
    // NotAllowed -> -1. Three items, one of each, so all three arms run in one
    // call and the sum can only come out right if each was decoded correctly.
    final sum = await askDartBlocking(
      values: Int64List.fromList([1, 2, 3]),
      f: (v) {
        if (v == 2) {
          throw RefusalErrorException(const RefusalErrorBusy(retryInMs: 50));
        }
        if (v == 3) {
          throw RefusalErrorException(const RefusalErrorNotAllowed());
        }
        return v * 10;
      },
    );
    expect(sum, 10 - 50 - 1);
  });

  test(
    'fallible callback on the BLOCKING path: an undeclared throw is loud',
    () async {
      await expectLater(
        askDartBlocking(
          values: Int64List.fromList([1]),
          f: (v) => throw StateError('a bug, not a refusal'),
        ),
        throwsA(
          isA<BridgePanicException>().having(
            (e) => e.message,
            'message',
            allOf(contains('callback threw'), contains('a bug, not a refusal')),
          ),
        ),
      );
      // The pool survives it.
      expect(
        await askDartBlocking(values: Int64List.fromList([4]), f: (v) => v),
        4,
      );
    },
  );

  // Rust `async fn` is no longer native-only: it runs on the cooperative
  // executor on every platform, so its coverage (including the async fn
  // awaiting a self-driving future, and 1000 concurrent multiplexed calls)
  // lives in bridge_test.dart, which runs on native VM and web.
}
