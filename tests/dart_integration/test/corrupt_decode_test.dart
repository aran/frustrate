/// Corrupt-discriminant decode tests for the GENERATED bindings.
///
/// corrupt_envelope_test.dart pins the runtime primitives; this file pins the
/// generated `_decX` / response-decode path that sits on top of them. It never
/// loads the native library: it installs a fake [FrustrateRuntime] whose
/// `callSync` hands back a crafted response payload, then calls a real
/// generated function and asserts the decode fails loudly and attributably on
/// a corrupt byte — rather than silently yielding `None`/the wrong variant.
///
/// Pure-Dart and platform-agnostic (no bridge, no init), so it runs identically
/// on the VM and dart2wasm. It lives in its own isolate (one test file = one
/// isolate), so installing a fake transport here does not disturb the real
/// bridge_test transport.
library;

import 'dart:typed_data';

import 'package:frustrate/frustrate.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

/// A transport that returns a pre-staged response payload for the next call.
/// Only `callSync` is meaningful here; everything else throws if touched.
class _FakeRuntime implements FrustrateRuntime {
  /// The bytes the next `callSync` will hand back as the response payload.
  Uint8List? nextPayload;

  /// Recorded by `Frustrate.install`, and stamped onto any handle minted
  /// through this transport. A transport is its own bridge — the answer both
  /// real ones give.
  @override
  Object get bridgeIdentity => this;

  @override
  BinaryReader callSync(
    int fnId,
    int sizeHint,
    void Function(BinaryWriter w) encode, {
    Object Function(BinaryReader)? typedError,
  }) {
    // The encoder still runs, against an ordinary heap writer: the generated
    // request encode is part of what this file exercises, and a fake that
    // skipped it would be testing half a call. The bytes go nowhere — the
    // response is staged, not derived from the request.
    encode(BinaryWriter(sizeHint));
    // `typedError` is ignored deliberately: this fake hands back a *payload*,
    // never an envelope, so no status byte is ever decoded and there is no
    // typed error to route. Present only to satisfy the interface.
    final p = nextPayload;
    if (p == null) {
      throw StateError('fake runtime: no staged payload for fn_id $fnId');
    }
    nextPayload = null;
    return BinaryReader(p);
  }

  @override
  dynamic noSuchMethod(Invocation invocation) => throw UnimplementedError(
    'fake runtime: unexpected ${invocation.memberName}',
  );
}

/// Matches a StateError whose message names the codec and contains [needles].
Matcher _codecError(List<String> needles) => throwsA(
  isA<StateError>().having(
    (e) => e.message,
    'message',
    allOf([contains('frustrate codec'), ...needles.map(contains)]),
  ),
);

void main() {
  final fake = _FakeRuntime();
  // No real bridge here: the transport is a fake that hands the generated
  // decoders staged bytes. The source it is installed under just has to be
  // something no init in this isolate will name (nothing else inits at all).
  setUpAll(
    () => Frustrate.install(
      fake,
      source: fake,
      description: "corrupt_decode_test's fake transport",
    ),
  );

  group('generated Option decode routes the presence tag through readBool', () {
    test(
      'a corrupt discriminant of 2 is a loud codec error, NOT silent null',
      () {
        // maybeDouble returns Option<f64>: the response is one presence byte.
        // A byte of 2 is neither 0 nor 1; before the fix `readU8() == 1` would
        // silently decode it as `null`. The validating read rejects it.
        fake.nextPayload = Uint8List.fromList([2]);
        expect(
          () => maybeDouble(x: 1.0),
          _codecError(['invalid bool byte', '2']),
        );
      },
    );

    test('other out-of-range discriminants are rejected too', () {
      for (final bad in [3, 127, 255]) {
        fake.nextPayload = Uint8List.fromList([bad]);
        expect(
          () => maybeDouble(x: 1.0),
          _codecError(['invalid bool byte', '$bad']),
          reason: 'discriminant $bad must not decode to null',
        );
      }
    });

    test('valid discriminants still decode (0 -> null, 1 -> value)', () {
      fake.nextPayload = Uint8List.fromList([0]);
      expect(maybeDouble(x: 1.0), isNull);

      final some = BinaryWriter()
        ..writeU8(1)
        ..writeF64(3.5);
      fake.nextPayload = some.takeBytes();
      expect(maybeDouble(x: 1.0), 3.5);
    });
  });

  group('generated fieldless-enum decode bounds-checks the variant index', () {
    test('an out-of-range index is attributable (names the enum), not a bare '
        'RangeError', () {
      // nextColor returns Color (3 variants: 0..2). A u32 index of 99 is out
      // of range; the decode must raise the codec error naming Color and the
      // bad index, not `values[99]`'s unattributable RangeError.
      fake.nextPayload = (BinaryWriter()..writeU32(99)).takeBytes();
      expect(
        () => nextColor(c: Color.red),
        _codecError(['invalid variant index', '99', 'Color']),
      );
    });

    test('an in-range index still decodes to its variant', () {
      fake.nextPayload = (BinaryWriter()..writeU32(2)).takeBytes();
      expect(nextColor(c: Color.red), Color.blue);
    });
  });

  group('generated response decode asserts the buffer is fully consumed', () {
    test('trailing bytes after a decoded value are rejected, not ignored', () {
      // addI32 returns an i32 (4 bytes). A response carrying 8 bytes has 4
      // trailing garbage bytes a single-value decode would otherwise ignore.
      final w = BinaryWriter()
        ..writeI32(42)
        ..writeI32(999); // trailing garbage
      fake.nextPayload = w.takeBytes();
      expect(
        () => addI32(a: 1, b: 2),
        _codecError(['trailing', 'not fully consumed']),
      );
    });

    test('an exactly-sized response still decodes', () {
      fake.nextPayload = (BinaryWriter()..writeI32(42)).takeBytes();
      expect(addI32(a: 1, b: 2), 42);
    });
  });
}
