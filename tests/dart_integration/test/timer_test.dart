/// `frustrate::runtime::sleep` — the portable "wait a bit", on every platform.
///
/// The runtime has no clock: the executor is a poll loop with no reactor, and a
/// stock wasm32 module has no host to ask. So `sleep` is a slot the app fills,
/// exactly as `logging::install` and `panic::register` are, and this drives the
/// filling an app actually writes — a Dart `Future.delayed` behind a
/// `DartFunction`.
///
/// The claim that needs a platform is that a sleeping task is a *future* and not
/// a parked thread. On single-threaded web there is one thread; a `sleep` that
/// blocked it would freeze the page rather than fail an assertion, so the
/// concurrency test below is the one that would catch it.
///
/// No `@TestOn`: it must run under the VM and both browser configurations.
library;

import 'dart:async';

import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

void main() {
  setUpAll(() async {
    await initBridge();
    // The install an app writes once at startup. Two one-way trips: a request
    // out, `fireTimer` back in. Not a DartFunction — the closure behind one
    // answers synchronously, so it cannot wait, which is the whole reason this
    // shape exists.
    installTimer(
      requests: (r) {
        final (id, millis) = r;
        Timer(Duration(milliseconds: millis), () => fireTimer(id: id));
      },
    );
  });

  // The installed callback holds a StreamSink, and an open stream pins the
  // isolate (docs/design/streams.md) — an app wants exactly that for its whole
  // life, and a suite that never released it would pass every test and then
  // hang instead of exiting. `dart test` force-exits and hides this; the Bazel
  // runner waits, and is how it was found.
  tearDownAll(uninstallTimer);

  test('sleep waits, then answers', () async {
    final watch = Stopwatch()..start();
    expect(await sleepThen(ms: 60, x: 7), 7);
    watch.stop();
    // A lower bound only. An upper bound would be asserting the machine is not
    // busy, which is not a property of this code.
    expect(
      watch.elapsedMilliseconds,
      greaterThanOrEqualTo(50),
      reason: 'the call answered before its sleep elapsed',
    );
  });

  test('many sleeps overlap instead of queueing', () async {
    // The decisive one. 24 sleeps of 60ms cost ~60ms if each is a suspended
    // future and ~1.4s if each holds a thread — and on single-threaded web
    // there is no second thread to hold, so a blocking sleep would hang the
    // page here rather than merely be slow.
    const n = 24;
    final watch = Stopwatch()..start();
    final answers = await Future.wait([
      for (var i = 0; i < n; i++) sleepThen(ms: 60, x: i),
    ]);
    watch.stop();
    expect(answers, [for (var i = 0; i < n; i++) i]);
    expect(
      watch.elapsedMilliseconds,
      lessThan(60 * n ~/ 2),
      reason: 'the sleeps serialised, so they are not futures',
    );
  });

  test('a detached backoff loop sleeps between attempts', () async {
    // spawn + sleep together: the shape the pair exists for. The call answers
    // immediately and everything observable happens afterwards, paced by the
    // timer, in a task nothing is waiting on.
    final c = StreamController<int>();
    final got = <int>[];
    final done = c.stream.listen(got.add).asFuture<void>();

    final watch = Stopwatch()..start();
    spawnBackoff(attempts: 3, ms: 20, sink: c);
    expect(got, isEmpty, reason: 'the call carries none of the loop');

    await done;
    watch.stop();
    expect(got, [1, 2, 3]);
    // 20 + 40 + 80: the doubling is the point, so a loop that ignored its
    // delay would come back far too early.
    expect(
      watch.elapsedMilliseconds,
      greaterThanOrEqualTo(120),
      reason: 'the backoff did not actually back off',
    );
  });
}
