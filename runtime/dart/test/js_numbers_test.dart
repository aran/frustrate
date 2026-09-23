/// The JS-number codec arm, checked against the accessors it stands in for.
///
/// **Why this test runs on the VM, which is not the backend it is about.**
/// The arm only executes where `int` is a JavaScript number — dart2js and DDC
/// — and CI runs neither: the browser suite compiles with dart2wasm, where
/// [kJsNumbers] is false and every line below is dead code. Left alone, the
/// one branch whose failure mode is *silently wrong bytes* would be the one
/// branch nothing ever ran.
///
/// So the arm is written as plain functions over a `ByteData` rather than
/// hidden behind the `const` branch, and this calls them directly. That is not
/// a simulation: `%`, `~/`, `-` and the `Uint32`/`Int32` accessors are exact
/// integer operations on the VM *and* on a JS number for every value in range,
/// so the arithmetic executed here is the arithmetic that executes there. What
/// the VM cannot reproduce is the surrounding language — 53-bit `int`, 32-bit
/// bitwise — and nothing here claims to.
///
/// The payoff is a reference the target backend cannot provide: `setInt64` and
/// `getInt64` exist here, so every case below is checked against the real
/// 64-bit encoding byte for byte, rather than against a hand-written
/// expectation that could be wrong in the same direction as the code.
@TestOn('vm')
library;

import 'dart:typed_data';

import 'package:frustrate/src/binary_codec.dart';
import 'package:frustrate/src/js_numbers.dart';
import 'package:test/test.dart';

import 'js_numbers_goldens.dart';

/// The same values the browser half checks, so the two tests cannot drift into
/// covering different things.
final List<int> _corpus = [for (final (v, _) in i64Goldens) v];

Uint8List _reference(int v) {
  final d = ByteData(8)..setInt64(0, v, Endian.little);
  return d.buffer.asUint8List();
}

Uint8List _referenceU(int v) {
  final d = ByteData(8)..setUint64(0, v, Endian.little);
  return d.buffer.asUint8List();
}

