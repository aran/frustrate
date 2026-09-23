/// Declaring the Rust pool's width from Dart, on every fixture — native,
/// single-threaded web, threaded web.
///
/// **Only the refusals are exercised here, and deliberately.** `dart test` runs
/// this package's suites sequentially in one process (dart_test.yaml), and the
/// pool's width is fixed once per process, so a suite that *successfully*
/// declared a width would resize the pool for every file that ran after it — in
/// filesystem order. The positive half — a declaration made before the pool
/// exists is the width it is built at — is `runtime/rust/tests/
/// declared_pool_width.rs`, which gets a process of its own for exactly this
/// reason.
///
/// What the three fixtures have in common is that neither refusal is silent:
/// the pool is *never* the width that was asked for, and the caller is always
/// told which width it is and why.
library;

import 'package:frustrate/frustrate.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

void main() {
  setUpAll(initBridge);

  test('a width the pool cannot have is refused, naming what it has', () async {
    // Build the pool first, so this suite cannot resize it however it runs and
    // whatever ran before it. On native and threaded web an async call is what
    // builds the pool; on single-threaded web there is no pool to build and the
    // call runs inline, which is the case the second arm below is about.
    expect(await poolNthPrime(n: 100), 541);
    final width = poolWidth();
    expect(width, greaterThanOrEqualTo(1));

    expect(
      () => declarePoolWidth(width: width + 1),
      throwsA(
        isA<BridgeException>().having(
          (e) => e.message,
          'message',
          Frustrate.instance.asyncIsParallel
              // Native and threaded web: a real pool, already built.
              ? contains('already fixed at $width')
              // Single-threaded web: no pool at all, so no ordering fixes
              // this and the message must not suggest one.
              : contains('no pool to size'),
        ),
      ),
    );

    // Declaring the width it does have is the hot-restart case: Flutter
    // re-enters main() in the same process against a pool that is not rebuilt,
    // so the second run's declaration is true and must not throw.
    declarePoolWidth(width: width);

    expect(poolWidth(), width, reason: 'a refused declaration changed nothing');
  });
}
