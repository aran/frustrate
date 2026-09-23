/// Passing one locked handle for two parameters of a member is refused, and
/// the refusal names the member and both parameters.
///
/// Two write guards on one `RwLock` from one thread do not error — they hang.
/// Natively that wedges a pool worker forever and the future never completes;
/// on single-threaded web std's no-threads backend takes an `rtabort!`, which
/// bypasses the panic hook and cannot be attributed to anything. So the
/// acquisition is planned instead: `handle::lock_plan` sorts a call's handles
/// and refuses a repeat before taking any guard.
///
/// The file holds exactly one call on purpose, for the reason
/// `trap_attribution_test.dart` gives: web is `panic=abort`, so the trap
/// leaves the instance's Rust state arbitrary and nothing may be asserted
/// after it in the same page. The distinct-handle cases that must keep
/// working live in `locked_order_test.dart`.
library;

import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

void main() {
  setUpAll(initBridge);

  test(
    'one handle passed for two parameters is refused, naming both',
    () async {
      final v = Vault.new_(balance: 10);
      await expectLater(
        v.merge(other: v),
        throwsA(
          isA<BridgePanicException>().having(
            (e) => e.message,
            'message',
            allOf(
              contains('Vault::merge'),
              contains('this'),
              contains('other'),
            ),
          ),
        ),
      );
    },
  );
}
