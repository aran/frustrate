/// Synchronous access to a `locked` handle under `on_contention = "error"`,
/// on **every** target — VM here, and both web builds through `web_test`.
///
/// This is the one synchronous path on the web surface that touches state a
/// worker can be holding. It is legal because the acquisition is a
/// compare-exchange and the release reaches no wait instruction: frustrate
/// owns the lock (`runtime/rust/src/rwlock.rs`), and its waiter list is a spin
/// lock on threaded wasm. A wrong answer there is not a failed expectation but
/// a trapped page, which is why the contended cases below matter more than the
/// uncontended ones.
///
/// The blocking contract beside it is still native-only and lives in
/// native_blocking_test.dart: `block` waits to *acquire*, by declaration, and
/// no argument about the release changes that.
library;

import 'package:frustrate/frustrate.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

void main() {
  setUpAll(initBridge);

  group('uncontended', () {
    test('a sync read answers in the caller\'s frame', () async {
      final c = Counter.new_();
      await c.add(delta: 7);
      expect(c.tryGet(), 7);
      c.dispose();
    });

    test(
      'a sync write takes the write guard and the mutation stands',
      () async {
        final c = Counter.new_();
        expect(c.tryBump(by: 5), 5);
        expect(c.tryBump(by: 3), 8, reason: 'the second call sees the first');
        expect(await c.get(), 8, reason: 'and so does the dispatched sibling');
        c.dispose();
      },
    );

    test('a contract-marked member on a type with two representations', () {
      // `Note` crosses as both a value class and a locked handle class. The
      // handle half's try-lock member is on every surface now, beside the
      // value half's ordinary sync member — so nothing about a dual type is
      // per-platform any more.
      final h = NoteHandle.open(title: 't', body: 'a b c');
      expect(h.tryWordCount(), 3);
      expect(noteTitle(n: h), 't');
      h.dispose();
    });

    test(
      'the multi-lock plan\'s try-lock arm, at a fixed and a runtime count',
      () {
        final a = Vault.new_(balance: 3);
        final b = Vault.new_(balance: 4);
        expect(sumVaultsTry(a: a, b: b), 7);
        expect(vaultsTotalTry(vs: []), 0);
        expect(
          vaultsTotalTry(vs: [Vault.new_(balance: 3), Vault.new_(balance: 4)]),
          7,
        );
      },
    );

    test('the try-lock crosses a tagged &dyn param', () async {
      // The guard is taken per impl tag off the trait's own carrier, which is
      // a different arm of the emitter from the concrete cases above — and it
      // is now compiled into the wasm module like the rest.
      final counting = CountingStore.fresh();
      expect(storeSizeNow(s: counting), 0);
      final store = await openStore(kind: 'mem');
      await store.put(key: 'k', value: 'v');
      expect(store.size(), 1, reason: 'the trait\'s own sync member');
    });
  });

  group('contended', () {
    // The holder is `holdWriteAsking`: a Rust `async fn` that takes the write
    // guard and then awaits a Dart callback, so the callback body runs with
    // the object provably locked and the holding task parked as heap data.
    // No sleep and no polling, which is what lets one body cover every config
    // — on threaded web the guard is held on a pool worker while the main
    // thread try-reads, and on single-threaded web it is held by a task on the
    // microtask executor.
    //
    // It is also the Rust -> Dart -> Rust shape: the refusing call is issued
    // from inside the holder's own call chain. Waiting there would deadlock by
    // construction, which is the whole of why `error` exists.

    test('a sync read refuses while an async fn holds the write guard', () {
      final c = Counter.new_();
      ContentionException? seen;
      var closureRan = false;
      final result = c.holdWriteAsking(
        f: (v) {
          closureRan = true;
          try {
            c.tryGet();
          } on ContentionException catch (e) {
            seen = e;
          }
          return 1;
        },
      );
      return result.then((v) {
        expect(closureRan, isTrue, reason: 'the closure must run mid-hold');
        expect(
          seen,
          isNotNull,
          reason: 'a sync read must refuse while the guard is held',
        );
        expect(seen!.message, contains('Counter::try_get'));
        expect(seen!.message, contains('on_contention'));
        expect(v, 1, reason: 'the holder still completed normally');
        // And the release handed the lock back: the same read now answers.
        expect(c.tryGet(), 1);
        c.dispose();
      });
    });

    test('the main thread releases a guard with a waiter queued behind it', () async {
      // The case the whole change turns on, and the one the refusals above do
      // NOT reach: a refused try-lock is one compare-exchange and touches no
      // waiter list. Here the main thread *succeeds*, a dispatched writer
      // queues behind it, and the release therefore has to take the waiter
      // list, grant the writer and wake it — through the executor and onto the
      // pool queue — all on the browser main thread. If any link of that chain
      // could wait, threaded wasm traps here with
      // "Atomics.wait cannot be called in this context" rather than failing an
      // expectation.
      //
      // `LockProbe` owns its lock so it can *say* whether a waiter arrived,
      // which is what stops this passing vacuously: on a build with real
      // workers the assertion below is what fails if the interleaving stops
      // happening.
      final probe = LockProbe.new_();
      final parallel = Frustrate.instance.asyncIsParallel;
      var observed = false;
      // A round either sees its writer queue behind the guard, or learns the
      // writer already ran and tries again. With real workers the writer that
      // has not run yet must queue, so no round depends on how fast it is
      // scheduled. Without them nothing can queue, and the rounds are bounded.
      for (var round = 0; !observed && (parallel || round < 40); round++) {
        final before = probe.value();
        final pending = probe.bump();
        observed = probe.readUntilContended(
          unchangedFrom: before,
          maxSpins: parallel ? 1000000000000 : 2000000,
        );
        await pending;
      }
      if (parallel) {
        expect(
          observed,
          isTrue,
          reason:
              'with real workers a dispatched writer must queue behind '
              'the main thread\'s read guard',
        );
      } else {
        // Single-threaded web and the VM's own single-isolate case: a
        // dispatched call cannot start while a synchronous member is on the
        // stack, so nothing can be queued when it releases. That is not a hole
        // — with one thread there is no release-with-a-waiter to reach.
        expect(
          observed,
          isFalse,
          reason:
              'nothing can run between a sync acquire and its release '
              'here, so no waiter can exist to observe',
        );
      }
      expect(probe.value(), greaterThan(0), reason: 'every bump landed');
    });

    test('the same race through the generated locked glue', () async {
      // `LockProbe` above proves the release path; this drives it through the
      // emitted glue instead — `locked_ref` + `try_read`, with the guard
      // dropping at the end of the glue fn — while a dispatched writer races a
      // main-thread reader on one object. The body cannot see the lock, so
      // this asserts only what it can: whichever way a round interleaves, the
      // call either answers or refuses, never traps, and every queued write
      // lands.
      final c = Counter.new_();
      var refusals = 0;
      for (var round = 0; round < 40; round++) {
        final pending = c.add(delta: 1);
        try {
          c.holdReadSpinning(iters: 20000);
        } on ContentionException {
          // The writer got there first, which is the other legal order.
          refusals++;
        }
        await pending;
      }
      expect(c.tryGet(), 40, reason: 'every queued write was granted and ran');
      expect(refusals, lessThanOrEqualTo(40));
      c.dispose();
    });

    test(
      'a sync write refuses on the same guard, and lands once it is free',
      () {
        final c = Counter.new_();
        ContentionException? seen;
        final result = c.holdWriteAsking(
          f: (v) {
            try {
              c.tryBump(by: 100);
            } on ContentionException catch (e) {
              seen = e;
            }
            return 2;
          },
        );
        return result.then((v) {
          expect(seen, isNotNull, reason: 'a sync write must refuse too');
          expect(seen!.message, contains('Counter::try_bump'));
          expect(v, 2);
          expect(
            c.tryBump(by: 100),
            102,
            reason: 'the write lands after the hold',
          );
          c.dispose();
        });
      },
    );
  });
}
