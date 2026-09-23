/// The wire-schema fingerprint guard (codegen/src/hash.rs): the loaded
/// library exports `frustrate_schema_hash`, the bindings carry the same value
/// as `frustrateSchemaHash`, and the transport compares them at init. A
/// mismatch means stale bindings — which would otherwise dispatch wrong
/// fn_ids / decode wrong types into unsafe handle derefs — so it must fail
/// loudly, not silently corrupt.
///
/// Prerequisite: `cargo build -p test_api` (native) or the web fixture build.
library;

import 'package:frustrate/frustrate.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

void main() {
  setUpAll(initBridge);

  test('matching schema passes', () {
    // initBridge already ran checkFrustrateSchema(); a second explicit call is
    // still a no-throw (the loaded library matches these bindings).
    expect(checkFrustrateSchema, returnsNormally);
  });

  test('a mismatched expected hash throws a loud, actionable error', () {
    // Flip a bit so the "compiled-in" value no longer matches the library.
    final wrong = frustrateSchemaHash ^ BigInt.one;
    expect(
      () => Frustrate.instance.checkSchemaHash(wrong),
      throwsA(
        isA<StateError>().having(
          (e) => e.message,
          'message',
          allOf(contains('stale'), contains('regenerate')),
        ),
      ),
    );
  });
}
