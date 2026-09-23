/// The JS-number codec arm, running on the backend that actually uses it.
///
/// `js_numbers_test` checks the same functions on the VM, where `setInt64`
/// provides a reference the browser cannot. This is the other half, and it is
/// not redundant: the arm's one genuinely double-specific hazard is invisible
/// on the VM. Reconstructing 2^53 + 1 from its halves rounds it *down* to
/// 2^53, so a range check written on the reconstructed value passes there and
/// silently accepts an out-of-range wire value. Measured: moving the guard off
/// the halves left the VM test green and turned this one red.
///
/// Run: `bazel run //runtime/dart:js_arm_web_test`
/// (`dart test -p chrome -c dart2js` — see the driver for why it cannot be a
/// `bazel test` target.)
@TestOn('browser')
library;

import 'dart:typed_data';

import 'package:frustrate/src/binary_codec.dart';
import 'package:frustrate/src/js_numbers.dart';
import 'package:test/test.dart';

import 'js_numbers_goldens.dart';

ByteData _view(List<int> bytes) =>
    ByteData.sublistView(Uint8List.fromList(bytes));

void main() {
  test('this really is a JS-number backend', () {
    // If this fails the rest of the file proves nothing: it would be
    // exercising the dart2wasm arm under a browser, which the VM test
    // already covers.
    expect(kJsNumbers, isTrue);
    expect(identical(0, 0.0), isTrue);
    expect(
      () => ByteData(8).setInt64(0, 1, Endian.little),
      throwsUnsupportedError,
      reason: 'the accessor this arm exists to replace must be absent here',
    );
  });

  test('writes the same bytes the VM 64-bit accessor produces', () {
    for (final (v, want) in i64Goldens) {
      final d = ByteData(8);
      jsWriteI64(d, 0, v);
      expect(d.buffer.asUint8List(), want, reason: 'encoding $v');
    }
  });

  test('reads those bytes back to the same values', () {
    for (final (v, bytes) in i64Goldens) {
      expect(jsReadI64(_view(bytes), 0), v, reason: 'decoding $v');
    }
  });

  test('rejects out-of-range wire values instead of truncating them', () {
    for (final bytes in outOfRangeBytes) {
      expect(
        () => jsReadI64(_view(bytes), 0),
        throwsUnsupportedError,
        reason: '$bytes must throw',
      );
    }
  });

  group('Vec<i64>', () {
    // `binary_codec.dart` gives `i64` a bulk arm that copies bytes straight
    // out of an `Int64List`. It is gated on `bulkI64Ok`, which excludes this
    // backend — and these are the two facts that exclusion rests on: the
    // element arm here writes exactly the bytes the VM's byte copy writes, and
    // the bulk arm cannot be entered here at all.
    final values = [for (final (v, _) in i64Goldens) v];
    final want = [for (final (_, bytes) in i64Goldens) ...bytes];

    test('the element arm writes exactly the bytes the VM copies', () {
      // Same expectation the VM half asserts for both of its arms, so the two
      // backends are pinned to one wire rather than to two tables.
      final w = BinaryWriter()..writeI64List(values);
      expect(w.takeBytes(), want);
    });

    test('the payload still decodes element by element', () {
      final r = BinaryReader(Uint8List.fromList(want));
      for (final v in values) {
        expect(r.readI64(), v, reason: 'decoding $v');
      }
      r.assertConsumed();
    });

    test('readI64List cannot run here: dart2js has no Int64List', () {
      // Not a frustrate refusal — `Int64List(n)` throws on its own, exactly as
      // the `Int64List.fromList(List.generate(...))` decode did before the bulk
      // pair existed. This is the `Vec<i64>` cliff docs/ANNOTATIONS.md names,
      // and it is why `bulkI64Ok` states its exclusion instead of leaning on
      // this: the codec's loudness must not rest on an SDK's inability.
      expect(() => Int64List(1), throwsUnsupportedError);
      expect(
        () => BinaryReader(Uint8List(16)).readI64List(2),
        throwsUnsupportedError,
      );
    });
  });

  test('handles round trip, and a 32-bit-wide handle is unaffected', () {
    // Every wasm32 handle is a linear-memory offset below 2^32, so this is
    // the case that has to be fast and exact, not the extreme one.
    for (final v in [0, 1, 0x1000, 0xFFFFFFFF, 0x100000000, jsMaxExact]) {
      final d = ByteData(8);
      jsWriteHandle(d, 0, v);
      expect(jsReadHandle(d, 0), v, reason: 'handle $v');
    }
  });

  test(
    'the app-facing arithmetic is still 53-bit, and this file cannot fix it',
    () {
      // Stated here because this is the file a reader lands on when they ask
      // "is the dev loop safe?". The codec is exact; the language is not.
      expect(1 << 62, 0, reason: 'bitwise operations are 32-bit here');
      expect(
        jsMaxExact + 1 == jsMaxExact,
        isTrue,
        reason: 'integers past 2^53 round silently',
      );
    },
  );
}
