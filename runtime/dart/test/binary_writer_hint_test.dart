/// The `BinaryWriter` size-hint contract: the hint is a LOWER BOUND and never
/// an assertion.
///
/// Generated encoders pass a partial sum of the top-level parameters they are
/// about to write (`codegen`'s `SizeHint`), which is deliberately incomplete —
/// a map, a struct, an extern contribute nothing. So the bytes a writer
/// produces must be independent of the hint in *both* directions: a hint far
/// below the payload, a hint far above it, and no hint at all must all yield
/// identical bytes. Anything that made the hint load-bearing (an assert, a
/// capacity-derived length, a fast path that assumed the buffer was big
/// enough) would corrupt the wire rather than merely waste memory, so this is
/// pinned here rather than left to the perf claim that motivated it.
///
/// Pure Dart, no dylib and no browser — the same reason `stream_router_test`
/// and `pending_calls_test` live at this level.
@TestOn('vm')
library;

import 'dart:typed_data';

import 'package:frustrate/src/binary_codec.dart';
import 'package:test/test.dart';

/// Writes one of everything the codec can emit, so the comparison covers the
/// `_ensure`-per-scalar path, the bulk `setRange` paths, and a growth step
/// that starts from a non-power-of-two capacity.
void writeSampler(BinaryWriter w, Uint8List payload, String text) {
  w.writeBool(true);
  w.writeI8(-8);
  w.writeU8(200);
  w.writeI16(-300);
  w.writeU16(60000);
  w.writeI32(-70000);
  w.writeU32(4000000000);
  w.writeI64(-1234567890123);
  w.writeU64(BigInt.parse('18446744073709551615'));
  w.writeI128(-(BigInt.one << 100));
  w.writeU128((BigInt.one << 127) + BigInt.one);
  w.writeF32(1.5);
  w.writeF64(-2.25);
  w.writeUsize(42);
  w.writeIsize(-42);
  w.writeChar('\u{1F600}');
  w.writeHandle(0xDEADBEEF);
  w.writeString(text);
  w.writeBytes(payload);
  w.writeByteArray(Uint8List.fromList(List.generate(16, (i) => i)), 16);
  // A trailing scalar after the bulk writes: catches a `_len`/`_view` that
  // drifted apart across a growth step.
  w.writeI64(7);
}

void main() {
  final payload = Uint8List.fromList(List.generate(200000, (i) => i & 0xFF));
  // Deliberately not ASCII: `String.length` is UTF-16 code units, which the
  // hint rule leans on as a lower bound for the UTF-8 byte count.
  final text = 'héllo \u{1F600} wörld ' * 500;

  Uint8List bytesWith(int? hint) {
    final w = hint == null ? BinaryWriter() : BinaryWriter(hint);
    writeSampler(w, payload, text);
    return w.takeBytes();
  }

  group('the hint is a lower bound, never an assertion', () {
    final reference = bytesWith(null);

    test('the payload is big enough for this test to mean something', () {
      expect(reference.length, greaterThan(200000));
    });

    test('a hint far below the payload round-trips identically', () {
      // 1 byte: every growth step still has to happen.
      expect(bytesWith(1), reference);
      expect(bytesWith(0), reference);
      // Negative cannot arise from a generated sum, but tolerating it costs
      // nothing and a clamp that only handled zero would be a trap.
      expect(bytesWith(-1000), reference);
      // The realistic under-estimate: a sum that saw the bytes but not the
      // string, or vice versa.
      expect(bytesWith(payload.length), reference);
      expect(bytesWith(text.length), reference);
    });

    test('a hint far above the payload round-trips identically', () {
      expect(bytesWith(reference.length * 4), reference);
      expect(bytesWith(8 * 1024 * 1024), reference);
    });

    test('an exact hint round-trips identically', () {
      expect(bytesWith(reference.length), reference);
    });

    test('a non-power-of-two hint that must still grow is identical', () {
      // 1 byte short of exact, so `_ensure` doubles from an odd base — the
      // one growth shape the 64-byte default could never produce.
      expect(bytesWith(reference.length - 1), reference);
      expect(bytesWith(reference.length ~/ 3), reference);
      expect(bytesWith(12345), reference);
    });

    test('takeBytes is bounded by what was written, not by the capacity', () {
      // The over-hinted writer owns a much larger buffer; the bytes it hands
      // out must still stop at the last thing written. A view that leaked the
      // capacity would put trailing garbage on the wire.
      final w = BinaryWriter(1 << 20);
      w.writeI64(1);
      w.writeI64(2);
      expect(w.takeBytes().length, 16);
      expect(BinaryReader(w.takeBytes()).readI64(), 1);
    });

    test('the default is unchanged: a small write still fits 64 bytes', () {
      final w = BinaryWriter();
      w.writeI64(1);
      expect(w.takeBytes().length, 8);
    });
  });

  test('a hinted writer decodes back to the values written', () {
    // The round-trip, not just byte equality: proves the reference itself is
    // right rather than consistently wrong.
    final r = BinaryReader(bytesWith(1));
    expect(r.readBool(), isTrue);
    expect(r.readI8(), -8);
    expect(r.readU8(), 200);
    expect(r.readI16(), -300);
    expect(r.readU16(), 60000);
    expect(r.readI32(), -70000);
    expect(r.readU32(), 4000000000);
    expect(r.readI64(), -1234567890123);
    expect(r.readU64(), BigInt.parse('18446744073709551615'));
    expect(r.readI128(), -(BigInt.one << 100));
    expect(r.readU128(), (BigInt.one << 127) + BigInt.one);
    expect(r.readF32(), 1.5);
    expect(r.readF64(), -2.25);
    expect(r.readUsize(), 42);
    expect(r.readIsize(), -42);
    expect(r.readChar(), '\u{1F600}');
    expect(r.readHandle(), 0xDEADBEEF);
    expect(r.readString(), text);
    expect(r.readBytes(), payload);
    expect(
      r.readByteArray(16),
      Uint8List.fromList(List.generate(16, (i) => i)),
    );
    expect(r.readI64(), 7);
    r.assertConsumed();
  });
}
