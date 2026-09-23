/// `BinaryReader` over a JS-backed `Uint8List` decodes identically to one over
/// a Dart-heap list — every method, including every error path.
///
/// **Why this file exists.** On dart2wasm the Dart heap is WasmGC arrays, which
/// JS cannot address, so moving bytes between a Dart-heap `Uint8List` and a JS
/// `ArrayBuffer` costs a JS `for` loop calling an exported wasm function once
/// per byte (SDK `_internal/wasm/lib/js_helper_patch.dart:47-71`). The web
/// transport avoids that by handing the reader a list that is already JS-backed
/// rather than copying into the Dart heap. That is not a hypothetical: the
/// actor path has done it in production all along (`runtime_web.dart`, the
/// `'resp'` case — `JSArrayBuffer.toDart.asUint8List()`, both free wrappers).
///
/// So the reader has two backings on web and only one of them was ever
/// exercised deliberately. `corrupt_envelope_test` and `corrupt_decode_test` do
/// run in the browser, but they build their own Dart-heap `Uint8List`s, so no
/// test in the tree distinguishes the two. This one does, and it does it by
/// *differential* comparison rather than by restating expected values: each
/// case decodes the same bytes twice and requires the two answers to agree,
/// which keeps the assertions honest as the codec changes.
///
/// The backings are not merely two allocators. They dispatch differently in the
/// SDK, in ways that could each go wrong on their own:
///   - `_view.getX` is `ByteData` (a WasmGC load) vs `JSDataViewImpl` (a JS
///     call, with `getBigInt64` + BigInt boxing behind `readI64`/`readHandle`).
///   - `readU8` is an element index, which is a JS call on a JS-backed list.
///   - `readString` picks the decoder by *type*: `_Utf8Decoder.convertSingle`
///     tests `codeUnits is JSUint8ArrayImpl` (`convert_patch.dart:2217`)
///     **before** the `is U8List` branch, so a JS-backed list decodes through
///     `TextDecoder` and a Dart-heap one through the Dart decoder. The
///     malformed-UTF-8 case below is the assertion that matters most in this
///     file: `TextDecoder` is constructed `{fatal: true}`, its throw is caught
///     inside `_useTextDecoder`, which returns null and falls through to the
///     Dart decoder — and only then does the `FormatException` that
///     `readString` re-wraps appear. Three SDK layers have to line up for the
///     codec's own attributable error to survive on web, and nothing else pins
///     that they do.
///
/// Browser-only, and deliberately does **not** initialise the bridge: this is a
/// codec test, not a transport test. It needs no wasm module and no Rust, so it
/// stays fast and cannot be knocked over by an unrelated transport failure.
@TestOn('browser')
library;

import 'dart:js_interop';
import 'dart:typed_data';

import 'package:frustrate/frustrate.dart';
import 'package:test/test.dart';

/// A copy of [src] whose bytes live in a JS `ArrayBuffer`.
///
/// `JSUint8Array.withLength` allocates `new Uint8Array(n)` — offset 0, exact
/// size — and `.toDart` wraps it without copying, so the result is a real
/// `Uint8List` that `is` also a JS typed array.
Uint8List _jsBacked(Uint8List src) {
  final js = JSUint8Array.withLength(src.length);
  final view = js.toDart;
  view.setRange(0, src.length, src);
  return view;
}

/// Whether [x]'s bytes live in a JS `ArrayBuffer`.
///
/// `Uint8List.toJS` **unwraps** a JS-backed list and **clones** a Dart-heap one
/// (SDK `js_interop_patch.dart:442-450`). So writing through the JS side and
/// looking for the write is a public-API probe for the backing — no SDK
/// internals, and it keeps working if the private class names change.
bool _isJsBacked(Uint8List x) {
  if (x.isEmpty) {
    throw ArgumentError('the backing probe needs at least one byte');
  }
  final before = x[0];
  final probe = (before + 1) & 0xff;
  x.toJS.toDart[0] = probe;
  final aliased = x[0] == probe;
  if (aliased) x[0] = before;
  return aliased;
}

