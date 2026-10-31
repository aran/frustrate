/// A Rust panic that no call waits on is an uncaught error in the zone the
/// work belongs to: an actor's posted closure or detached task in the zone the
/// actor was built in, a detached task on no actor in the zone frustrate was
/// initialised in.
///
/// Every row builds its actor in a `runZonedGuarded` zone of its own, so the
/// zone that hears a panic is the one under test, and the test's own zone
/// hearing it instead fails the test. The last row panics on the main
/// instance, which on web leaves that instance's Rust state arbitrary, so
/// nothing runs after it.
@Timeout(Duration(minutes: 2))
library;

import 'dart:async';

import 'package:frustrate/frustrate.dart' show Frustrate;
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

/// What the zone frustrate was initialised in has heard.
final _initZoneHeard = <Object>[];

/// Run [body] in a fresh guarded zone; answer its result and what the zone
/// hears.
Future<(T, List<Object>)> _guarded<T>(Future<T> Function() body) async {
  final heard = <Object>[];
  final done = Completer<T>();
  runZonedGuarded(
    () => body().then(done.complete, onError: done.completeError),
    (e, _) => heard.add(e),
  );
  return (await done.future, heard);
}

/// Wait, bounded, until [heard] holds something.
Future<void> _until(List<Object> heard) async {
  final deadline = DateTime.now().add(const Duration(seconds: 10));
  while (heard.isEmpty && DateTime.now().isBefore(deadline)) {
    await Future<void>.delayed(const Duration(milliseconds: 5));
  }
}

Matcher _panic(String message) => isA<BridgePanicException>().having(
  (e) => e.message,
  'message',
  contains(message),
);

void main() {
  setUpAll(() {
    final done = Completer<void>();
    runZonedGuarded(
      () => initBridge().then(done.complete, onError: done.completeError),
      (e, _) => _initZoneHeard.add(e),
    );
    return done.future;
  });

  test('a posted closure that panics reaches the zone the actor was built '
      'in, and the actor keeps serving', () async {
    final (s, heard) = await _guarded(Slicer.new_);
    await s.postPanic(message: 'a posted closure panicked');
    await _until(heard);
    expect(heard, [_panic('a posted closure panicked')]);
    await s.mark(label: 'after');
    expect(await s.entries(), ['after']);
    await s.dispose();
  });

  test('Addr.call on a closure that panics resolves to Gone', () async {
    final (s, heard) = await _guarded(Slicer.new_);
    await expectLater(
      s.callPanic(message: 'a called closure panicked'),
      throwsA(
        isA<BridgeException>().having(
          (e) => e.message,
          'message',
          'the actor is gone',
        ),
      ),
    );
    await _until(heard);
    expect(heard, [_panic('a called closure panicked')]);
    await s.dispose();
  });

  test('a detached task an actor spawns reaches the actor\'s zone', () async {
    final (s, heard) = await _guarded(Slicer.new_);
    await s.spawnPanic(message: 'a detached task panicked');
    await _until(heard);
    expect(heard, [_panic('a detached task panicked')]);
    expect(await s.slices(), 0, reason: 'the actor still answers');
    await s.dispose();
  });

  test(
    'a spawn_local task an actor spawns reaches the actor\'s zone',
    () async {
      // `runtime::spawn_local` exists only on single-threaded wasm; making it
      // exist for actor instances on threaded wasm is a separate decision.
      if (isNativeVm || Frustrate.instance.asyncIsParallel) {
        markTestSkipped(
          'runtime::spawn_local exists only on single-threaded wasm',
        );
        return;
      }
      final (s, heard) = await _guarded(Slicer.new_);
      await s.spawnLocalPanic(message: 'a spawn_local task panicked');
      await _until(heard);
      expect(heard, [_panic('a spawn_local task panicked')]);
      expect(await s.slices(), 0, reason: 'the actor still answers');
      await s.dispose();
    },
  );

  test('a detached task on no actor reaches the zone frustrate was '
      'initialised in', () async {
    spawnDetachedPanic(message: 'a detached task on no actor panicked');
    await _until(_initZoneHeard);
    expect(_initZoneHeard, [_panic('a detached task on no actor panicked')]);
  });
}
