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

import 'package:frustrate/frustrate.dart' show Frustrate;
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';
import 'zone_heard.dart';

/// What the zone frustrate was initialised in has heard.
final _initZoneHeard = ZoneHeard();

Matcher _panic(String message) => isA<BridgePanicException>().having(
  (e) => e.message,
  'message',
  contains(message),
);

void main() {
  setUpAll(() => _initZoneHeard.run(initBridge));

  test('a posted closure that panics reaches the zone the actor was built '
      'in, and the actor keeps serving', () async {
    final (s, heard) = await guarded(Slicer.new_);
    await s.postPanic(message: 'a posted closure panicked');
    await heard.first;
    expect(heard.errors, [_panic('a posted closure panicked')]);
    await s.mark(label: 'after');
    expect(await s.entries(), ['after']);
    await s.dispose();
  });

  test('Addr.call on a closure that panics resolves to Gone', () async {
    final (s, heard) = await guarded(Slicer.new_);
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
    await heard.first;
    expect(heard.errors, [_panic('a called closure panicked')]);
    await s.dispose();
  });

  test('a detached task an actor spawns reaches the actor\'s zone', () async {
    final (s, heard) = await guarded(Slicer.new_);
    await s.spawnPanic(message: 'a detached task panicked');
    await heard.first;
    expect(heard.errors, [_panic('a detached task panicked')]);
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
      final (s, heard) = await guarded(Slicer.new_);
      await s.spawnLocalPanic(message: 'a spawn_local task panicked');
      await heard.first;
      expect(heard.errors, [_panic('a spawn_local task panicked')]);
      expect(await s.slices(), 0, reason: 'the actor still answers');
      await s.dispose();
    },
  );

  test('a detached task on no actor reaches the zone frustrate was '
      'initialised in', () async {
    spawnDetachedPanic(message: 'a detached task on no actor panicked');
    await _initZoneHeard.first;
    expect(_initZoneHeard.errors, [
      _panic('a detached task on no actor panicked'),
    ]);
  });
}
