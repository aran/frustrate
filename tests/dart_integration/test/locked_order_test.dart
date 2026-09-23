/// Members taking two locked handles keep working when the handles are
/// distinct — including a pair of members that name the same two types in
/// opposite parameter orders, which is the inversion `handle::lock_plan`
/// exists to prevent.
///
/// The planned acquisition changes the *order* guards are taken in, never
/// which guards a call ends up holding, so these are the control: whatever
/// the plan does, the call still sees both objects and mutates them.
///
/// Reads go through `readBalance`, the dispatched sibling, so what these
/// assert is the ordering rather than the acquisition mode. The try-lock arm
/// of the same plan is in locked_sync_test.dart.
library;

import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

void main() {
  setUpAll(initBridge);

  test('two distinct locked handles are both acquired', () async {
    final a = Vault.new_(balance: 100);
    final b = Vault.new_(balance: 1);
    expect(await transfer(from: a, to: b, amount: 10), 11);
    expect(await a.readBalance(), 90);
  });

  test('the reversed-order sibling reaches the same result', () async {
    final a = Vault.new_(balance: 100);
    final b = Vault.new_(balance: 1);
    expect(await transferReversed(to: b, from: a, amount: 10), 11);
    expect(await a.readBalance(), 90);
  });

  test('a locked receiver and a distinct locked parameter merge', () async {
    final a = Vault.new_(balance: 5);
    final b = Vault.new_(balance: 7);
    expect(await a.merge(other: b), 12);
    expect(await b.readBalance(), 0);
  });
}
