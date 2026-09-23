/// Two shared borrows of one confined object are ordinary Rust, so they stay
/// legal — the refusal in `confined_alias_test.dart` is narrower than "no
/// handle twice", and this is the half that says so.
///
/// This is where the locked and confined refusals differ, deliberately:
/// `lock_plan` rejects any repeated locked handle, because an `RwLock` cannot
/// serve a second acquisition at all. A bare `Box` serves two `&T` perfectly
/// well, so only a pair involving `&mut` is refused.
library;

import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

void main() {
  setUpAll(initBridge);

  test('one handle passed to two shared parameters is allowed', () {
    final l = Ledger.new_(total: 5);
    expect(ledgersEqual(a: l, b: l), isTrue);
  });

  test('two distinct handles still work through the checked path', () {
    final a = Ledger.new_(total: 5);
    final b = Ledger.new_(total: 7);
    expect(a.absorb(other: b), 12);
    expect(a.total(), 12);
  });
}
