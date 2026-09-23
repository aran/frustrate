/// The negative control for `std_facilities_web_test.dart`.
///
/// That file skips every assertion when the probes return their sentinel, which
/// is right — one fixture serves both flavours and it says at runtime which it
/// is. But a suite where the interesting tests skip themselves can go green
/// while the facility path is completely broken, and nobody would notice.
///
/// This asserts the other half: on a **stock** std the sentinels are still
/// there. So the two files together distinguish three states that would
/// otherwise look alike — facilities working, facilities absent (expected), and
/// facilities present but broken.
///
/// It is the same argument as the `//tests/bazel_rules:wasm_std_check_test`
/// pair: a gate that can only pass is not a gate.
@TestOn('browser')
library;

import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_web.dart';

void main() {
  setUpAll(initBridge);

  test('a stock-std fixture reports the sentinel for every facility', () {
    if (stdClockMicros() != -1) {
      markTestSkipped(
        'this fixture has std facilities; the assertions in '
        'std_facilities_web_test.dart are the ones that apply',
      );
      return;
    }

    // All of them, not just the one the skip keyed on: a facility set that
    // half-linked — say a clock but no stdio — is exactly the state this pair
    // exists to make visible.
    expect(stdClockMicros(), -1, reason: 'SystemTime');
    expect(stdMonotonicNanos(), -1, reason: 'Instant');
    expect(stdPrintln(msg: 'should go nowhere'), 0, reason: 'stdout');
    expect(stdHashSeed(), -1, reason: 'entropy');
    expect(stdParallelism(), -1, reason: 'available_parallelism');
    expect(stdSleepMillis(millis: 1), -1, reason: 'sleep');
  });
}
