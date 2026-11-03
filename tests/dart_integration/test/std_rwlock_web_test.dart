/// A conflicting `RwLock` acquire on single-threaded web, against a std from
/// toolchain/custom_std: a write requested under a live read guard can never
/// be satisfied, and surfaces as an attributed panic. std's own answer is
/// `rtabort!`, which bypasses the panic hook and so carries no message.
///
///     bazel run //toolchain/custom_std:build -- --facilities=clock,random,stdio,thread
///     bazel run //tests/dart_integration:web_test -- --facilities test/std_rwlock_web_test.dart
///
/// On any other fixture the probes return -1 and every test here skips. The
/// actor test runs first because a panic leaves its wasm instance's Rust state
/// arbitrary (`panic=abort`), and the actor's instance is not the main one.
@TestOn('browser')
library;

import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_web.dart';

Matcher get _rwlockPanic => throwsA(
  isA<BridgePanicException>().having(
    (e) => e.message,
    'message',
    contains('rwlock locked for reading'),
  ),
);

void main() {
  setUpAll(initBridge);

  bool skipped() {
    if (stdParkAfterUnpark() != -1) return false;
    markTestSkipped(
      'fixture is not a single-threaded custom std; build it with '
      '`bazel run //tests/dart_integration:web_test -- --facilities`',
    );
    return true;
  }

  test('a write under a read guard panics, on an actor', () async {
    if (skipped()) return;
    final miner = await Miner.new_(label: 'locker');
    await expectLater(miner.rwlockConflictOnExecutor(), _rwlockPanic);
  });

  test('a write under a read guard panics, on the main instance', () {
    if (skipped()) return;
    expect(stdRwlockConflict, _rwlockPanic);
  });
}
