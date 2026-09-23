/// The bulk typed-list codec path: one memcpy per list, no boxing, and the
/// **same bytes** the per-element loop produced.
///
/// A `Vec<f64>` used to decode as
/// `Float64List.fromList(List.generate(n, (_) => r.readF64()))` — n boxed
/// doubles into a growable `List<double>`, then a copy into the typed list.
/// `readF64List`/`writeF64List` replace that with a single byte-range copy.
/// The wire is unchanged, and that is the whole point of this file: every
/// assertion here is either "the bulk form equals the element form" or a
/// pinned golden vector, so a bulk path that shifted the format goes red.
///
/// Pure Dart, no dylib and no browser — the same reason
/// `binary_writer_hint_test` and `stream_router_test` live at this level.
@TestOn('vm')
library;

import 'dart:typed_data';

import 'package:frustrate/frustrate.dart';
import 'package:test/test.dart';

void main() {
  group('the gate', () {
    // The decision table, taken as arguments rather than read off this
    // backend's compile-time constants — the same idiom as `jsNumberVerdict`,
    // and for the same reason: on any backend that can run a test, at most one
    // row is reachable in production. Taking them as arguments is what makes
    // the whole table checkable.
    test('bulk is refused unless the host is little-endian', () {
      expect(
        bulkTypedDataOk(wasmTypedData: false, littleEndianHost: false),
        isFalse,
      );
      expect(
        bulkTypedDataOk(wasmTypedData: true, littleEndianHost: false),
        isFalse,
      );
    });

    test('bulk is refused on dart2wasm even on a little-endian host', () {
      // Not an endianness question: dart2wasm backs each typed list with a
      // WasmArray of that element type, so a byte-buffer view of a
      // Float64List is a byte-composing `SlowF64List`. Taking the bulk path
      // there would hand the caller a permanently slow list.
      expect(
        bulkTypedDataOk(wasmTypedData: true, littleEndianHost: true),
        isFalse,
      );
    });

    test('bulk is taken on a little-endian non-wasm host', () {
      expect(
        bulkTypedDataOk(wasmTypedData: false, littleEndianHost: true),
        isTrue,
      );
    });

    test('an i64 bulk copy additionally needs a real 64-bit int', () {
      // The whole `bulkI64Ok` table. Reinterpreting eight bytes as an `int`
      // gives what the element loop gives only where `int` is a 64-bit
      // integer; on a JS-number backend it is a 53-bit double and the element
      // path *throws* past 2^53 rather than truncating, which a byte copy
      // cannot do.
      expect(bulkI64Ok(bulkTypedData: true, jsNumbers: false), isTrue);
      expect(bulkI64Ok(bulkTypedData: true, jsNumbers: true), isFalse);
      expect(bulkI64Ok(bulkTypedData: false, jsNumbers: false), isFalse);
      expect(bulkI64Ok(bulkTypedData: false, jsNumbers: true), isFalse);
    });
  });

  group('the bulk form is the element form', () {
    // The equivalence that keeps the wire frozen, in both directions and for
    // every converted element type. `elementBytes` is what the generated code
    // emitted before the bulk path existed.
    test('writeF64List emits exactly the per-element loop bytes', () {
      final xs = Float64List.fromList([
        0.0,
        -0.0,
        1.5,
        -2.5,
        1e308,
        double.nan,
        double.infinity,
      ]);

      final bulk = BinaryWriter();
      bulk.writeLen(xs.length);
      bulk.writeF64List(xs);

      final loop = BinaryWriter();
      loop.writeLen(xs.length);
      for (final x in xs) {
        loop.writeF64(x);
      }

      expect(bulk.takeBytes(), loop.takeBytes());
    });

    test('readF64List reads exactly what the per-element loop wrote', () {
      final xs = Float64List.fromList([3.25, -7.5, 0.0, 1e-300]);
      final w = BinaryWriter();
      w.writeLen(xs.length);
      for (final x in xs) {
        w.writeF64(x);
      }

      final r = BinaryReader(w.takeBytes());
      expect(r.readF64List(r.readLen()), xs);
      r.assertConsumed();
    });

    test('every converted element type round-trips through the bulk pair', () {
      void check<T>(
        void Function(BinaryWriter) write,
        List<num> Function(BinaryReader) read,
        List<num> expected,
      ) {
        final w = BinaryWriter();
        write(w);
        final r = BinaryReader(w.takeBytes());
        expect(read(r), expected);
        r.assertConsumed();
      }

      final i8 = Int8List.fromList([-128, -1, 0, 1, 127]);
      check(
        (w) => w
          ..writeLen(i8.length)
          ..writeI8List(i8),
        (r) => r.readI8List(r.readLen()),
        i8,
      );

      final u8 = Uint8List.fromList([0, 1, 127, 128, 255]);
      check(
        (w) => w
          ..writeLen(u8.length)
          ..writeU8List(u8),
        (r) => r.readU8List(r.readLen()),
        u8,
      );

      final i16 = Int16List.fromList([-32768, -1, 0, 1, 32767]);
      check(
        (w) => w
          ..writeLen(i16.length)
          ..writeI16List(i16),
        (r) => r.readI16List(r.readLen()),
        i16,
      );

      final u16 = Uint16List.fromList([0, 1, 32768, 65535]);
      check(
        (w) => w
          ..writeLen(u16.length)
          ..writeU16List(u16),
        (r) => r.readU16List(r.readLen()),
        u16,
      );

      final i32 = Int32List.fromList([-2147483648, -1, 0, 1, 2147483647]);
      check(
        (w) => w
          ..writeLen(i32.length)
          ..writeI32List(i32),
        (r) => r.readI32List(r.readLen()),
        i32,
      );

      final u32 = Uint32List.fromList([0, 1, 2147483648, 4294967295]);
      check(
        (w) => w
          ..writeLen(u32.length)
          ..writeU32List(u32),
        (r) => r.readU32List(r.readLen()),
        u32,
      );

      final f32 = Float32List.fromList([0.5, -0.5, 1e38, -1e-38]);
      check(
        (w) => w
          ..writeLen(f32.length)
          ..writeF32List(f32),
        (r) => r.readF32List(r.readLen()),
        f32,
      );

      final f64 = Float64List.fromList([0.5, -0.5, 1e308, -1e-308]);
      check(
        (w) => w
          ..writeLen(f64.length)
          ..writeF64List(f64),
        (r) => r.readF64List(r.readLen()),
        f64,
      );

      final i64 = Int64List.fromList([
        -9223372036854775808,
        -1,
        0,
        1,
        9223372036854775807,
      ]);
      check(
        (w) => w
          ..writeLen(i64.length)
          ..writeI64List(i64),
        (r) => r.readI64List(r.readLen()),
        i64,
      );
    });

    test('every bulk writer matches its element loop byte for byte', () {
      // Widths 1, 2 and 4 as well as 8 — a stride mistake in one of them
      // would otherwise only show as a wrong value in an integration test.
      final pairs = <String, (Uint8List, Uint8List)>{
        'i8': _both(
          Int8List.fromList([-5, 0, 5]),
          (w, v) => w.writeI8List(v),
          (w, x) => w.writeI8(x),
        ),
        'u8': _both(
          Uint8List.fromList([250, 0, 5]),
          (w, v) => w.writeU8List(v),
          (w, x) => w.writeU8(x),
        ),
        'i16': _both(
          Int16List.fromList([-300, 0, 300]),
          (w, v) => w.writeI16List(v),
          (w, x) => w.writeI16(x),
        ),
        'u16': _both(
          Uint16List.fromList([65535, 0, 300]),
          (w, v) => w.writeU16List(v),
          (w, x) => w.writeU16(x),
        ),
        'i32': _both(
          Int32List.fromList([-70000, 0, 70000]),
          (w, v) => w.writeI32List(v),
          (w, x) => w.writeI32(x),
        ),
        'u32': _both(
          Uint32List.fromList([4294967295, 0, 70000]),
          (w, v) => w.writeU32List(v),
          (w, x) => w.writeU32(x),
        ),
        'i64': _both(
          Int64List.fromList([
            -9223372036854775808,
            -1,
            0,
            1,
            9223372036854775807,
          ]),
          (w, v) => w.writeI64List(v),
          (w, x) => w.writeI64(x),
        ),
      };
      pairs.forEach((label, io) {
        expect(io.$1, io.$2, reason: label);
      });

      final f32 = _bothD(
        Float32List.fromList([1.5, -0.25, 0.0]),
        (w, v) => w.writeF32List(v),
        (w, x) => w.writeF32(x),
      );
      expect(f32.$1, f32.$2, reason: 'f32');
      final f64 = _bothD(
        Float64List.fromList([1.5, -0.25, 0.0]),
        (w, v) => w.writeF64List(v),
        (w, x) => w.writeF64(x),
      );
      expect(f64.$1, f64.$2, reason: 'f64');
    });

    test('writeI64List emits the same bytes typed or untyped', () {
      // `writeI64List` is the one method whose two arms are reachable on
      // *different backends*: the VM copies bytes out of an `Int64List`, while
      // dart2wasm and dart2js run the element loop — and so does the VM, for a
      // plain `List<int>`, which is what a generic data class
      // (`Page<T> { items: Vec<T> }` at `T = i64`) hands it. Both are here, so
      // the equality that lets the two backends share a wire is checked rather
      // than argued.
      final values = <int>[
        -9223372036854775808,
        -4294967297,
        -1,
        0,
        1,
        4294967296,
        9223372036854775807,
      ];

      final typed = BinaryWriter()..writeI64List(Int64List.fromList(values));
      final untyped = BinaryWriter()..writeI64List(values);
      final loop = BinaryWriter();
      for (final x in values) {
        loop.writeI64(x);
      }

      final want = loop.takeBytes();
      expect(typed.takeBytes(), want, reason: 'Int64List (bulk arm)');
      expect(untyped.takeBytes(), want, reason: 'List<int> (element arm)');
    });

    test('readI64List reads exactly what the per-element loop wrote', () {
      final xs = Int64List.fromList([
        -9223372036854775808,
        0,
        42,
        9223372036854775807,
      ]);
      final w = BinaryWriter();
      w.writeLen(xs.length);
      for (final x in xs) {
        w.writeI64(x);
      }

      final r = BinaryReader(w.takeBytes());
      expect(r.readI64List(r.readLen()), xs);
      r.assertConsumed();
    });
  });

  group('golden wire vectors', () {
    // Pinned bytes, mirrored by `bulk_list_wire_bytes` in
    // runtime/rust/src/codec.rs — the cross-language contract for the new
    // methods. The existing goldens pin scalars only.
    test('Vec<f64> [1.0, -2.5] is its length prefix then 16 LE bytes', () {
      final w = BinaryWriter();
      w.writeLen(2);
      w.writeF64List(Float64List.fromList([1.0, -2.5]));
      expect(w.takeBytes(), [
        // i64 length prefix: 2
        0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        // 1.0
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xF0, 0x3F, //
        // -2.5
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0xC0,
      ]);
    });

    test('Vec<i32> [1, -2] is its length prefix then 8 LE bytes', () {
      final w = BinaryWriter();
      w.writeLen(2);
      w.writeI32List(Int32List.fromList([1, -2]));
      expect(w.takeBytes(), [
        0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x01, 0x00, 0x00, 0x00, //
        0xFE, 0xFF, 0xFF, 0xFF,
      ]);
    });

    test('Vec<i64> [1, -2] is its length prefix then 16 LE bytes', () {
      final w = BinaryWriter();
      w.writeLen(2);
      w.writeI64List(Int64List.fromList([1, -2]));
      expect(w.takeBytes(), [
        0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0xFE, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
      ]);
    });

    test('a golden buffer decodes to the golden values', () {
      final r = BinaryReader(
        Uint8List.fromList([
          0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
          0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xF0, 0x3F, //
          0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0xC0,
        ]),
      );
      expect(r.readF64List(r.readLen()), [1.0, -2.5]);
      r.assertConsumed();
    });
  });

  group('edges', () {
    test('an empty list is zero bytes of payload', () {
      final w = BinaryWriter();
      w.writeLen(0);
      w.writeF64List(Float64List(0));
      final bytes = w.takeBytes();
      expect(bytes.length, 8);

      final r = BinaryReader(bytes);
      expect(r.readF64List(r.readLen()), isEmpty);
      r.assertConsumed();
    });

    test('the reader leaves the cursor exactly past the list', () {
      final w = BinaryWriter();
      w.writeLen(3);
      w.writeF64List(Float64List.fromList([1, 2, 3]));
      w.writeI32(-99);

      final r = BinaryReader(w.takeBytes());
      expect(r.readF64List(r.readLen()), [1, 2, 3]);
      expect(r.readI32(), -99);
      r.assertConsumed();
    });

    test('a typed list at a non-zero offsetInBytes encodes its own window', () {
      // `Float64List.sublistView` keeps the parent buffer; the writer must
      // read from `offsetInBytes`, not from the start of the buffer.
      final parent = Float64List.fromList([9.0, 1.0, -2.5, 9.0]);
      final window = Float64List.sublistView(parent, 1, 3);

      final w = BinaryWriter();
      w.writeLen(window.length);
      w.writeF64List(window);
      final r = BinaryReader(w.takeBytes());
      expect(r.readF64List(r.readLen()), [1.0, -2.5]);
      r.assertConsumed();
    });

    test('an i64 list at a non-zero offsetInBytes encodes its own window', () {
      // Same property as the f64 window above, at the width where a mistaken
      // `v.buffer` instead of `v.offsetInBytes` would be widest.
      final parent = Int64List.fromList([9, 1, -2, 9]);
      final window = Int64List.sublistView(parent, 1, 3);

      final w = BinaryWriter();
      w.writeLen(window.length);
      w.writeI64List(window);
      final r = BinaryReader(w.takeBytes());
      expect(r.readI64List(r.readLen()), [1, -2]);
      r.assertConsumed();
    });

    test('a truncated list is a loud, attributable codec error', () {
      // Claims 4 elements, carries 1.5.
      final r = BinaryReader(Uint8List(12));
      expect(
        () => r.readF64List(4),
        throwsA(
          isA<StateError>().having(
            (e) => e.message,
            'message',
            contains('frustrate codec: truncated buffer'),
          ),
        ),
      );
    });

    test('a corrupt length is rejected before anything is allocated', () {
      // The element count times the width would overflow a 64-bit int, and
      // `Float64List(n)` for such an n is an out-of-memory abort, not an
      // error a caller can catch. The check has to happen on the count.
      final r = BinaryReader(Uint8List(16));
      expect(
        () => r.readF64List(0x2000000000000000),
        throwsA(
          isA<StateError>().having(
            (e) => e.message,
            'message',
            contains('frustrate codec: truncated buffer'),
          ),
        ),
      );
    });

    test('NaN and the signed zeros survive the bulk round trip', () {
      final xs = Float64List.fromList([
        double.nan,
        0.0,
        -0.0,
        -double.infinity,
      ]);
      final w = BinaryWriter();
      w.writeLen(xs.length);
      w.writeF64List(xs);

      final r = BinaryReader(w.takeBytes());
      final got = r.readF64List(r.readLen());
      r.assertConsumed();
      expect(got[0].isNaN, isTrue);
      expect(got[1], 0.0);
      expect(got[2].isNegative, isTrue, reason: '-0.0 kept its sign');
      expect(got[3], double.negativeInfinity);
    });
  });
}

/// The same integer list written twice — once by the bulk method, once by the
/// per-element loop that shipped before it.
(Uint8List, Uint8List) _both<L extends List<int>>(
  L v,
  void Function(BinaryWriter, L) bulk,
  void Function(BinaryWriter, int) one,
) {
  final a = BinaryWriter();
  bulk(a, v);
  final b = BinaryWriter();
  for (final x in v) {
    one(b, x);
  }
  return (a.takeBytes(), b.takeBytes());
}

/// [_both] for the floating-point lists.
(Uint8List, Uint8List) _bothD<L extends List<double>>(
  L v,
  void Function(BinaryWriter, L) bulk,
  void Function(BinaryWriter, double) one,
) {
  final a = BinaryWriter();
  bulk(a, v);
  final b = BinaryWriter();
  for (final x in v) {
    one(b, x);
  }
  return (a.takeBytes(), b.takeBytes());
}
