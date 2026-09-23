/// `#[bridge(resident)]` end to end: a `!Send` Rust object reached
/// synchronously, reclaimed by the isolate that built it, and reported when it
/// cannot be.
///
/// `Scene` holds `Rc<RefCell<Vec<String>>>` with a second clone beside it, so
/// it is genuinely thread-affine rather than merely un-annotated — a build that
/// lost that would stop testing anything. `handle::confined_new` refuses this
/// type outright (its doctest says so), which is the whole reason the model
/// exists.
///
/// **What is deliberately not asserted here.** Whether a *wrong-thread* touch
/// is refused is not a Dart-observable fact on demand: a Dart isolate may run
/// each event-loop turn on a different OS thread, but nothing makes it, so a
/// test that waits for a hop can only ever say "not this time". The refusal is
/// pinned deterministically in Rust instead — `frustrate::resident`'s
/// `a_second_thread_is_refused` and `a_wrong_thread_reclaim_leaks_instead_of_dropping`
/// drive it with a real second thread. What this file adds is the half those
/// cannot reach: the transport wiring, the `dart:core` `Finalizer`, and the
/// exit notice.
///
/// Every positive assertion is made **within one event-loop turn** — mint,
/// touch, dispose, with no `await` between — which is not luck: one Dart
/// message is handled on one thread, so the object cannot move underneath a
/// synchronous block.
///
/// VM-only. The registry that answers [residentLeakReport] is native-only
/// because web has neither isolates nor threads to migrate between (see
/// `runtime/rust/src/resident.rs`), so there is nothing there to report.
///
/// Two of these tests wait on the VM rather than on the bridge — a finalizer
/// callback and an exit notice — and each carries its own `timeout:` for the
/// reason `gc_finalizer_test.dart`'s do. Under a loaded `bazel test //...`
/// that wait competes with every other suite for the machine, and the
/// harness's 30-second default turns a slow pass into a timeout with nothing
/// to read. **Per test, not `@Timeout` on the library**: this suite runs the
/// compiled dill directly, and a library annotation does not reach the
/// declarer there — measured, by watching the 30-second default fire with one
/// in place. Each wait bounds itself well inside its ceiling, so a genuine
/// regression arrives as a failed expectation naming what it wanted.
@TestOn('vm')
library;

import 'dart:async';
import 'dart:isolate';

import 'package:frustrate/frustrate.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart';

/// Turn the event loop, with allocation pressure, until [done] or [limit].
///
/// Shaped after `gc_finalizer_test.dart`'s pressure loop, and for its reasons:
/// short-lived multi-page allocations promote a scavenge, and the yield is what
/// lets a finalizer callback — delivered on the message loop — ever run. The
/// running sum keeps the optimizer from eliding the allocations.
Future<bool> _turnUntil(
  bool Function() done, {
  Duration limit = const Duration(seconds: 20),
  bool pressure = true,
}) async {
  final sw = Stopwatch()..start();
  var sink = 0;
  while (sw.elapsed < limit) {
    if (done()) return true;
    if (pressure) {
      for (var i = 0; i < 32; i++) {
        final junk = List<int>.filled(1 << 14, i);
        sink += junk[junk.length - 1];
      }
      expect(sink, isNonZero, reason: 'the pressure loop must not be elided');
      await Future<void>.delayed(Duration.zero);
    } else {
      // Sleep rather than spin. What this waits for — an isolate's exit notice
      // — is delivered on a *Rust* thread through a native port, so a tight
      // `Duration.zero` loop here does not hurry it along; it only takes a
      // core away from the thread that would.
      await Future<void>.delayed(const Duration(milliseconds: 2));
    }
  }
  return done();
}

/// The count for one type in the report, or 0 when it names none.
int _leaked(String type) {
  for (final row in residentLeakReport()) {
    final at = row.lastIndexOf(':');
    if (row.substring(0, at) == type) return int.parse(row.substring(at + 1));
  }
  return 0;
}

/// Mint a `Scene` and abandon it — no `dispose()`, no returned reference.
///
/// Its own never-inlined function for the reason `gc_finalizer_test.dart`'s
/// probe is: a local in the test body can stay live on the frame for the rest
/// of the method, which would keep the handle reachable and make a red result
/// meaningless.
@pragma('vm:never-inline')
void _abandonScene() {
  final s = Scene.new_();
  s.add(name: 'leaf');
  if (s.isDisposed) throw StateError('a fresh scene cannot be disposed');
}