/// Decode [bytes] twice — Dart-heap and JS-backed — and require agreement.
///
/// Returns the JS-backed reader's answer so a caller can assert on it further.
/// A throw is part of the comparison: if one backing throws and the other does
/// not, or they throw different messages, that is the failure this file exists
/// to catch.
T _bothAgree<T>(Uint8List bytes, T Function(BinaryReader) read) {
  Object? heapErr;
  T? heapOk;
  try {
    heapOk = read(BinaryReader(bytes));
  } catch (e) {
    heapErr = e;
  }

  Object? jsErr;
  T? jsOk;
  try {
    jsOk = read(BinaryReader(_jsBacked(bytes)));
  } catch (e) {
    jsErr = e;
  }

  if (heapErr != null || jsErr != null) {
    expect(
      jsErr?.toString(),
      heapErr?.toString(),
      reason: 'the two backings disagreed on whether/how the decode fails',
    );
    throw jsErr!;
  }
  expect(jsOk, heapOk, reason: 'the two backings decoded different values');
  return jsOk as T;
}

void main() {
  // The meta-test. Without it every case below could be silently comparing a
  // Dart-heap reader against another Dart-heap reader and passing for free.
  group('the fixture', () {
    test(
      '_jsBacked really produces a JS-backed list, and Uint8List does not',
      () {
        final heap = Uint8List.fromList([1, 2, 3, 4]);
        expect(
          _isJsBacked(heap),
          isFalse,
          reason:
              'Uint8List(n) is a Dart-heap U8List; if this is true the '
              'probe is broken and every other test here is vacuous',
        );
        expect(_isJsBacked(_jsBacked(heap)), isTrue);
      },
    );

    test('_jsBacked copies the bytes faithfully', () {
      final heap = Uint8List.fromList(List.generate(257, (i) => i & 0xff));
      expect(_jsBacked(heap), heap);
    });
  });

  group('scalars decode identically', () {
    test('every integer width, at its extremes', () {
      final w = BinaryWriter()
        ..writeU8(0)
        ..writeU8(255)
        ..writeI8(-128)
        ..writeI8(127)
        ..writeU16(0)
        ..writeU16(65535)
        ..writeI16(-32768)
        ..writeI16(32767)
        ..writeU32(0)
        ..writeU32(4294967295)
        ..writeI32(-2147483648)
        ..writeI32(2147483647)
        // i64 extremes are the interesting ones: on a JS-backed reader this is
        // DataView.getBigInt64 plus a BigInt→int narrowing, on the Dart heap it
        // is a WasmGC i64 load. Nothing else in the suite compares them.
        ..writeI64(-9223372036854775808)
        ..writeI64(9223372036854775807)
        ..writeI64(0)
        ..writeI64(-1);
      final bytes = w.takeBytes();

      _bothAgree(bytes, (r) {
        expect(r.readU8(), 0);
        expect(r.readU8(), 255);
        expect(r.readI8(), -128);
        expect(r.readI8(), 127);
        expect(r.readU16(), 0);
        expect(r.readU16(), 65535);
        expect(r.readI16(), -32768);
        expect(r.readI16(), 32767);
        expect(r.readU32(), 0);
        expect(r.readU32(), 4294967295);
        expect(r.readI32(), -2147483648);
        expect(r.readI32(), 2147483647);
        expect(r.readI64(), -9223372036854775808);
        expect(r.readI64(), 9223372036854775807);
        expect(r.readI64(), 0);
        expect(r.readI64(), -1);
        r.assertConsumed();
        return null;
      });
    });

    test('the two-halves BigInt path: u64, u128, i128', () {
      final u64Max = (BigInt.one << 64) - BigInt.one;
      final u128Max = (BigInt.one << 128) - BigInt.one;
      final i128Min = -(BigInt.one << 127);
      final w = BinaryWriter()
        ..writeU64(BigInt.zero)
        ..writeU64(u64Max)
        ..writeU64(BigInt.one << 63)
        ..writeU128(BigInt.zero)
        ..writeU128(u128Max)
        ..writeI128(i128Min)
        ..writeI128(-BigInt.one)
        ..writeI128(BigInt.one);
      final bytes = w.takeBytes();

      _bothAgree(bytes, (r) {
        expect(r.readU64(), BigInt.zero);
        expect(r.readU64(), u64Max);
        expect(r.readU64(), BigInt.one << 63);
        expect(r.readU128(), BigInt.zero);
        expect(r.readU128(), u128Max);
        expect(r.readI128(), i128Min);
        expect(r.readI128(), -BigInt.one);
        expect(r.readI128(), BigInt.one);
        r.assertConsumed();
        return null;
      });
    });

    test('floats, including the ones that are not equal to themselves', () {
      final w = BinaryWriter()
        ..writeF32(0.0)
        ..writeF32(-0.0)
        ..writeF32(1.5)
        ..writeF64(0.0)
        ..writeF64(-0.0)
        ..writeF64(3.141592653589793)
        ..writeF64(double.infinity)
        ..writeF64(double.negativeInfinity)
        ..writeF64(double.nan);
      final bytes = w.takeBytes();

      _bothAgree(bytes, (r) {
        expect(r.readF32(), 0.0);
        expect(r.readF32().isNegative, isTrue); // -0.0
        expect(r.readF32(), 1.5);
        expect(r.readF64(), 0.0);
        expect(r.readF64().isNegative, isTrue);
        expect(r.readF64(), 3.141592653589793);
        expect(r.readF64(), double.infinity);
        expect(r.readF64(), double.negativeInfinity);
        // NaN never equals itself, so _bothAgree's expect() cannot carry it —
        // assert the predicate here instead.
        expect(r.readF64().isNaN, isTrue);
        r.assertConsumed();
        return null;
      });
    });

    test('bool, char, usize, handle', () {
      final w = BinaryWriter()
        ..writeBool(true)
        ..writeBool(false)
        ..writeChar('a')
        ..writeChar('\u{FFFD}')
        ..writeUsize(0)
        ..writeUsize(1 << 40)
        ..writeHandle(0)
        ..writeHandle(1 << 40);
      final bytes = w.takeBytes();

      _bothAgree(bytes, (r) {
        expect(r.readBool(), isTrue);
        expect(r.readBool(), isFalse);
        expect(r.readChar(), 'a');
        expect(r.readChar(), '\u{FFFD}');
        expect(r.readUsize(), 0);
        expect(r.readUsize(), 1 << 40);
        expect(r.readHandle(), 0);
        expect(r.readHandle(), 1 << 40);
        r.assertConsumed();
        return null;
      });
    });
  });

  group('strings decode identically', () {
    // These are the cases where the two backings run *different decoders*.
    test('ASCII, CJK, emoji, empty, and an interior BOM', () {
      final cases = <String>[
        '',
        'hello',
        'a' * 5000, // long enough to be past any small-input fast path
        '日本語テキスト',
        '👋🏽 family: 👨‍👩‍👧‍👦',
        // Interior, not leading — a *leading* U+FEFF is lost, see the
        // dedicated test below.
        'an interior \u{FEFF} must survive as content',
        'mixed ascii and 日本語 and 👋 in one string',
      ];
      for (final s in cases) {
        final bytes = (BinaryWriter()..writeString(s)).takeBytes();
        expect(
          _bothAgree(bytes, (r) {
            final v = r.readString();
            r.assertConsumed();
            return v;
          }),
          s,
          reason: 'round trip failed for: ${s.length} code units',
        );
      }
    });

    test('a leading BOM is content and survives', () {
      // Dart's UTF-8 decoder strips a byte-order mark from the start of its
      // input — a document convention that does not apply to a length-prefixed
      // wire field. `writeString` emits EF BB BF faithfully, so without the
      // skip-and-restore in `readString` this direction is silently lossy and
      // the other is not. Both backings strip for their own reasons (the VM's
      // decoder, and `TextDecoder` on dart2wasm), so no config is a workaround
      // and agreement between the two would not have caught it.
      for (final sent in [
        '\u{FEFF}',
        '\u{FEFF}leading BOM',
        '\u{FEFF}\u{FEFF}two of them',
        '\u{FEFF}日本語',
      ]) {
        final bytes = (BinaryWriter()..writeString(sent)).takeBytes();
        expect(bytes.sublist(8, 11), [
          0xEF,
          0xBB,
          0xBF,
        ], reason: 'the encoder must emit the mark');
        expect(
          _bothAgree(bytes, (r) {
            final v = r.readString();
            r.assertConsumed();
            return v;
          }),
          sent,
          reason: 'leading U+FEFF lost for a ${sent.length}-unit string',
        );
      }
    });

    test('malformed UTF-8 raises the codec error, not the decoder error', () {
      // A length prefix followed by bytes that are not valid UTF-8. On the
      // JS-backed reader this must traverse TextDecoder(fatal:true) → throw →
      // caught in _useTextDecoder → null → Dart decoder → FormatException →
      // readString's re-wrap. If any link changes, this is what notices.
      final w = BinaryWriter()..writeLen(3);
      w.writeU8(0xff);
      w.writeU8(0xfe);
      w.writeU8(0xfd);
      final bytes = w.takeBytes();

      expect(
        () => _bothAgree(bytes, (r) => r.readString()),
        throwsA(
          isA<StateError>().having(
            (e) => e.message,
            'message',
            allOf(
              contains('invalid UTF-8 in string field'),
              contains('offset'),
            ),
          ),
        ),
      );
    });

    test('a truncated string length is a codec error on both backings', () {
      final w = BinaryWriter()..writeLen(64); // claims 64 bytes, supplies 2
      w.writeU8(1);
      w.writeU8(2);
      final bytes = w.takeBytes();

      expect(
        () => _bothAgree(bytes, (r) => r.readString()),
        throwsA(
          isA<StateError>().having(
            (e) => e.message,
            'message',
            contains('truncated buffer'),
          ),
        ),
      );
    });
  });

  group('byte fields', () {
    test('readBytes and readByteArray decode identically', () {
      final payload = Uint8List.fromList(List.generate(1000, (i) => i & 0xff));
      final head = Uint8List.fromList(List.generate(32, (i) => 255 - i));
      final w = BinaryWriter()
        ..writeBytes(payload)
        ..writeByteArray(head, 32)
        ..writeBytes(Uint8List(0));
      final bytes = w.takeBytes();

      _bothAgree(bytes, (r) {
        expect(r.readBytes(), payload);
        expect(r.readByteArray(32), head);
        expect(r.readBytes(), isEmpty);
        r.assertConsumed();
        return null;
      });
    });

    test('a byte field keeps its reader\'s backing', () {
      // This is the property the web transport relies on: `readBytes` is a
      // `sublistView`, so a JS-backed reader yields JS-backed byte fields all
      // the way out to the caller — no per-byte crossing anywhere on the path.
      final payload = Uint8List.fromList(List.generate(64, (i) => i + 1));
      final bytes = (BinaryWriter()..writeBytes(payload)).takeBytes();

      expect(_isJsBacked(BinaryReader(bytes).readBytes()), isFalse);
      expect(_isJsBacked(BinaryReader(_jsBacked(bytes)).readBytes()), isTrue);
    });
  });

  group('error paths agree', () {
    test('an invalid bool byte', () {
      final bytes = (BinaryWriter()..writeU8(2)).takeBytes();
      expect(
        () => _bothAgree(bytes, (r) => r.readBool()),
        throwsA(
          isA<StateError>().having(
            (e) => e.message,
            'message',
            contains('invalid bool byte 2'),
          ),
        ),
      );
    });

    test('a lone surrogate in a char field', () {
      final bytes = (BinaryWriter()..writeU32(0xD800)).takeBytes();
      expect(
        () => _bothAgree(bytes, (r) => r.readChar()),
        throwsA(
          isA<StateError>().having(
            (e) => e.message,
            'message',
            contains('invalid Unicode scalar value U+D800'),
          ),
        ),
      );
    });

    test('a negative usize', () {
      final bytes = (BinaryWriter()..writeI64(-1)).takeBytes();
      expect(
        () => _bothAgree(bytes, (r) => r.readUsize()),
        throwsA(
          isA<StateError>().having(
            (e) => e.message,
            'message',
            contains('negative usize'),
          ),
        ),
      );
    });

    test('reading past the end', () {
      final bytes = (BinaryWriter()..writeU8(7)).takeBytes();
      expect(
        () => _bothAgree(bytes, (r) {
          r.readU8();
          return r.readI64();
        }),
        throwsA(
          isA<StateError>().having(
            (e) => e.message,
            'message',
            contains('truncated buffer'),
          ),
        ),
      );
    });

    test('trailing bytes are rejected by assertConsumed', () {
      final bytes =
          (BinaryWriter()
                ..writeI64(1)
                ..writeU8(9))
              .takeBytes();
      expect(
        () => _bothAgree(bytes, (r) {
          r.readI64();
          r.assertConsumed();
          return null;
        }),
        throwsA(
          isA<StateError>().having(
            (e) => e.message,
            'message',
            contains('1 trailing byte(s)'),
          ),
        ),
      );
    });
  });
}
