/// `Addr`: an actor posting closures to itself, on every fixture.
///
/// The ordering checks are by construction, not by timing. Natively a post
/// made during a call is queued before that call's answer leaves the
/// executor, so a call sent after the answer arrives runs after the post. On
/// web the browser interleaves the worker's posts with the calls it receives,
/// so only the order among posts and among calls is checked there; that
/// calls do get to run between slices is measured instead.
@Timeout(Duration(minutes: 2))
library;

import 'dart:async';

import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

const bool isWeb = bool.fromEnvironment('dart.library.js_interop');

void main() {
  setUpAll(initBridge);

  test('posts run in post order, after the call that made them', () async {
    final s = await Slicer.new_();
    await s.postMark(label: 'P1');
    await s.mark(label: 'C1');
    await s.postMark(label: 'P2');
    await s.mark(label: 'C2');
    // A post runs as a message of its own; wait for both to have run.
    while ((await s.entries()).length < 4) {
      await Future<void>.delayed(Duration.zero);
    }
    final entries = await s.entries();
    expect(entries, containsAll(['P1', 'C1', 'P2', 'C2']));
    expect(entries.indexOf('P1'), lessThan(entries.indexOf('P2')));
    expect(entries.indexOf('C1'), lessThan(entries.indexOf('C2')));
    // P2 was posted by a call that ran after C1.
    expect(entries.indexOf('C1'), lessThan(entries.indexOf('P2')));
    if (!isWeb) {
      // The host's one queue: each post is queued before its call answers.
      expect(entries, ['P1', 'C1', 'P2', 'C2']);
    }
    await s.dispose();
  });

  test('a post and the calls sent with it all run, calls in order', () async {
    final s = await Slicer.new_();
    // One turn: the post's call, then two calls, none awaited.
    final posted = s.postMark(label: 'P');
    final c1 = s.mark(label: 'C1');
    final c2 = s.mark(label: 'C2');
    await Future.wait([posted, c1, c2]);
    while ((await s.entries()).length < 3) {
      await Future<void>.delayed(Duration.zero);
    }
    final entries = await s.entries();
    expect(entries, containsAll(['P', 'C1', 'C2']));
    expect(entries.indexOf('C1'), lessThan(entries.indexOf('C2')));
    await s.dispose();
  });

  test(
    'calls run between the slices of a job that only a call can stop',
    () async {
      final s = await Slicer.new_();
      // The job re-posts itself forever until `stop` runs, so it finishes only
      // if calls are served between its slices.
      final job = s.runUntilStopped(sliceUs: 0);
      final a = await s.slices();
      final b = await s.slices();
      expect(b, greaterThanOrEqualTo(a));
      await s.stop();
      final total = await job;
      expect(total, greaterThanOrEqualTo(b));
      await s.dispose();
    },
  );

  test(
    'a call waits a few slices at most behind a job that keeps posting',
    () async {
      const sliceUs = 1000;
      const calls = 300;
      final s = await Slicer.new_();
      final job = s.runUntilStopped(sliceUs: sliceUs);
      await s.slicesSince();
      final sw = Stopwatch()..start();
      final roundTripUs = <int>[];
      final between = <int>[];
      for (var i = 0; i < calls; i++) {
        final t0 = sw.elapsedMicroseconds;
        between.add(await s.slicesSince());
        roundTripUs.add(sw.elapsedMicroseconds - t0);
      }
      await s.stop();
      await job;
      await s.dispose();
      roundTripUs.sort();
      between.sort();
      final p50 = roundTripUs[calls ~/ 2];
      final p99 = roundTripUs[calls * 99 ~/ 100];
      final most = roundTripUs.last;
      // ignore: avoid_print
      print(
        'Addr liveness, $sliceUs us slices: call round trip p50 $p50 us, '
        'p99 $p99 us, max $most us; slices between calls '
        'p50 ${between[calls ~/ 2]}, max ${between.last}, '
        'total ${between.fold<int>(0, (a, b) => a + b)}',
      );
      // A call waits for the slice running when it arrives, not for the job,
      // which runs until the last call stops it. The test prints the measured
      // wait; the bound leaves room for a loaded machine.
      expect(most, lessThan(25 * sliceUs));
      // And the job keeps going while calls run.
      expect(between.fold<int>(0, (a, b) => a + b), greaterThan(calls ~/ 2));
    },
  );

  test(
    'a Deferred future applies its result to the actor through Addr.call',
    () async {
      final s = await Slicer.new_();
      expect(await s.addLater(n: 5), 5);
      expect(await s.addLater(n: 2), 7);
      expect(await s.slices(), 7);
      await s.dispose();
    },
  );

  test(
    'waiting on Addr.call inside a method body panics instead of hanging',
    () async {
      final s = await Slicer.new_();
      await expectLater(
        s.callSelfAndWait(),
        throwsA(
          isA<BridgePanicException>().having(
            (e) => e.toString(),
            'message',
            contains("awaited on its own actor's executor"),
          ),
        ),
      );
      await s.dispose();
    },
  );

  test(
    'dispose with slices pending ends the job and refuses later calls',
    () async {
      final s = await Slicer.new_();
      final job = s.runUntilStopped(sliceUs: 0);
      await s.slices();
      final jobFails = expectLater(job, throwsA(isA<StateError>()));
      await s.dispose();
      // Natively the queued slice finds the actor gone and is dropped; on web
      // the worker is terminated with it. Either way the job never resolves on
      // its own, and dispose fails it.
      await jobFails;
      await expectLater(s.slices(), throwsA(isA<StateError>()));
    },
  );
}
