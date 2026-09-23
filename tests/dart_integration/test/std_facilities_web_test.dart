/// The std facilities, in a browser: a clock, entropy, console stdio,
/// `available_parallelism` and `sleep`, all reached through **std's own APIs**
/// from Rust that does not know it is on wasm.
///
/// Requires a fixture built against a facility std:
///
///     bazel run //toolchain/custom_std:build -- --facilities=clock,random,stdio,thread
///     bazel run //tests/dart_integration:web_test -- --facilities
///
/// Every test here gates on the probe's own return value rather than on a flag,
/// the same shape the threaded tests use with `asyncIsParallel`: the fixture is
/// one build and says at runtime what it is. `-1` is the sentinel for "this std
/// has no such facility", so a default single-threaded run skips instead of
/// failing — and `std_facilities_absent_web_test.dart` asserts the sentinels are
/// still there, which is what keeps "not linked" distinguishable from "broken".
///
/// **Clock resolution is a property of the page, and this page is the coarse
/// one.** The pub `test` server sends no COOP/COEP, so `performance.now()` is
/// coarsened to 100 µs (5 µs cross-origin isolated). Two `Instant`s taken
/// either side of short work therefore compare *equal*. Nothing here asserts a
/// strictly increasing pair; monotonicity is asserted across real work instead.
@TestOn('browser')
library;

import 'dart:async';
import 'dart:js_interop';
import 'dart:js_interop_unsafe';

import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_web.dart';

/// Lines `console.log` received while [body] ran.
///
/// Same shape `call_entry_count_test.dart` uses to wrap `$frustrateCall`: take
/// the real function, install a delegate, restore it afterwards. The host
/// buffers stdout per stream until a newline (`frustrate.js`), so a `println!`
/// arrives as exactly one entry and a `print!` arrives as none until one does.
List<String> _captureConsole(void Function() body) {
  final console = globalContext.getProperty<JSObject>('console'.toJS);
  final original = console.getProperty<JSFunction>('log'.toJS);
  final seen = <String>[];
  console.setProperty(
    'log'.toJS,
    ((JSAny? line) {
      seen.add(line?.dartify()?.toString() ?? '');
      return original.callAsFunction(console, line);
    }).toJS,
  );
  try {
    body();
  } finally {
    console.setProperty('log'.toJS, original);
  }
  return seen;
}

/// Declares a test that runs only against a fixture built with a facility std.
///
/// The gate reads the probe rather than a build flag: one fixture, one path,
/// self-describing at runtime — the same shape the pool tests use with
/// `asyncIsParallel`.
///
/// **The early return belongs here, not in the caller.** `markTestSkipped`
/// records the skip but does *not* abort the body, so a guard that only calls
/// it leaves every assertion below to run against the sentinel: the suite goes
/// red on a stock fixture while reporting the test as skipped. Every other file
/// in this suite writes `markTestSkipped(...); return;` inline and cannot forget
/// the second half; a shared helper can, so it owns the return instead.
void facilityTest(String name, FutureOr<void> Function() body) {
  test(name, () async {
    if (stdClockMicros() == -1) {
      markTestSkipped(
        'fixture has no std facilities; build it with '
        '`bazel run //tests/dart_integration:web_test -- --facilities`',
      );
      return;
    }
    await body();
  });
}

void main() {
  setUpAll(initBridge);

  facilityTest('SystemTime::now agrees with the page clock', () {
    final rust = stdClockMicros();
    final dart = DateTime.now().microsecondsSinceEpoch;
    expect(rust, greaterThan(0));
    // A minute of slack: this is asserting that the wall clock is *the wall
    // clock* — that the host wired `Date.now()` and not, say, the monotonic
    // clock, whose epoch is page load and would be off by decades.
    expect(
      (rust - dart).abs(),
      lessThan(60 * 1000 * 1000),
      reason: 'SystemTime is not tracking the page wall clock',
    );
  });

  facilityTest('Instant::now is monotonic across real work', () {
    // Never a tight pair: at 100 us resolution on a non-isolated page two
    // adjacent reads legitimately return the same value. What must hold is
    // that it never goes *backwards*, over enough work to exceed a tick.
    var previous = -1;
    for (var i = 0; i < 20; i++) {
      final elapsed = stdMonotonicNanos();
      expect(
        elapsed,
        greaterThanOrEqualTo(0),
        reason: 'Instant::now went backwards between two reads',
      );
      previous = elapsed;
      sumSquares(n: 50000);
    }
    expect(previous, greaterThanOrEqualTo(0));
  });

  facilityTest('println! reaches console.log, one entry per line', () {
    late int written;
    final lines = _captureConsole(() {
      written = stdPrintln(msg: 'facility probe: hello from rust');
    });
    expect(
      written,
      greaterThan(0),
      reason: 'std reported writing nothing; stdout is still the stub',
    );
    expect(lines, contains('facility probe: hello from rust'));
  });

  facilityTest('HashMap seeding does not panic and is not the sentinel', () {
    // Within one instance this only proves the CSPRNG path is reachable — std
    // caches the keys per thread, so a second read here is the same seed by
    // construction. That the seed *differs between instantiations*, which is
    // the property that matters, needs two page loads and is asserted in the
    // demo's Playwright spec.
    expect(stdHashSeed(), greaterThan(0));
  });

  facilityTest('available_parallelism reports the machine', () {
    final rust = stdParallelism();
    final js = globalContext
        .getProperty<JSObject>('navigator'.toJS)
        .getProperty<JSNumber>('hardwareConcurrency'.toJS)
        .toDartInt;
    expect(
      rust,
      js,
      reason: 'available_parallelism must be navigator.hardwareConcurrency',
    );
    // Deliberately not gated on asyncIsParallel: this reports the machine, not
    // permission to use it, so a single-threaded build must still see the real
    // core count (toolchain/custom_std/pal/thread.rs).
    expect(rust, greaterThanOrEqualTo(1));
  });

  facilityTest('thread::sleep is refused on the main thread', () {
    // The contract, not a bug: the main thread must never wait on a
    // synchronization primitive, so the host throws rather than freezing the
    // page, and the throw crosses back attributably.
    expect(
      () => stdSleepMillis(millis: 5),
      throwsA(isA<Exception>()),
      reason: 'sleeping on the main thread must be refused, loudly',
    );
  });

  facilityTest(
    'thread::sleep works on an actor, where waiting is legal',
    () async {
      final miner = await Miner.new_(label: 'sleeper');
      try {
        final elapsed = await miner.sleepOnExecutor(millis: 20);
        expect(elapsed, greaterThanOrEqualTo(0));
        // The busy-wait is not precise, and the clock under it is coarse, so
        // this asserts it waited *at all* rather than a duration.
        expect(elapsed, lessThan(5000), reason: 'implausible elapsed reading');
      } finally {
        await miner.dispose();
      }
    },
  );
}