void main() {
  group('i64', () {
    test('encodes byte-identically to setInt64 across the corpus', () {
      for (final v in _corpus) {
        final d = ByteData(8);
        jsWriteI64(d, 0, v);
        expect(
          d.buffer.asUint8List(),
          _reference(v),
          reason: 'jsWriteI64 disagrees with setInt64 for $v',
        );
      }
    });

    test('decodes identically to getInt64 across the corpus', () {
      for (final v in _corpus) {
        final d = ByteData.sublistView(_reference(v));
        expect(
          jsReadI64(d, 0),
          v,
          reason: 'jsReadI64 disagrees with getInt64 for $v',
        );
      }
    });

    test('round trips at a non-zero offset', () {
      // The codec always writes at a running cursor, never at 0.
      final d = ByteData(24);
      jsWriteI64(d, 5, -0x1FFFFFFFFF);
      jsWriteI64(d, 13, 0x100000001);
      expect(jsReadI64(d, 5), -0x1FFFFFFFFF);
      expect(jsReadI64(d, 13), 0x100000001);
    });

    test('the halves are in the right order and the high half is signed', () {
      // The two failure modes a same-direction expectation would not catch:
      // swapping the halves, and writing the high half unsigned so every
      // negative value comes back 2^32 too large. Both are caught by the
      // corpus above only because it is checked against setInt64; this states
      // them directly so the intent survives a refactor of the corpus.
      final d = ByteData(8);
      jsWriteI64(d, 0, -1);
      expect(d.getUint32(0, Endian.little), 0xFFFFFFFF, reason: 'low half');
      expect(d.getInt32(4, Endian.little), -1, reason: 'high half, signed');

      jsWriteI64(d, 0, 0x100000002);
      expect(d.getUint32(0, Endian.little), 2, reason: 'low half');
      expect(d.getUint32(4, Endian.little), 1, reason: 'high half');
    });
  });

  group('handles', () {
    test('encode and decode identically to the u64 accessors', () {
      for (final v in [0, 1, 0xFFFFFFFF, 0x100000000, jsMaxExact]) {
        final d = ByteData(8);
        jsWriteHandle(d, 0, v);
        expect(d.buffer.asUint8List(), _referenceU(v));
        expect(jsReadHandle(ByteData.sublistView(_referenceU(v)), 0), v);
      }
    });

    test('a negative handle is rejected as an argument error', () {
      expect(() => jsWriteHandle(ByteData(8), 0, -1), throwsArgumentError);
    });
  });

  group('the range guard', () {
    // The contract that makes this backend usable at all: out of range is
    // loud, never truncated. A silently wrong pointer or length is the one
    // outcome that must be impossible.
    test('rejects writes beyond 2^53, both signs', () {
      expect(
        () => jsWriteI64(ByteData(8), 0, jsMaxExact + 2),
        throwsUnsupportedError,
      );
      expect(
        () => jsWriteI64(ByteData(8), 0, -jsMaxExact - 2),
        throwsUnsupportedError,
      );
      expect(
        () => jsWriteHandle(ByteData(8), 0, jsMaxExact + 2),
        throwsUnsupportedError,
      );
    });

    test('rejects reads beyond 2^53, both signs', () {
      for (final v in [jsMaxExact + 2, -jsMaxExact - 2, 1 << 62, -(1 << 62)]) {
        expect(
          () => jsReadI64(ByteData.sublistView(_reference(v)), 0),
          throwsUnsupportedError,
          reason: 'reading $v must throw, not truncate',
        );
      }
      expect(
        () =>
            jsReadHandle(ByteData.sublistView(_referenceU(jsMaxExact + 2)), 0),
        throwsUnsupportedError,
      );
    });

    test(
      'rejects 2^53 + 1 on read — the value a magnitude check would miss',
      () {
        // 2^53 + 1 has no JS-number representation, so reconstructing it first
        // and then testing `> 2^53` would let it through as exactly 2^53. The
        // guard is on the halves, before reconstruction, precisely for this.
        final bytes = _reference(jsMaxExact + 1);
        expect(
          () => jsReadI64(ByteData.sublistView(bytes), 0),
          throwsUnsupportedError,
        );
        expect(
          () => jsReadHandle(ByteData.sublistView(bytes), 0),
          throwsUnsupportedError,
        );
      },
    );

    test(
      'accepts and rejects at exactly the same boundary in both directions',
      () {
        // The spike's first draft was asymmetric here: it accepted a write of
        // +2^53 and threw reading the same value back. A value that can be
        // written must be readable, or a round trip through Rust fails on the
        // way home for something the caller was allowed to send.
        for (final v in [jsMaxExact, -jsMaxExact]) {
          final d = ByteData(8);
          expect(() => jsWriteI64(d, 0, v), returnsNormally);
          expect(jsReadI64(d, 0), v);
        }
        expect(
          () => jsWriteI64(ByteData(8), 0, jsMaxExact + 2),
          throwsUnsupportedError,
        );
        expect(
          () => jsReadI64(ByteData.sublistView(_reference(jsMaxExact + 2)), 0),
          throwsUnsupportedError,
        );
      },
    );

    test('says what to do about it', () {
      // A developer hitting this is mid-iteration and needs the way out, not
      // a number. Both halves of the remedy have to be in the message.
      Object? caught;
      try {
        jsWriteI64(ByteData(8), 0, jsMaxExact + 2);
      } catch (e) {
        caught = e;
      }
      final message = caught.toString();
      expect(message, contains('development-only'));
      expect(message, contains('--wasm'));
    });
  });

  group('the goldens the browser half depends on', () {
    // dart2js has no setInt64, so the reference it compares against has to be
    // carried in a file. A carried reference can rot; this is the end that
    // can still check it against the real thing.
    test('are still exactly what setInt64 produces', () {
      for (final (v, want) in i64Goldens) {
        expect(_reference(v), want, reason: 'golden bytes for $v have drifted');
      }
    });

    test('decode back through getInt64 to the same values', () {
      for (final (v, bytes) in i64Goldens) {
        expect(
          ByteData.sublistView(Uint8List.fromList(bytes))
              .getInt64(0, Endian.little),
          v,
        );
      }
    });

    test('a Vec<i64> is those goldens concatenated, on both writer arms', () {
      // The list golden is the scalar table end to end rather than a second
      // table, so it cannot drift from what `setInt64` produces — and the
      // browser half asserts the *same* expectation on real dart2js, where
      // only the element arm can run. Here both arms are reachable, which is
      // what makes them comparable at all: the VM copies bytes out of an
      // `Int64List`, and runs the element loop for the plain `List<int>` a
      // generic data class (`Page<T> { items: Vec<T> }` at `T = i64`) hands it.
      final values = [for (final (v, _) in i64Goldens) v];
      final want = [for (final (_, bytes) in i64Goldens) ...bytes];

      final typed = BinaryWriter()..writeI64List(Int64List.fromList(values));
      final untyped = BinaryWriter()..writeI64List(values);
      expect(typed.takeBytes(), want, reason: 'Int64List (bulk arm)');
      expect(untyped.takeBytes(), want, reason: 'List<int> (element arm)');

      final r = BinaryReader(Uint8List.fromList(want));
      expect(r.readI64List(values.length), values);
      r.assertConsumed();
    });

    test('the out-of-range patterns really are out of range', () {
      // Otherwise the browser half would be asserting that in-range values
      // throw, which would pass for entirely the wrong reason.
      for (final bytes in outOfRangeBytes) {
        final v = ByteData.sublistView(Uint8List.fromList(bytes))
            .getInt64(0, Endian.little);
        expect(
          v.abs() > jsMaxExact,
          isTrue,
          reason: '$v is inside the exact range; it would not throw',
        );
      }
    });
  });

  group('the release fence', () {
    // The whole table, because on any single backend at most one row is
    // reachable — the inputs are compile-time facts of whoever is compiling.
    // These are the rows a dropped `!` would swap.
    JsNumberVerdict v(bool js, bool release, bool allowed) => jsNumberVerdict(
      jsNumbers: js,
      releaseMode: release,
      allowedInRelease: allowed,
    );

    test('a 64-bit backend is never touched, whatever the other flags say', () {
      for (final release in [false, true]) {
        for (final allowed in [false, true]) {
          expect(
            v(false, release, allowed),
            JsNumberVerdict.supported,
            reason: 'release=$release allowed=$allowed',
          );
        }
      }
    });

    test('a JS-number release build is refused', () {
      expect(v(true, true, false), JsNumberVerdict.refused);
    });

    test('the opt-in define is the only thing that lifts the refusal', () {
      expect(v(true, true, true), JsNumberVerdict.developmentOnly);
    });

    test('a JS-number development build is allowed, and warned about', () {
      expect(v(true, false, false), JsNumberVerdict.developmentOnly);
      // The opt-in must not turn a development build into a silent one.
      expect(v(true, false, true), JsNumberVerdict.developmentOnly);
    });

    test('both messages carry the way out', () {
      // A developer reading either one is mid-task and needs the remedy, not
      // a diagnosis. The supported escape has to appear in both.
      expect(jsNumberRefusal, contains('--wasm'));
      expect(jsNumberRefusal, contains('frustrate.allowJsNumbers'));
      expect(jsNumberWarning, contains('--wasm'));
      // The warning's whole point is the part the codec cannot fix.
      expect(jsNumberWarning, contains('1 << 62'));
    });
  });

  test('kJsNumbers agrees with the identical(0, 0.0) idiom', () {
    // The arm keys on the compilers' declared variables rather than on how a
    // backend happens to represent numbers. The two must never disagree: if
    // they ever do, one of them is wrong about the platform and the codec
    // would pick an encoding the peer does not expect. Verified by execution
    // on all three backends (VM, dart2js, dart2wasm) when this landed; this
    // pins the VM leg in CI.
    expect(kJsNumbers, identical(0, 0.0));
    expect(kJsNumbers, isFalse, reason: 'the VM has real 64-bit ints');
  });
}
