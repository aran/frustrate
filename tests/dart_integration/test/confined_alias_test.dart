/// Handing one confined handle to both parameters of a member is refused, and
/// the refusal names the member and both parameters.
///
/// A confined handle is a raw `Box` pointer. `absorb(&mut self, other: &Ledger)`
/// called with one object materializes `&mut T` and `&T` over one allocation —
/// undefined behaviour under Rust's exclusivity rule, which Miri reports as a
/// borrow-stack violation. So the handles are compared before either deref
/// happens (`handle::alias_check`).
///
/// The file holds exactly one call on purpose, for the reason
/// `trap_attribution_test.dart` gives: web is `panic=abort`, so the trap leaves
/// the instance's Rust state arbitrary. The calls that must keep working are in
/// `confined_shared_test.dart`.
library;

import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

void main() {
  setUpAll(initBridge);

  test('one handle for a mutable and a shared parameter is refused', () {
    final l = Ledger.new_(total: 5);
    expect(
      () => l.absorb(other: l),
      throwsA(
        isA<BridgePanicException>().having(
          (e) => e.message,
          'message',
          allOf(
            contains('Ledger::absorb'),
            contains('self'),
            contains('other'),
          ),
        ),
      ),
    );
  });
}
