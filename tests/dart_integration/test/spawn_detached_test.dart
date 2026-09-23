/// `frustrate::runtime::spawn` — background work with no call behind it, on a
/// real platform rather than against a hand-built executor.
///
/// The unit tests (runtime/rust/src/executor.rs) own the task lifecycle: that a
/// detached task is not in the call registry, survives a `Pending` poll,
/// answers nobody, and reports a panic to the listener. What only a platform
/// can say is that the Scheduler seam carries it — pool workers on native and
/// threaded web, the `queueMicrotask` drain on single-threaded web — and that a
/// suspended detached task is resumed by a wake arriving from **outside** the
/// drain that started it.
///
/// `spawnDetachedEcho` is built for that second claim. Its call answers
/// immediately; the detached task then makes three `DartFunction::call_async`
/// round trips, each of which returns `Pending` and is resumed by
/// `frustrate_callback_respond` on a later drain. On single-threaded web the
/// awaited call has already returned to Dart before the first of those resumes,
/// so the task is running with nothing but its own detached handle keeping it
/// alive.
///
/// No `@TestOn`: the claim is per-platform, so it must run under the VM and
/// under both browser configurations.
library;

import 'dart:async';

import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

void main() {
  setUpAll(initBridge);

  test(
    'a detached task outlives its call and is resumed by later wakes',
    () async {
      final c = StreamController<int>();
      final got = <int>[];
      final done = c.stream.listen(got.add).asFuture<void>();

      // The call answers before any of the task's work has happened.
      expect(await spawnDetachedEcho(x: 1, f: (v) => v * 3, sink: c), 1);
      expect(
        got,
        isEmpty,
        reason:
            'the answered call carries none of the detached task; every '
            'item below is produced after it returned',
      );

      await done;
      expect(
        got,
        [3, 9, 27],
        reason:
            'three suspensions, each resumed by a callback response '
            'arriving outside the drain that spawned the task',
      );
    },
  );

  test('detached tasks run concurrently with each other and with calls', () async {
    // One executor, one run queue: several detached tasks and an ordinary
    // awaited call are all multiplexed on it. On single-threaded web that is
    // literally one thread, so a task that parked a thread instead of a future
    // would deadlock the page here rather than fail an assertion.
    const n = 8;
    final controllers = [for (var i = 0; i < n; i++) StreamController<int>()];
    final collected = [for (final c in controllers) c.stream.toList()];
    final answers = await Future.wait([
      for (var i = 0; i < n; i++)
        spawnDetachedEcho(x: i + 1, f: (v) => v + 1, sink: controllers[i]),
      transform(x: 100, f: (v) => v + 1),
    ]);

    expect(answers, [for (var i = 0; i < n; i++) i + 1, 101]);
    expect(await Future.wait(collected), [
      for (var i = 0; i < n; i++) [i + 2, i + 3, i + 4],
    ]);
  });

  test('a detached task keeps its stream open after the call is answered', () async {
    // The refcount claim from docs/design/streams.md: the call that opened the
    // sink is long gone, and the stream is still live because the detached
    // producer holds it. It closes when the task ends and the sink drops —
    // nothing else closes it, so a `done` that never completes is the failure.
    final c = StreamController<int>();
    var closed = false;
    unawaited(c.stream.drain<void>().then((_) => closed = true));

    await spawnDetachedEcho(x: 2, f: (v) => v, sink: c);
    expect(closed, isFalse, reason: 'the producer outlives its call');

    await c.done;
    expect(closed, isTrue, reason: 'and the task ending is what closes it');
  });
}