void main() {
  setUpAll(initBridge);

  test('a !Send object crosses, mutates and reads on the caller', () {
    // One turn, no await: see the library doc on why that is the shape.
    final scene = Scene.new_();
    expect(scene.count(), 0);
    scene.add(name: 'root');
    scene.add(name: 'leaf');
    expect(scene.count(), 2);
    // Read through the *second* `Rc` owner. Two owners of one allocation is
    // what makes the type `!Send`; a single-owner `Rc` is indistinguishable
    // from a `Box` at runtime and would pass for the wrong reason.
    expect(
      scene.sharedCount(),
      2,
      reason: 'the second Rc clone sees the same allocation',
    );
    scene.dispose();
    expect(scene.isDisposed, isTrue);
  });

  test(
    'a resident handle is lent beside the receiver, like any Box-held one',
    () {
      final a = Scene.new_();
      final b = Scene.new_();
      a.add(name: 'a1');
      b.add(name: 'b1');
      b.add(name: 'b2');
      expect(sceneMerge(into: a, from: b), 3);
      expect(a.count(), 3);
      expect(b.count(), 2, reason: 'the lent handle is untouched');
      a.dispose();
      b.dispose();
    },
  );

  test('a consuming member takes the object and spends the handle', () {
    final scene = Scene.new_();
    scene.add(name: 'only');
    expect(scene.take().intoNames(), ['only']);
    expect(scene.isDisposed, isTrue);
    // Spent, not merely emptied: dispose() is a no-op and any use throws.
    scene.dispose();
    expect(() => scene.count(), throwsStateError);
  });

  test('dispose() frees the object it built', () {
    final before = residentLiveCount();
    // Relative, not zero: an earlier test in this file may legitimately have
    // stranded one (a reclaim that landed on a migrated thread), and that is a
    // different fact from the one under test here.
    final leakedBefore = _leaked('Scene');
    final scene = Scene.new_();
    expect(residentLiveCount(), before + 1);
    scene.dispose();
    expect(
      residentLiveCount(),
      before,
      reason: 'the registry entry goes with the object',
    );
    // Idempotent, and a second dispose must not double-free or re-report.
    scene.dispose();
    expect(residentLiveCount(), before);
    expect(
      _leaked('Scene'),
      leakedBefore,
      reason: 'an orderly dispose leaks nothing',
    );
  });

  /// The `dart:core` `Finalizer` path — the one thing that distinguishes this
  /// model's reclaim from every other model's on native.
  ///
  /// Dart guarantees only that a finalizer *may* run, so a red result here is
  /// not proof the path is dead — the same caveat `gc_finalizer_test.dart`
  /// carries. It is still the useful signal: a reclaim that needs this much
  /// prodding is not one a developer would ever see.
  test('an abandoned handle is reclaimed by the isolate that built it', () async {
    final before = residentLiveCount();
    _abandonScene();
    expect(residentLiveCount(), before + 1, reason: 'minted and not disposed');

    final leakedBefore = _leaked('Scene');
    await _turnUntil(() => residentLiveCount() == before);
    expect(
      residentLiveCount(),
      before,
      reason: 'the Finalizer callback reached frustrate_finalize_Scene',
    );
    // A *strand* also retires the entry, so the count alone cannot tell the
    // two apart. This is what says the object was freed rather than merely
    // written off — the callback ran on the thread that built it, which is the
    // property `dart:core`'s Finalizer is chosen for.
    expect(
      _leaked('Scene'),
      leakedBefore,
      reason: 'reclaimed on the owning thread, not stranded',
    );
  }, timeout: const Timeout(Duration(minutes: 2)));

  /// The price of the model, made observable.
  ///
  /// An isolate that exits cannot free its residents — only the thread that
  /// built one may run its `Drop` — so the object is unreachable *and*
  /// unreclaimable, and the report is the only thing left to assert on.
  ///
  /// Killed rather than left to exit on its own, and deterministic because of
  /// it: `Isolate.kill` is what Flutter hot restart does, and the exit
  /// listener fires before anything here reads the report.
  test(
    'an isolate that exits holding a resident is reported by type name',
    () async {
      final before = _leaked('Scene');
      final ready = ReceivePort();
      final exited = ReceivePort();
      final child = await Isolate.spawn(_mintAndWait, ready.sendPort);
      child.addOnExitListener(exited.sendPort);
      await ready.first;
      ready.close();
      child.kill(priority: Isolate.immediate);
      await exited.first;
      exited.close();

      // The exit notice reaches Rust on the child's own port, so the sweep may
      // land a turn or two after the listener fires. No allocation pressure:
      // nothing here is waiting on a collection.
      await _turnUntil(() => _leaked('Scene') > before, pressure: false);
      expect(
        _leaked('Scene'),
        before + 1,
        reason: 'the dead isolate left one Scene nothing can ever free',
      );
      // The same fact through the transport, which is all an app that installed
      // no `log` sink can see.
      expect(
        Frustrate.instance.residentLeakCount,
        greaterThan(0),
        reason: 'the loss is visible from Dart, not only in the Rust log',
      );
    },
    timeout: const Timeout(Duration(minutes: 2)),
  );
}

/// Builds one `Scene`, says so, and waits to be killed. The handle stays
/// reachable on the frame, so nothing can reclaim it before the kill.
///
/// **The open [ReceivePort] is what makes the test deterministic, and it is
/// not decoration.** An unawaited `Completer` does not keep an isolate alive —
/// only a live port does — so without it this isolate ran to the end of
/// `main` and exited on its own, racing the parent's `addOnExitListener`.
/// `Isolate.addOnExitListener` on an isolate that is already dead sends
/// nothing, so the parent waited forever. Measured: under a loaded
/// `bazel test //...` the child won that race three runs in four.
///
/// With the port held, the isolate cannot end on its own and `Isolate.kill`
/// is the only thing that ends it — which is also the case under test, since
/// a forced kill is what a Flutter hot restart does.
Future<void> _mintAndWait(SendPort reply) async {
  await initBridge();
  final hold = ReceivePort();
  final scene = Scene.new_();
  scene.add(name: 'held');
  reply.send(scene.count());
  await hold.first;
}
