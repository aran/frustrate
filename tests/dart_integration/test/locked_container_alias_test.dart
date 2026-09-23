/// One locked handle sent twice **inside one list** is refused, and the
/// refusal names the two elements.
///
/// The same rule `locked_alias_test.dart` drives at two parameters, over a
/// count the caller decides: the plan is built from the handles the list
/// actually carried, so the positions it names are elements (`vs[0]`,
/// `vs[1]`) rather than parameter names. Two read guards on one `RwLock` from
/// one task do not error — they can deadlock against a queued writer — so the
/// duplicate is refused before any guard is taken.
///
/// Its own file, and exactly one call in it, for the reason
/// `locked_alias_test.dart` gives: web is `panic=abort`, so the trap leaves
/// the instance's Rust state arbitrary and nothing may be asserted after it in
/// the same page.
library;

import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

void main() {
  setUpAll(initBridge);

  test(
    'one locked handle twice in one list is refused, naming both elements',
    () async {
      final v = Vault.new_(balance: 10);
      await expectLater(
        vaultsTotal(vs: [v, v]),
        throwsA(
          isA<BridgePanicException>().having(
            (e) => e.message,
            'message',
            allOf(
              contains('vaults_total'),
              contains('vs[0]'),
              contains('vs[1]'),
            ),
          ),
        ),
      );
    },
  );
}
