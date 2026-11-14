/// `FrustrateNative.manualDrain` when the garbage collector schedules Rust
/// work: a handle collected without `dispose()` is dropped by its finalizer,
/// and a `Drop` that spawns puts the task on the run queue when the collector
/// chooses. The next drain throws, naming the handle type, and the one after
/// runs the work. `FrustrateNative.manualHandles` counts handles made and
/// given back, the same whether or when the collector ran.
///
/// Its own target, as `gc_finalizer_test` is: whether a finalizer runs within
/// the time allowed is up to the VM.
@TestOn('vm')
library;

import 'package:frustrate/frustrate.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart';

/// Allocate a handle and abandon it. Its own function, never inlined, so no
/// frame keeps the handle reachable.
@pragma('vm:never-inline')
void _abandon() {
  final handle = SpawnsOnDrop.new_();
  if (handle.isDisposed) throw StateError('a fresh handle cannot be disposed');
}

void main() {
  setUpAll(initBridge);

  test('a disposed handle\'s Drop schedules work the drain runs', () {
    final drain = FrustrateNative.manualDrain();
    final handles = FrustrateNative.manualHandles();
    while (drain()) {}
    handles();
    final before = dropSpawnsRun();
    for (var i = 0; i < 3; i++) {
      SpawnsOnDrop.new_().dispose();
    }
    expect(drain(), isTrue);
    expect(dropSpawnsRun(), before + 3);
    expect(handles()['SpawnsOnDrop'], (made: 3, disposed: 3));
  });

  test('a call that takes the object gives its handle back', () {
    final handles = FrustrateNative.manualHandles();
    handles();
    expect(Tape.new_(marks: ['a', 'b']).take().intoCount(), 2);
    final kept = Tape.new_(marks: []);
    expect(handles()['Tape'], (made: 2, disposed: 1));
    expect(handles(), isEmpty, reason: 'each read starts the count again');
    kept.dispose();
    expect(handles()['Tape'], (made: 0, disposed: 1));
  });

  test(
    'work a finalizer schedules fails the next drain, naming the type',
    () async {
      final drain = FrustrateNative.manualDrain();
      final handles = FrustrateNative.manualHandles();
      while (drain()) {}
      handles();
      final before = dropSpawnsRun();
      _abandon();
      final abandoned = handles();

      StateError? error;
      final elapsed = Stopwatch()..start();
      var sink = 0;
      while (error == null && elapsed.elapsed < const Duration(seconds: 10)) {
        // Short-lived multi-page allocations promote a scavenge; the sum keeps
        // the optimizer from eliding them.
        for (var i = 0; i < 32; i++) {
          final junk = List<int>.filled(1 << 14, i);
          sink += junk[junk.length - 1];
        }
        await Future<void>.delayed(Duration.zero);
        try {
          expect(drain(), isFalse, reason: 'nothing else is queued');
        } on StateError catch (e) {
          error = e;
        }
      }
      expect(sink, isNot(0));
      expect(error, isNotNull, reason: 'the handle was not finalized in 10s');
      expect(error!.message, contains('SpawnsOnDrop: 1'));
      expect(abandoned['SpawnsOnDrop'], (
        made: 1,
        disposed: 0,
      ), reason: 'read before the collector ran');
      expect(handles(), isEmpty, reason: 'a finalizer gives nothing back');
      expect(
        dropSpawnsRun(),
        before,
        reason: 'the refused drain polled nothing',
      );

      expect(drain(), isTrue, reason: 'the report was read, so the work runs');
      expect(dropSpawnsRun(), before + 1);
    },
  );
}
