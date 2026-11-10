/// `thread::park` on single-threaded web, against a std from
/// toolchain/custom_std: a park nothing can unpark panics with a message that
/// says so, instead of spinning the only thread forever.
///
///     bazel run //toolchain/custom_std:build -- --facilities=clock,random,stdio,thread
///     bazel run //tests/dart_integration:web_test -- --browser-std test/std_park_web_test.dart
///
/// On any other fixture the probes return -1 and every test here skips.
///
/// **The two trapping calls are the last thing each part does.** Web is
/// `panic=abort`, so a panic leaves its wasm instance's Rust state arbitrary;
/// the actor runs in its own Worker with its own instance, so its trap leaves
/// the main instance alone, and the main-instance trap comes last.
@TestOn('browser')
library;

import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_web.dart';

const _message = 'park on single-threaded wasm: nothing can unpark this thread';

Matcher get _parkPanic => throwsA(
  isA<BridgePanicException>().having(
    (e) => e.message,
    'message',
    contains(_message),
  ),
);

void main() {
  setUpAll(initBridge);

  // The early return lives here rather than in a helper a caller could forget:
  // `markTestSkipped` records the skip and lets the body run on.
  bool skipped() {
    if (stdParkAfterUnpark() != -1) return false;
    markTestSkipped(
      'fixture is not a single-threaded custom std; build it with '
      '`bazel run //tests/dart_integration:web_test -- --browser-std`',
    );
    return true;
  }

  test('park after an unpark returns: the token is the contract', () {
    if (skipped()) return;
    expect(stdParkAfterUnpark(), 1);
    // The token was consumed, so a second park would have nothing to wait for;
    // that call is the trapping one below and is not repeated here.
  });

  test('park_timeout with no token returns instead of waiting', () {
    if (skipped()) return;
    expect(stdParkTimeout(), 1);
  });

  test('park with nothing to unpark it panics, on an actor', () async {
    if (skipped()) return;
    final miner = await Miner.new_(label: 'parker');
    await expectLater(miner.parkOnExecutor(), _parkPanic);
  });

  test('park with nothing to unpark it panics, on the main instance', () {
    if (skipped()) return;
    expect(stdPark, _parkPanic);
  });
}
