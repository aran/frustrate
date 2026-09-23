/// Cross-language integration tests: real Dart, real Rust, both platforms.
///
/// Native prerequisite: `cargo build -p test_api` (the build script
/// regenerates the bindings imported below). Run with `dart test`.
///
/// Web prerequisite: `dart run tool/build_web_fixture.dart` (builds and
/// stages the wasm module). Run with `dart test -p chrome -c dart2wasm`.
/// Platform-divergent semantics are pinned by the testOn-gated tests at the
/// bottom of the Locked group.
library;

import 'dart:async';
import 'dart:typed_data';

import 'package:frustrate/frustrate.dart';
import 'package:frustrate_integration/fake_plan.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

void main() {
  setUpAll(initBridge);

  // No test may leave a Rust→Dart registration open: an open stream/callback
  // pins the isolate, and a suite that leaks one hangs at
  // process exit. A stored sink/callback needs an explicit dispose — and while
  // a collected-but-undisposed handle now reports itself (LeakedChannelError),
  // that report needs a collection to happen, which an
  // idle isolate never provides. This assertion is the half that does not
  // depend on the collector, so it stays the primary guard.
  // Native-only: on web the page is always alive (no keepIsolateAlive) and
  // retirement timing differs, so the guard would misfire. Settle async
  // drop-retire first, then assert the ledger is flat.
  tearDown(() async {
    if (!isNativeVm) return;
    for (var i = 0; i < 200 && Frustrate.instance.openChannelCount != 0; i++) {
      await Future<void>.delayed(const Duration(milliseconds: 5));
    }
    expect(
      Frustrate.instance.openChannelCount,
      0,
      reason:
          'test leaked an open channel — dispose stored sinks/callbacks '
          '(would pin the isolate). Still open: '
          '${Frustrate.instance.openChannelLabels}',
    );
  });

  group('scalars and strings', () {
    test('sync i32 add', () {
      expect(addI32(a: 2, b: 40), 42);
      expect(addI32(a: -7, b: 7), 0);
    });

    test('every scalar crosses intact', () {
      final s = mixScalars(
        b: true,
        x8: -8,
        x16: -1600,
        x32: -320000,
        x64: -64000000000,
        u8v: 200,
        u16v: 65000,
        u32v: 4000000000,
        f32v: 1.5,
        f64v: 2.25,
        us: 12345,
        is_: -54321,
        u64v: BigInt.parse('18446744073709551615'),
      );
      expect(
        s,
        'true|-8|-1600|-320000|-64000000000|200|65000|4000000000|1.5|2.25|12345|-54321|18446744073709551615',
      );
    });

    test('unicode strings round trip (async)', () async {
      final joined = await concatStrings(
        parts: ['héllo', '👨‍👩‍👧‍👦', 'мир'],
        sep: ' ~ ',
      );
      expect(joined, 'héllo ~ 👨‍👩‍👧‍👦 ~ мир');
    });

    test('empty call', () {
      noArgsNoRet();
    });
  });

  group('adversarial values', () {
    test('float special values survive exactly (NaN, ±Inf, -0.0)', () {
      expect(echoF64(x: double.nan).isNaN, isTrue);
      expect(echoF64(x: double.infinity), double.infinity);
      expect(echoF64(x: double.negativeInfinity), double.negativeInfinity);
      // -0.0 == 0.0 numerically, so distinguish it by the sign of 1/x.
      final negZero = echoF64(x: -0.0);
      expect(negZero, 0.0);
      expect(
        1 / negZero,
        double.negativeInfinity,
        reason: 'the negative sign of -0.0 must survive the round trip',
      );
      expect(1 / echoF64(x: 0.0), double.infinity);
    });

    test('f32 precision: exact values survive, the 24-bit limit rounds', () {
      // Exactly representable in f32 → bit-identical back.
      expect(echoF32(x: 0.5), 0.5);
      expect(echoF32(x: 0.25), 0.25);
      expect(echoF32(x: -0.0), 0.0);
      expect(1 / echoF32(x: -0.0), double.negativeInfinity);
      expect(echoF32(x: double.infinity), double.infinity);
      expect(echoF32(x: double.nan).isNaN, isTrue);
      // 2^24 + 1 is the first integer f32 cannot represent — it rounds to
      // 2^24, proving the value crossed as a genuine 32-bit float, not an f64.
      expect(echoF32(x: 16777217.0), 16777216.0);
    });

    test('float edge values inside a collection', () {
      final out = echoF64s(
        xs: Float64List.fromList([
          double.nan,
          double.infinity,
          double.negativeInfinity,
          -0.0,
          3.5,
        ]),
      );
      expect(out, isA<Float64List>());
      expect(out[0].isNaN, isTrue);
      expect(out[1], double.infinity);
      expect(out[2], double.negativeInfinity);
      expect(1 / out[3], double.negativeInfinity);
      expect(out[4], 3.5);
    });

    test('i64 extremes round-trip (Dart→Rust→Dart)', () {
      expect(
        echoI64(x: -9223372036854775808),
        -9223372036854775808,
      ); // i64::MIN
      expect(echoI64(x: 9223372036854775807), 9223372036854775807); // i64::MAX
      expect(echoI64(x: 0), 0);
    });

    test('unsigned extremes round-trip as arguments', () {
      expect(
        echoU64(x: BigInt.parse('18446744073709551615')),
        BigInt.parse('18446744073709551615'),
      ); // u64::MAX
      expect(echoU64(x: BigInt.zero), BigInt.zero);
      expect(echoU32(x: 4294967295), 4294967295); // u32::MAX
      expect(echoU32(x: 0), 0);
    });

    test('integer extremes minted Rust-side (return direction)', () {
      final e = intExtremes();
      expect(e.i64Min, -9223372036854775808);
      expect(e.i64Max, 9223372036854775807);
      expect(e.u32Max, 4294967295);
      expect(e.u64Max, BigInt.parse('18446744073709551615'));
    });

    test('empty String / Vec<u8> / Vec<T> arguments cross', () {
      expect(echoString(s: ''), '');
      expect(echoBytes(data: Uint8List(0)), isEmpty);
      expect(echoI64s(xs: Int64List(0)), isEmpty);
      // And a populated one for contrast, through the same path.
      expect(echoI64s(xs: Int64List.fromList([1, -2, 3])), [1, -2, 3]);
    });

    test('lone UTF-16 surrogate: encoder replaces it (lossy, not corrupt)', () {
      // A Dart String is UTF-16 and may hold an unpaired surrogate; Rust
      // `String` is UTF-8, which cannot represent one. FINDING: the Dart
      // encoder (utf8.encode) substitutes U+FFFD REPLACEMENT CHARACTER rather
      // than throwing or emitting raw bytes — so the crossing is lossy but
      // safe (no memory corruption, no invalid UTF-8 reaching Rust). A well-
      // formed surrogate PAIR (a real emoji) is unaffected.
      const loneHigh = '\uD800'; // unpaired high surrogate
      expect(
        stringUtf8Len(s: loneHigh),
        3,
        reason: 'U+FFFD encodes to 3 UTF-8 bytes',
      );
      expect(echoString(s: loneHigh), '�');
      // A valid surrogate pair (👍 = U+1F44D) survives intact — 4 UTF-8 bytes.
      const thumbsUp = '\u{1F44D}';
      expect(stringUtf8Len(s: thumbsUp), 4);
      expect(echoString(s: thumbsUp), thumbsUp);
    });

    test('a leading U+FEFF is content, not a byte-order mark', () {
      // Dart's UTF-8 decoder strips a BOM from the start of its input. That is
      // a document convention and it does not apply to a length-prefixed wire
      // field, where U+FEFF (ZERO WIDTH NO-BREAK SPACE) is ordinary content —
      // e.g. a Rust string carrying one through from a file or a network body.
      //
      // The asymmetry is what makes it dangerous: `utf8.encode` emits EF BB BF
      // faithfully, so Dart -> Rust is lossless and only the return leg drops
      // it. `stringUtf8Len` measures what Rust actually received, which is why
      // it is asserted alongside — it separates a lossy encoder from a lossy
      // decoder, and the encoder is not the problem.
      const bom = '\u{FEFF}';
      expect(stringUtf8Len(s: bom), 3, reason: 'Rust must receive the mark');
      expect(echoString(s: bom), bom);
      expect(stringUtf8Len(s: '${bom}x'), 4);
      expect(echoString(s: '${bom}x'), '${bom}x');
      // Two marks: the decoder removes at most one, so restoring exactly one
      // would leave this case short.
      expect(echoString(s: '$bom$bom'), '$bom$bom');
      // Interior marks were never affected; asserted so a future fix that
      // over-corrects by scanning the whole string is caught here.
      expect(echoString(s: 'a${bom}b'), 'a${bom}b');
    });
  });

  group('time mappings', () {
    test('Duration round-trips as microseconds', () {
      final sum = addDuration(
        a: const Duration(milliseconds: 1500),
        b: const Duration(microseconds: 250),
      );
      expect(sum, const Duration(milliseconds: 1500, microseconds: 250));
      expect(sum.inMicroseconds, 1500250);
    });

    test('SystemTime round-trips as a UTC DateTime', () {
      // Microsecond granularity on purpose, on every platform. dart2wasm
      // patches `dart:core`'s DateTime with the *VM's* implementation
      // (libraries.json "wasm" -> vm_shared/lib/date_patch.dart), whose
      // `_value` is microseconds since the epoch, and dart2js/DDC carry a
      // separate 0..999 `_microsecond` field beside a millisecond `_value`.
      // So the sub-millisecond digits this asserts are load-bearing
      // everywhere, and a mapping that quietly rounded to milliseconds would
      // fail here rather than pass on a technicality.
      final t = DateTime.utc(2020, 6, 15, 12, 30, 45, 123, 456);
      final later = advanceTime(t: t, by: const Duration(seconds: 10));
      expect(later.isUtc, isTrue);
      expect(later, DateTime.utc(2020, 6, 15, 12, 30, 55, 123, 456));
      expect(later.microsecond, 456);
    });

    // A pre-epoch instant is where the *std* peer's range stops being the
    // wire's, and it stops in a platform-divergent place: on the VM
    // `SystemTime` is a signed offset and 1969 round-trips, while on
    // wasm32-unknown-unknown std's `SystemTime` is the unsupported PAL's
    // unsigned `Duration` from the epoch and cannot hold it at all. That used
    // to surface as `overflow when subtracting duration from instant` raised
    // from inside library/std — loud, but naming neither the bridge nor a fix.
    // It is a named codec contract now, and the peers that *do* span the epoch
    // on every target are the ones the message points at. Pinned on both
    // platforms because the divergence is the point.
    test('a pre-epoch DateTime crosses as negative microseconds', () {
      final t = DateTime.utc(1969, 7, 20, 20, 17, 40);
      expect(t.microsecondsSinceEpoch, isNegative);
      final later = advanceTime(t: t, by: const Duration(hours: 1));
      expect(later, DateTime.utc(1969, 7, 20, 21, 17, 40));
    }, testOn: 'vm');

    test('a pre-epoch DateTime is refused attributably on web', () {
      expect(
        () => advanceTime(
          t: DateTime.utc(1969, 7, 20),
          by: const Duration(hours: 1),
        ),
        throwsA(
          isA<BridgePanicException>().having(
            (e) => e.toString(),
            'message',
            allOf(contains('pre-epoch'), contains('chrono::DateTime')),
          ),
        ),
      );
    }, testOn: 'browser');

    // §1 claims the time mappings compose into every container. Neither is on
    // the bulk numeric fast path, so `Vec<SystemTime>`/`Vec<Duration>` take
    // the generic per-element codec and `Option<SystemTime>` takes the
    // null-flag arm — three arms nothing had ever crossed the boundary to
    // check, in either direction.
    test('time types compose into lists and options', () {
      final base = DateTime.utc(2020, 1, 1, 0, 0, 0, 0, 7);
      final got = earliestAfter(
        stamps: [base, base.add(const Duration(days: 1))],
        spans: const [Duration(hours: 1), Duration(microseconds: 3)],
        floor: null,
      );
      expect(got, DateTime.utc(2020, 1, 1, 1, 0, 0, 0, 10));

      // The Option arm both ways: a non-null floor that excludes the earlier
      // stamp, and an empty result coming back as null.
      // (The 7µs rides along, which is the composition claim doing its job.)
      expect(
        earliestAfter(
          stamps: [base, base.add(const Duration(days: 1))],
          spans: const [],
          floor: DateTime.utc(2020, 1, 1, 12),
        ),
        DateTime.utc(2020, 1, 2, 0, 0, 0, 0, 7),
      );
      expect(
        earliestAfter(
          stamps: [base],
          spans: const [],
          floor: DateTime.utc(2021),
        ),
        isNull,
      );
    });

    // Dart's Duration is signed; `std::time::Duration` is not. The two are
    // therefore not the same set of values, and reinterpreting -5µs as an
    // unsigned count produced 584542 years on the Rust side — a plausible
    // value nothing downstream would question. It is refused at the boundary
    // now, and the message names the peer types that accept a negative span,
    // because declaring one is the fix when the value is legitimately signed.
    test('a negative Duration is refused, naming the signed peers', () {
      expect(
        () =>
            addDuration(a: const Duration(microseconds: -1), b: Duration.zero),
        throwsA(
          isA<ArgumentError>().having(
            (e) => e.message.toString(),
            'message',
            allOf(contains('unsigned'), contains('chrono::Duration')),
          ),
        ),
      );
      // The Rust-side backstop for the same contract is generated too (a
      // `u64::try_from` that panics attributably); this is the near end.
      expect(addDuration(a: Duration.zero, b: Duration.zero), Duration.zero);
    });
  });

  group('u64 codec (cross-language golden vectors)', () {
    // Pure codec tests — no bridge involved, so they ride every platform
    // and web build. The byte vectors are pinned identically on the Rust side
    // (codec.rs u64_wire_bytes).
    test('wire bytes match the Rust goldens', () {
      final w = BinaryWriter();
      w.writeU64(BigInt.parse('0123456789ABCDEF', radix: 16));
      w.writeU64(BigInt.parse('FFFFFFFFFFFFFFFF', radix: 16));
      expect(w.takeBytes(), [
        0xEF, 0xCD, 0xAB, 0x89, 0x67, 0x45, 0x23, 0x01, //
        0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
      ]);
    });

    test('round trips across the full range', () {
      final values = [
        BigInt.zero,
        BigInt.one,
        BigInt.one << 63,
        (BigInt.one << 64) - BigInt.one,
      ];
      final w = BinaryWriter();
      for (final v in values) {
        w.writeU64(v);
      }
      final r = BinaryReader(w.takeBytes());
      for (final v in values) {
        expect(r.readU64(), v);
      }
      expect(r.isAtEnd, isTrue);
    });

    test('out-of-range values throw before crossing', () {
      final w = BinaryWriter();
      expect(() => w.writeU64(BigInt.from(-1)), throwsArgumentError);
      expect(() => w.writeU64(BigInt.one << 64), throwsArgumentError);
    });
  });

  group('u64 (BigInt)', () {
    test('extremes above 2^63 arrive intact', () {
      expect(u64Extremes(), [
        BigInt.zero,
        BigInt.one,
        BigInt.parse('9223372036854775808'),
        BigInt.parse('18446744073709551615'),
      ]);
    });

    test('BigInt map keys look up by value', () {
      final ids = {
        BigInt.two: 'two',
        BigInt.parse('18446744073709551615'): 'max',
      };
      expect(
        nameById(ids: ids, key: BigInt.parse('18446744073709551615')),
        'max',
      );
      expect(nameById(ids: ids, key: BigInt.from(3)), isNull);
    });

    test('option composition', () {
      expect(
        bumpU64(x: BigInt.parse('18446744073709551614')),
        BigInt.parse('18446744073709551615'),
      );
      expect(bumpU64(x: null), isNull);
    });

    test('out-of-range argument throws before crossing', () {
      expect(() => bumpU64(x: BigInt.from(-1)), throwsArgumentError);
      expect(() => bumpU64(x: BigInt.one << 64), throwsArgumentError);
    });
  });

  group('char codec (cross-language golden vectors)', () {
    // Pure codec test — no bridge. Pins the same bytes as codec.rs
    // char_wire_bytes: a `char` rides the u32 wire (LE codepoint).
    test('wire bytes match the Rust goldens', () {
      final w = BinaryWriter();
      w.writeChar('A'); // U+0041
      w.writeChar('é'); // U+00E9
      w.writeChar('🦀'); // U+1F980
      expect(w.takeBytes(), [
        0x41, 0x00, 0x00, 0x00, //
        0xE9, 0x00, 0x00, 0x00, //
        0x80, 0xF9, 0x01, 0x00,
      ]);
    });

    test('round trips including an astral scalar', () {
      const values = [' ', 'A', 'é', '🦀', '\u{10FFFF}'];
      final w = BinaryWriter();
      for (final v in values) {
        w.writeChar(v);
      }
      final r = BinaryReader(w.takeBytes());
      for (final v in values) {
        expect(r.readChar(), v);
      }
      expect(r.isAtEnd, isTrue);
    });

    test('non-single-scalar strings throw before crossing', () {
      final w = BinaryWriter();
      expect(() => w.writeChar(''), throwsArgumentError); // empty
      expect(() => w.writeChar('ab'), throwsArgumentError); // multi-char
      expect(() => w.writeChar('a🦀'), throwsArgumentError); // multi (astral)
      expect(
        () => w.writeChar('\uD800'),
        throwsArgumentError,
      ); // lone surrogate
    });

    test('decode rejects a lone surrogate / out-of-range codepoint loudly', () {
      // 0xD800 (lone surrogate) and 0x110000 (past U+10FFFF) are not scalars.
      expect(
        () =>
            BinaryReader(Uint8List.fromList([0x00, 0xD8, 0x00, 0x00]))
                .readChar(),
        throwsStateError,
      );
      expect(
        () =>
            BinaryReader(Uint8List.fromList([0x00, 0x00, 0x11, 0x00]))
                .readChar(),
        throwsStateError,
      );
    });
  });

  group('char (String)', () {
    test('round-trips across ASCII, BMP, and astral scalars', () {
      expect(echoChar(c: 'A'), 'A');
      expect(echoChar(c: 'é'), 'é');
      expect(
        echoChar(c: '🦀'),
        '🦀',
      ); // U+1F980, astral (surrogate pair in Dart)
      expect(echoChar(c: ' '), ' ');
    });

    test('astral char minted Rust-side reconstructs as a surrogate pair', () {
      expect(crab(), '🦀');
      expect(crab().runes.single, 0x1F980);
    });

    test('non-single-scalar argument throws before crossing', () {
      expect(() => echoChar(c: ''), throwsArgumentError);
      expect(() => echoChar(c: 'ab'), throwsArgumentError);
      expect(
        () => echoChar(c: '\uD800'),
        throwsArgumentError,
      ); // lone surrogate
    });
  });

  group('i128/u128 codec (cross-language golden vectors)', () {
    // Pure codec test — pins the same bytes as codec.rs i128_u128_wire_bytes.
    // 16 LE bytes = the u64 codec extended (two u64 halves), byte-identical to
    // Rust's to_le_bytes().
    test('wire bytes match the Rust goldens', () {
      final w = BinaryWriter();
      w.writeU128(BigInt.parse('0F0E0D0C0B0A09080706050403020100', radix: 16));
      w.writeI128(BigInt.from(-1));
      w.writeI128(-(BigInt.one << 127)); // i128::MIN
      expect(w.takeBytes(), [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, //
        0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, //
        0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, //
        0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80,
      ]);
    });

    test('i128/u128 round trip across the full range', () {
      final i128Min = -(BigInt.one << 127);
      final i128Max = (BigInt.one << 127) - BigInt.one;
      final u128Max = (BigInt.one << 128) - BigInt.one;
      final signed = [i128Min, i128Max, BigInt.from(-1), BigInt.zero];
      final unsigned = [BigInt.zero, BigInt.one, BigInt.one << 100, u128Max];
      final w = BinaryWriter();
      for (final v in signed) {
        w.writeI128(v);
      }
      for (final v in unsigned) {
        w.writeU128(v);
      }
      final r = BinaryReader(w.takeBytes());
      for (final v in signed) {
        expect(r.readI128(), v);
      }
      for (final v in unsigned) {
        expect(r.readU128(), v);
      }
      expect(r.isAtEnd, isTrue);
    });

    test('out-of-range values throw before crossing', () {
      final w = BinaryWriter();
      expect(() => w.writeU128(BigInt.from(-1)), throwsArgumentError);
      expect(() => w.writeU128(BigInt.one << 128), throwsArgumentError);
      expect(
        () => w.writeI128(BigInt.one << 127),
        throwsArgumentError,
      ); // 2^127
      expect(
        () => w.writeI128(-(BigInt.one << 127) - BigInt.one),
        throwsArgumentError,
      ); // i128::MIN - 1
    });
  });

  group('i128/u128 (BigInt)', () {
    final i128Min = -(BigInt.one << 127);
    final i128Max = (BigInt.one << 127) - BigInt.one;
    final u128Max = (BigInt.one << 128) - BigInt.one;

    test('signed extremes round-trip (Dart→Rust→Dart)', () {
      expect(echoI128(x: i128Min), i128Min);
      expect(echoI128(x: i128Max), i128Max);
      expect(echoI128(x: BigInt.from(-1)), BigInt.from(-1));
      expect(echoI128(x: BigInt.zero), BigInt.zero);
    });

    test('unsigned extremes round-trip (Dart→Rust→Dart)', () {
      expect(echoU128(x: u128Max), u128Max);
      expect(echoU128(x: BigInt.one << 100), BigInt.one << 100);
      expect(echoU128(x: BigInt.zero), BigInt.zero);
    });

    test('128-bit extremes minted Rust-side (return direction)', () {
      final e = int128Extremes();
      expect(e.i128Min, i128Min);
      expect(e.i128Max, i128Max);
      expect(e.u128Max, u128Max);
    });

    test('out-of-range argument throws before crossing', () {
      expect(() => echoU128(x: BigInt.from(-1)), throwsArgumentError);
      expect(() => echoI128(x: BigInt.one << 127), throwsArgumentError);
    });
  });

  group('bytes and arrays', () {
    test('bytes round trip', () {
      final out = revBytes(data: Uint8List.fromList([1, 2, 3, 250]));
      expect(out, [250, 3, 2, 1]);
    });

    test('immutable &[u8] slice param', () {
      // A &[u8] param crosses as Uint8List, identically to Vec<u8>.
      expect(sumBytes(data: Uint8List.fromList([1, 2, 3, 4])), 10);
      expect(sumBytes(data: Uint8List(0)), 0);
    });

    test('fixed 32-byte array', () {
      final head = Uint8List.fromList(List.generate(32, (i) => i));
      final rev = revHash(head: head);
      expect(rev.length, 32);
      expect(rev.first, 31);
      expect(rev.last, 0);
    });

    test('wrong fixed-array length throws before crossing', () {
      expect(() => revHash(head: Uint8List(31)), throwsArgumentError);
    });

    test('`[T; N]` round-trips for every element type', () {
      final t = Transform(
        matrix: Float32List.fromList(List.generate(16, (i) => i * 0.5)),
        ids: Int32List.fromList([-1, 0, 1, 2147483647]),
        longs: Int64List.fromList([-9223372036854775808, 9223372036854775807]),
        corners: const ['tl', '', 'br'],
        bounds: const [
          Point(x: 1.5, y: -2.5, label: 'a'),
          Point(x: 0, y: 0, label: null),
        ],
        digest: Uint8List.fromList(List.generate(8, (i) => 200 + i)),
        grid: [
          Int32List.fromList([1, 2]),
          Int32List.fromList([3, 4]),
          Int32List.fromList([5, 6]),
        ],
        flags: const [7, null],
        none: Float64List(0),
      );
      final back = echoTransform(t: t);
      // Distinct values in every slot, so an array built out of index order
      // would fail here rather than pass by symmetry.
      expect(back.matrix, t.matrix);
      expect(back.ids, t.ids);
      expect(back.longs, t.longs);
      expect(back.corners, ['tl', '', 'br']);
      expect(back.bounds.map((p) => (p.x, p.y, p.label)), [
        (1.5, -2.5, 'a'),
        (0.0, 0.0, null),
      ]);
      expect(back.digest, t.digest);
      expect(back.grid.map((r) => r.toList()), [
        [1, 2],
        [3, 4],
        [5, 6],
      ]);
      expect(back.flags, [7, null]);
      expect(back.none, isEmpty);
      // A `[T; N]` decodes fixed-length: the value's length is its type.
      expect(() => back.corners.add('x'), throwsUnsupportedError);
    });

    test('a wrong-length `[T; N]` throws before crossing, at every depth', () {
      expect(() => matrixTrace(m: Float32List(15)), throwsArgumentError);
      expect(matrixTrace(m: Float32List.fromList(List.filled(16, 2.0))), 8.0);
    });
  });

  group('collections and options', () {
    test('option some/none', () {
      expect(maybeDouble(x: 21.0), 42.0);
      expect(maybeDouble(x: null), isNull);
    });

    test('map round trip', () {
      final inverted = invertMap(m: {'a': 1, 'b': 2});
      expect(inverted, {1: 'a', 2: 'b'});
      expect(invertMap(m: {}), isEmpty);
    });

    test('HashSet round-trips as a Dart Set', () {
      final u = unionSets(a: {1, 2, 3}, b: {3, 4});
      expect(u, isA<Set<int>>());
      expect(u, {1, 2, 3, 4});
      expect(unionSets(a: <int>{}, b: <int>{}), isEmpty);
    });

    test('nested Option<Option<T>> keeps all three shapes distinct', () {
      // outer None -> null; Some(None) -> FrNone; Some(Some(v)) -> FrSome(v).
      // The three must not collapse into one another (the footgun a plain
      // `int??` would create).
      expect(nestOpt(x: null), isNull);

      final someNone = nestOpt(x: const FrNone<int>());
      expect(someNone, isA<FrNone<int>>());

      final someSome = nestOpt(x: const FrSome<int>(7));
      expect(someSome, isA<FrSome<int>>());
      expect((someSome as FrSome<int>).value, 7);

      // A negative and a zero payload round-trip too (guards the presence
      // byte vs value ordering).
      expect((nestOpt(x: const FrSome<int>(0)) as FrSome<int>).value, 0);
      expect((nestOpt(x: const FrSome<int>(-9)) as FrSome<int>).value, -9);
    });
  });

  group('nested generics (composable surface)', () {
    test('Vec<Option<T>> <-> List<int?>', () {
      expect(echoVecOpt(xs: [1, null, 3, null]), [1, null, 3, null]);
      expect(echoVecOpt(xs: <int?>[]), isEmpty);
    });

    test('Option<Vec<i64>> <-> Int64List?', () {
      final out = echoOptVec(x: Int64List.fromList([1, 2, 3]));
      expect(out, isA<Int64List>());
      expect(out, [1, 2, 3]);
      expect(echoOptVec(x: null), isNull);
      expect(
        echoOptVec(x: Int64List(0)),
        isEmpty,
      ); // Some(empty) distinct from None
    });

    test('HashMap<String, Vec<i64>> <-> Map<String, Int64List>', () {
      final m = {
        'a': Int64List.fromList([1, 2]),
        'b': Int64List(0),
        'c': Int64List.fromList([9]),
      };
      final out = echoMapOfVec(m: m);
      expect(out['a'], isA<Int64List>());
      expect(out, m);
      expect(echoMapOfVec(m: <String, Int64List>{}), isEmpty);
    });

    test('Vec<Vec<i64>> <-> List<Int64List>, round-trip and transpose', () {
      final grid = [
        Int64List.fromList([1, 2, 3]),
        Int64List.fromList([4, 5, 6]),
      ];
      final out = echoVecOfVec(xss: grid);
      expect(out, isA<List<Int64List>>());
      expect(out[0], isA<Int64List>());
      expect(out, grid);
      expect(echoVecOfVec(xss: <Int64List>[]), isEmpty);
      // Semantic transform: a byte-passthrough could not produce this.
      expect(transpose(grid: grid), [
        [1, 4],
        [2, 5],
        [3, 6],
      ]);
    });

    test('Vec<HashMap<K,V>> <-> List<Map<String, int>>', () {
      final ms = [
        {'x': 1, 'y': 2},
        <String, int>{},
        {'z': 9},
      ];
      expect(echoVecOfMap(ms: ms), ms);
    });

    test('Vec<Option<Option<T>>>: FrOption composes inside a collection', () {
      // Element type List<FrOption<int>?>: outer None -> null,
      // Some(None) -> FrNone, Some(Some(v)) -> FrSome(v), all distinct.
      final out = echoVecNestedOpt(
        xs: [null, const FrNone<int>(), const FrSome<int>(5), null],
      );
      expect(out, hasLength(4));
      expect(out[0], isNull);
      expect(out[1], isA<FrNone<int>>());
      expect((out[2] as FrSome<int>).value, 5);
      expect(out[3], isNull);
    });

    test('struct carrying nested-generic fields round-trips', () {
      final n = Nested(
        tags: ['a', null, 'c'],
        groups: {
          'g1': Int64List.fromList([1, 2]),
          'g2': Int64List(0),
        },
        grid: [
          Int64List.fromList([1, 2]),
          Int64List.fromList([3, 4]),
        ],
      );
      final back = echoNested(n: n);
      expect(back.tags, ['a', null, 'c']);
      expect(back.groups, {
        'g1': [1, 2],
        'g2': <int>[],
      });
      expect(back.grid, [
        [1, 2],
        [3, 4],
      ]);
      // Deep value equality covers the whole nested shape at once.
      expect(back, n);
    });

    test('data enum variants carrying nested generics round-trip', () {
      final rows = echoNestedEnum(
        e: NestedEnumRows([
          Int64List.fromList([1, 2]),
          Int64List.fromList([3]),
        ]),
      );
      expect((rows as NestedEnumRows).field0, [
        [1, 2],
        [3],
      ]);

      final named = echoNestedEnum(
        e: const NestedEnumNamed(
          entries: {
            'k': ['a', 'b'],
            'e': <String>[],
          },
        ),
      );
      expect((named as NestedEnumNamed).entries, {
        'k': ['a', 'b'],
        'e': <String>[],
      });
    });

    test('a recursive data type spelled with `Self` round-trips whole', () {
      // `Vec<Self>` in the field is the idiomatic Rust; the Dart class is
      // self-referential and the codecs recurse by function call, so depth is
      // bounded by the data rather than by the generated code.
      const tree = Category(
        name: 'root',
        children: [
          Category(
            name: 'a',
            children: [Category(name: 'a1', children: [])],
          ),
          Category(name: 'b', children: []),
        ],
      );
      final back = echoCategory(c: tree);
      expect(back.name, 'root');
      expect(back.children.map((c) => c.name), ['a', 'b']);
      expect(back.children.first.children.single.name, 'a1');
      // Generated deep equality reaches all the way down.
      expect(back, tree);
    });

    test(
      'a recursive data enum is evaluated Rust-side, so it arrived whole',
      () {
        expect(
          evalExpr(
            e: const ExprSum([
              ExprLit(1),
              ExprSum([ExprLit(2), ExprLit(3)]),
              ExprSum([]),
            ]),
          ),
          6,
        );
        expect(evalExpr(e: const ExprLit(7)), 7);
      },
    );

    test('`Box<Self>` gives the recursive shapes a `Vec` cannot', () {
      // A binary tree: exactly one child per side. The `Box` is invisible on
      // this side — the Dart field is a `Shape`, not a wrapper.
      expect(
        shapeSum(
          s: const ShapePair(
            ShapePair(ShapeLeaf(1), ShapeLeaf(2)),
            ShapeLeaf(4),
          ),
        ),
        7,
      );
      expect(shapeSum(s: const ShapeLeaf(-3)), -3);

      // `Option<Box<Self>>`: the nullable field composes exactly as
      // `Option<T>` does, so the Dart class is `Link? next`.
      final l = echoLink(
        l: const Link(
          value: 1,
          next: Link(value: 2, next: Link(value: 3, next: null)),
        ),
      );
      expect([l.value, l.next!.value, l.next!.next!.value], [1, 2, 3]);
      expect(l.next!.next!.next, isNull);
    });

    test('a `Box` around a value type is invisible to Dart', () {
      // `Box<i64>` is `int`, and `Vec<Box<i32>>` is the `Int32List` a
      // `Vec<i32>` is — including the bulk write, which Rust answers with an
      // element loop, so this crossing is where the two paths have to agree
      // byte for byte.
      final out = rebox(x: 10, xs: Int32List.fromList([1, 2, 3]));
      expect(out, isA<Int32List>());
      expect(out, [11, 12, 13]);
      expect(rebox(x: 0, xs: Int32List(0)), isEmpty);
    });
  });

  group('typed lists (Vec<numeric> <-> TypedData)', () {
    // INTEROP_CAPABILITY §1: every fixed-width numeric Vec crosses as the
    // matching Dart typed list, symmetric in both directions. Each case
    // asserts the returned value is the EXACT typed class (not just a List)
    // and round-trips. These run on native AND single-threaded web
    // (dart2wasm) — the one place a typed-list surprise (esp. Int64List)
    // would hide.
    test('each element type returns its exact typed-list class', () {
      final i8 = echoI8s(xs: Int8List.fromList([-128, 0, 127]));
      expect(i8, isA<Int8List>());
      expect(i8, [-128, 0, 127]);

      final i16 = echoI16s(xs: Int16List.fromList([-32768, 0, 32767]));
      expect(i16, isA<Int16List>());
      expect(i16, [-32768, 0, 32767]);

      final i32 = echoI32s(
        xs: Int32List.fromList([-2147483648, 0, 2147483647]),
      );
      expect(i32, isA<Int32List>());
      expect(i32, [-2147483648, 0, 2147483647]);

      final u16 = echoU16s(xs: Uint16List.fromList([0, 65535]));
      expect(u16, isA<Uint16List>());
      expect(u16, [0, 65535]);

      final u32 = echoU32s(xs: Uint32List.fromList([0, 4294967295]));
      expect(u32, isA<Uint32List>());
      expect(u32, [0, 4294967295]);

      final i64 = echoI64s(xs: Int64List.fromList([1, -2, 3]));
      expect(i64, isA<Int64List>());
      expect(i64, [1, -2, 3]);
    });

    test('i64::MIN/MAX survive inside Int64List (dart2wasm too)', () {
      // The adversarial 64-bit extremes must cross bit-exact under the
      // single-threaded wasm backend, where int is a genuine 64-bit value.
      final out = echoI64s(
        xs: Int64List.fromList([-9223372036854775808, 9223372036854775807, 0]),
      );
      expect(out, isA<Int64List>());
      expect(out[0], -9223372036854775808);
      expect(out[1], 9223372036854775807);
      expect(out[2], 0);
    });

    test('Float64List: NaN / ±Inf / -0.0 survive exactly', () {
      final out = echoF64s(
        xs: Float64List.fromList([
          double.nan,
          double.infinity,
          double.negativeInfinity,
          -0.0,
          3.5,
        ]),
      );
      expect(out, isA<Float64List>());
      expect(out[0].isNaN, isTrue);
      expect(out[1], double.infinity);
      expect(out[2], double.negativeInfinity);
      expect(1 / out[3], double.negativeInfinity); // -0.0 sign preserved
      expect(out[4], 3.5);
    });

    test('Float32List: specials survive and 24-bit rounding is genuine', () {
      final out = echoF32s(
        xs: Float32List.fromList([
          double.nan,
          double.infinity,
          0.5,
          16777217.0,
        ]),
      );
      expect(out, isA<Float32List>());
      expect(out[0].isNaN, isTrue);
      expect(out[1], double.infinity);
      expect(out[2], 0.5);
      // 2^24+1 rounds to 2^24 — proves the elements crossed as real f32.
      expect(out[3], 16777216.0);
    });

    test('empty typed lists round-trip as their typed class', () {
      expect(echoI32s(xs: Int32List(0)), isA<Int32List>());
      expect(echoI32s(xs: Int32List(0)), isEmpty);
      expect(echoF32s(xs: Float32List(0)), isEmpty);
      expect(echoU16s(xs: Uint16List(0)), isEmpty);
    });

    test('typed lists compose through Option / Map / nested Vec', () {
      // Option<Vec<i32>> -> Int32List?
      final opt = echoOptI32s(x: Int32List.fromList([7, 8, 9]));
      expect(opt, isA<Int32List>());
      expect(opt, [7, 8, 9]);
      expect(echoOptI32s(x: null), isNull);

      // HashMap<String, Vec<f64>> -> Map<String, Float64List>, with specials.
      final m = {
        'a': Float64List.fromList([1.5, double.nan]),
        'b': Float64List(0),
      };
      final mout = echoMapOfF64s(m: m);
      expect(mout['a'], isA<Float64List>());
      expect(mout['a']![0], 1.5);
      expect(mout['a']![1].isNaN, isTrue);
      expect(mout['b'], isEmpty);

      // Vec<Vec<i32>> -> List<Int32List> (outer generic, rows typed).
      final grid = [
        Int32List.fromList([1, 2]),
        Int32List.fromList([3, 4, 5]),
      ];
      final gout = echoI32Grid(g: grid);
      expect(gout, isA<List<Int32List>>());
      expect(gout[0], isA<Int32List>());
      expect(gout, grid);
    });

    test('struct with typed-list fields: round-trip and value equality', () {
      final s = Samples(
        xs: Int32List.fromList([1, 2, 3]),
        weights: Float64List.fromList([0.25, 0.75]),
      );
      final back = echoSamples(s: s);
      expect(back.xs, isA<Int32List>());
      expect(back.weights, isA<Float64List>());
      expect(back.xs, [1, 2, 3]);
      expect(back.weights, [0.25, 0.75]);
      // Generated deep == / hashCode must treat the typed-list fields
      // element-wise (they are List subtypes, routed through frDeepEquals).
      expect(back, s);
      expect(back.hashCode, s.hashCode);
      // A different independently-built instance with equal values compares
      // equal (not identity) and hashes equal — the point of value semantics.
      final twin = Samples(
        xs: Int32List.fromList([1, 2, 3]),
        weights: Float64List.fromList([0.25, 0.75]),
      );
      expect(twin, s);
      expect(twin.hashCode, s.hashCode);
      expect({s}.contains(twin), isTrue); // works as a Set member / map key
      // A differing element breaks equality.
      final other = Samples(
        xs: Int32List.fromList([1, 2, 4]),
        weights: Float64List.fromList([0.25, 0.75]),
      );
      expect(other == s, isFalse);
    });

    test('wire is byte-identical to the pre-typed-list List<int> encoding', () {
      // The typed-list change is Dart-container-only: encoding an Int32List
      // must produce exactly the bytes a plain List<int> would. Pinning this
      // proves wire compatibility (a Rust peer built before this change still
      // decodes the stream).
      final typed = BinaryWriter();
      typed.writeLen(3);
      for (final v in Int32List.fromList([1, -2, 300])) {
        typed.writeI32(v);
      }
      final plain = BinaryWriter();
      plain.writeLen(3);
      for (final v in <int>[1, -2, 300]) {
        plain.writeI32(v);
      }
      expect(typed.takeBytes(), plain.takeBytes());
    });
  });

  group('data types', () {
    test('struct with option field', () {
      final m = midpoint(
        a: const Point(x: 0, y: 0, label: 'origin'),
        b: const Point(x: 2, y: 4, label: null),
      );
      expect(m.x, 1.0);
      expect(m.y, 2.0);
      expect(m.label, 'origin');
    });

    test('tuple and unit structs', () {
      // Positional constructor, positional `copyWith`-able fields under their
      // synthesized names.
      expect(scaleMeters(m: const Meters(2.5), k: 4), const Meters(10));
      expect(const Meters(2.5).field0, 2.5);
      // `copyWith` takes `Object?` (the nullable trap), so the double is
      // written as one — there is no context type to promote an int literal.
      expect(const Meters(1).copyWith(field0: 3.0), const Meters(3));
      final s = relabelSpan(s: const Span(7, 'old'), label: 'new');
      expect(s, const Span(7, 'new'));
      expect(s.toString(), 'Span(field0: 7, field1: new)');
      // A unit struct is a singleton: every instance equal, no fields.
      expect(origin(), const Origin());
      expect(<Origin>{const Origin(), origin()}, hasLength(1));
      // Members compose with both shapes: a positional receiver is reached by
      // index in its decode, a unit one has nothing to read.
      expect(const Meters(1).feet(), closeTo(3.2808, 1e-4));
      expect(const Origin().label(), 'origin');
    });

    test('unit enum', () {
      expect(nextColor(c: Color.red), Color.green);
      expect(nextColor(c: Color.blue), Color.red);
    });

    test(
      'explicit Rust discriminants cross as a getter; the wire is position',
      () {
        // The number the Rust declaration writes, per variant — including the
        // one that writes none and takes Rust's successor rule, and a negative.
        expect(Status.ok.discriminant, 200);
        expect(Status.redirect.discriminant, 201);
        expect(Status.notFound.discriminant, 404);
        expect(Status.local.discriminant, -1);
        // `index` is what the wire carries, and it is unmoved by the numbers:
        // a value crossing in either direction agrees on the position.
        expect(Status.ok.index, 0);
        expect(statusOf(code: 404), Status.notFound);
        expect(statusOf(code: 999), Status.local);
      },
    );

    test('data enum: every variant shape round trips', () {
      final patches = echoPatches(
        ps: [
          const TextPatchSplice(index: 3, text: 'héllo'),
          const TextPatchDelete(index: 0, length: 2),
          const TextPatchMark('bold', -5),
          const TextPatchClear(),
        ],
      );
      expect(patches.length, 4);
      final splice = patches[0] as TextPatchSplice;
      expect(splice.index, 3);
      expect(splice.text, 'héllo');
      final del = patches[1] as TextPatchDelete;
      expect(del.length, 2);
      final mark = patches[2] as TextPatchMark;
      expect(mark.field0, 'bold');
      expect(mark.field1, -5);
      expect(patches[3], isA<TextPatchClear>());
    });

    test('generated data classes have value equality', () {
      // Structural ==/hashCode: a computed result equals a literal, works as a
      // map key / set member, and passes expect() by value (not identity).
      final computed = midpoint(
        a: const Point(x: 0, y: 0, label: 'p'),
        b: const Point(x: 2, y: 4, label: null),
      );
      const literal = Point(x: 1, y: 2, label: 'p');
      expect(computed, literal);
      expect(computed.hashCode, literal.hashCode);
      expect({computed}.contains(literal), isTrue);
      expect(<Point, int>{literal: 7}[computed], 7);
      // Data-enum variants (sealed hierarchy) compare by value too, including
      // a round-tripped list of them.
      expect(const TextPatchMark('bold', 1), const TextPatchMark('bold', 1));
      expect(echoPatches(ps: [const TextPatchDelete(index: 0, length: 2)]), [
        const TextPatchDelete(index: 0, length: 2),
      ]);
    });

    test('a value type crosses by reference, including as a typed slice', () {
      // The Dart side is unchanged — a borrow is a fact about the Rust
      // signature, not about the wire.
      expect(normOf(p: const Point(x: 3, y: 4, label: null)), 5.0);
      expect(
        sumX(
          ps: [
            const Point(x: 1, y: 0, label: null),
            const Point(x: 2, y: 0, label: 'b'),
          ],
        ),
        3.0,
      );
      expect(maxOf(xs: Int64List.fromList([3, 9, 4])), 9);
      expect(maxOf(xs: Int64List(0)), 0);
    });

    test('dart_identifier says where a member lands', () {
      // `Point::norm` takes `pointNorm` on the generated fake, so the free
      // `point_norm` beside it would collide (FR0002). It says where it goes
      // instead, and that is the name the Dart surface has.
      const p = Point(x: 3, y: 4, label: null);
      expect(pointNormOf(p: p), 5.0);
      expect(p.norm(), 5.0);
    });

    test(
      'a getter is Dart property syntax, and a data receiver may be by value',
      () async {
        const p = Point(x: -1, y: 2, label: null);
        expect(p.quadrant, 2); // no parentheses
        // A by-value receiver on a data type: the call decodes its own copy, so
        // the Dart value is untouched and calling twice is ordinary. No
        // annotation, and `Tick` holds a `String`, so `Copy` is out of reach.
        const t = Tick(at: 21, note: 'n');
        expect(t.consume(), 21);
        expect(t.consume(), 21);
        expect(t.consumeBoxed(), 'n');
        expect(t, const Tick(at: 21, note: 'n'));
        expect(await t.doubled, 42);
      },
    );

    test('a data method takes its own type by reference', () {
      const a = Point(x: 3, y: 4, label: null);
      const b = Point(x: 1, y: 2, label: null);
      expect(a.dot(other: b), 11.0);
      // A borrowed return reads through the reference into the decoded copy
      // the same scope owns; the Dart side sees an ordinary value.
      expect(a.xRef(), 3.0);
    });

    test('methods on a data type run against a copy of the receiver', () async {
      const p = Point(x: 3, y: 4, label: 'p');
      expect(p.norm(), 5.0);
      // The value is a snapshot: calling twice is not a use-after-move, and
      // the receiver is untouched by the call.
      expect(p.norm(), 5.0);
      expect(p, const Point(x: 3, y: 4, label: 'p'));
      // Async takes the same route (decode on the caller, body on a pool
      // thread) and returns a fresh value.
      expect(await p.scaled(k: 2), const Point(x: 6, y: 8, label: 'p'));
      expect(p.x, 3.0);
      // Receiverless: a static on the class, beside its own const constructor.
      expect(Point.origin(), const Point(x: 0, y: 0, label: null));
    });

    test('methods on a data enum land on the sealed base and on the enum', () {
      // A unit-only enum is a Dart `enum` with a member body.
      expect(Color.red.hex(), '#ff0000');
      expect(Color.blue.hex(), '#0000ff');
      // A fielded enum is a sealed hierarchy; one member on the base covers
      // every variant, and the encoder switches on which one it got.
      expect(const TextPatchSplice(index: 3, text: 'héllo').span(), 6);
      expect(const TextPatchDelete(index: 0, length: 2).span(), 2);
      expect(const TextPatchClear().span(), 0);
      // Called through the base type, not the subclass.
      const TextPatch p = TextPatchMark('bold', 1);
      expect(p.span(), 0);
    });
  });

  group('data-class ergonomics (copyWith, final, no_eq)', () {
    test('copyWith() with no args equals the original', () {
      const w = Widget(id: 1, owner: 'ann', tags: ['a', 'b']);
      final same = w.copyWith();
      expect(same, w);
      expect(same.id, 1);
      expect(same.owner, 'ann');
      expect(same.tags, ['a', 'b']);
    });

    test('copyWith(field: v) overrides exactly that field', () {
      const w = Widget(id: 1, owner: 'ann', tags: ['a']);
      final renamed = w.copyWith(owner: 'bob');
      expect(renamed.owner, 'bob');
      expect(renamed.id, 1);
      expect(renamed.tags, ['a']);
      // Collection field override.
      final retagged = w.copyWith(tags: ['x', 'y', 'z']);
      expect(retagged.tags, ['x', 'y', 'z']);
      expect(retagged.owner, 'ann');
    });

    test('copyWith(nullableField: null) nulls it; omitting preserves it', () {
      // The correctness case: the sentinel distinguishes "omitted" from
      // "explicit null". A naive `String? owner` param could not null the field.
      const w = Widget(id: 1, owner: 'ann', tags: []);
      final cleared = w.copyWith(owner: null);
      expect(cleared.owner, isNull);
      expect(cleared.id, 1);
      // Omitting the nullable field keeps the old non-null value.
      final kept = w.copyWith(id: 2);
      expect(kept.owner, 'ann');
      expect(kept.id, 2);
      // And starting from null, omitting keeps null (not "unset" confusion).
      const n = Widget(id: 9, owner: null, tags: []);
      expect(n.copyWith(id: 10).owner, isNull);
    });

    test('copyWith round-trips a value that crossed the bridge', () {
      final w = echoWidget(
        input: const Widget(id: 5, owner: 'z', tags: ['t']),
      );
      expect(
        w.copyWith(owner: null),
        const Widget(id: 5, owner: null, tags: ['t']),
      );
    });

    test('final fields: a Widget is immutable by construction', () {
      // Fields are `final`, so `w.id = 2` is a compile error. Assert the value
      // type is not mutable in place: the only way to a changed value is a new
      // instance via copyWith, which leaves the original untouched.
      const w = Widget(id: 1, owner: 'ann', tags: ['a']);
      final w2 = w.copyWith(id: 2);
      expect(w.id, 1); // original unchanged
      expect(w2.id, 2);
      expect(identical(w, w2), isFalse);
    });

    test('#[bridge(no_eq)]: identity equality, not value equality', () {
      // Two independently-built equal-valued Tickets are NOT equal (identity).
      const a = Ticket(code: 1, note: 'x');
      const b = Ticket(code: 1, note: 'x');
      // `const` canonicalization would make identical consts equal, so build
      // one through the bridge to get a distinct instance.
      final crossed = echoTicket(input: const Ticket(code: 1, note: 'x'));
      expect(crossed == a, isFalse);
      expect(identical(crossed, a), isFalse);
      // A no_eq type is not usable as a value Map/Set key: a second equal-valued
      // instance does not find the first's entry.
      final byTicket = <Ticket, int>{crossed: 7};
      expect(byTicket[a], isNull);
      expect(byTicket[crossed], 7); // same identity does find it
      // Contrast: a default (eq) struct with the same shape IS a value key.
      final wmap = <Widget, int>{
        echoWidget(input: const Widget(id: 1, owner: null, tags: [])): 7,
      };
      expect(wmap[const Widget(id: 1, owner: null, tags: [])], 7);
      // Two const Tickets ARE identical (const canonicalization), so this only
      // documents that the bridge-built instance above is the meaningful case.
      expect(identical(a, b), isTrue);
    });

    test('a handle-bearing data class gets the ordinary structural ==', () {
      // One equality rule for every data class. A handle field compares by
      // identity because `frDeepEquals` ends at `==` and a generated handle
      // class does not override it — so the field participates, and no handle
      // is ever compared by its contents.
      final a = openWorkspace(label: 'ws', words: ['x', 'y']);
      final b = openWorkspace(label: 'ws', words: ['x', 'y']);
      // Equal field values, different Rust objects.
      expect(a == b, isFalse);
      // Rebuilt over `a`'s own handles: equal, and hashing agrees.
      final same = Workspace(
        label: a.label,
        snapshot: a.snapshot,
        counter: a.counter,
      );
      expect(same, a);
      expect(same.hashCode, a.hashCode);
      // A non-handle field still decides.
      expect(
        Workspace(label: 'other', snapshot: a.snapshot, counter: a.counter) ==
            a,
        isFalse,
      );
      for (final w in [a, b]) {
        w.snapshot.dispose();
        w.counter.dispose();
      }
    });

    test('#[bridge(no_eq)] keeps copyWith and toString', () {
      // No value `==` on a no_eq type, so assert copyWith field-by-field.
      const t = Ticket(code: 1, note: 'x');
      final cleared = t.copyWith(note: null);
      expect(cleared.code, 1);
      expect(cleared.note, isNull);
      expect(t.copyWith(code: 2).code, 2);
      expect(t.copyWith(code: 2).note, 'x');
      expect(t.toString(), 'Ticket(code: 1, note: x)');
    });
  });

  group('errors and panics', () {
    test('Ok result', () {
      expect(parseNumber(s: ' 42 '), 42);
    });

    test('Err surfaces as BridgeException with the Rust message', () {
      expect(
        () => parseNumber(s: ''),
        throwsA(
          isA<BridgeException>().having(
            (e) => e.message,
            'message',
            contains('empty input'),
          ),
        ),
      );
    });

    test('async Err surfaces identically', () async {
      await expectLater(
        parseNumberAsync(s: 'not a number'),
        throwsA(isA<BridgeException>()),
      );
    });

    test('`Box<dyn Error>` crosses on the untyped tier as its Display text', () async {
      expect(parsePort(s: '8080'), 8080);
      expect(await parsePortAsync(s: ' 443 '), 443);
      // The message is the boxed error's own `Display` — here `ParseIntError`'s
      // — reaching Dart as the same `BridgeException` `anyhow::Error` and
      // `String` reach it as.
      expect(
        () => parsePort(s: 'http'),
        throwsA(
          isA<BridgeException>().having(
            (e) => e.message,
            'message',
            contains('invalid digit'),
          ),
        ),
      );
      await expectLater(
        parsePortAsync(s: '99999'),
        throwsA(
          isA<BridgeException>().having(
            (e) => e.message,
            'message',
            contains('number too large'),
          ),
        ),
      );
    });

    test('a typed Err arrives as a value Dart can branch on', () {
      // The point of F-52: before this, both variants were one type with
      // different prose, and an app could only render the string.
      try {
        withdraw(balance: 100, amount: 250);
        fail('expected a typed error');
      } on WithdrawErrorException catch (e) {
        final err = e.error;
        expect(err, isA<WithdrawErrorInsufficient>());
        // The payload crosses too, not just the discriminant.
        expect((err as WithdrawErrorInsufficient).shortBy, 150);
      }
    });

    test('a typed Err is still a BridgeException', () {
      // Load-bearing: adopting a typed error must never make a failure escape
      // code that caught it before.
      expect(
        () => withdraw(balance: -1, amount: 0),
        throwsA(isA<BridgeException>()),
      );
      expect(
        () => withdraw(balance: -1, amount: 0),
        throwsA(isA<WithdrawErrorException>()),
      );
    });

    test('a typed Err renders as prose for a caller that only logs it', () {
      try {
        withdraw(balance: -1, amount: 0);
        fail('expected a typed error');
      } on WithdrawErrorException catch (e) {
        // No Display impl on the Rust side; the message is Dart's rendering.
        expect(e.message, contains('WithdrawErrorAccountFrozen'));
      }
    });

    test('a typed Err survives the async round trip', () async {
      // The sync path returns the envelope inline; the async path posts a
      // completion through PendingCalls, where the decoder has to have been
      // carried from issue to completion.
      await expectLater(
        withdrawAsync(balance: 10, amount: 40),
        throwsA(
          isA<WithdrawErrorException>().having(
            (e) => (e.error as WithdrawErrorInsufficient).shortBy,
            'shortBy',
            30,
          ),
        ),
      );
    });

    test('Ok still returns the value on a typed-error member', () {
      expect(withdraw(balance: 100, amount: 40), 60);
    });

    test('a typed Err survives a real async fn body', () async {
      // A third dispatch shape: `async fn` completes through the cooperative
      // executor rather than the pool, and this one yields first so the Err is
      // produced on a *resumed* poll, after the initial call returned.
      await expectLater(
        withdrawAwaiting(balance: 10, amount: 40),
        throwsA(
          isA<WithdrawErrorException>().having(
            (e) => (e.error as WithdrawErrorInsufficient).shortBy,
            'shortBy',
            30,
          ),
        ),
      );
      expect(await withdrawAwaiting(balance: 100, amount: 40), 60);
    });

    test('a typed Err survives an actor method', () async {
      // The fourth shape, and the one furthest from a free function: the call
      // runs on the actor's own executor and comes back through its host.
      final m = await Miner.new_(label: 'typed');
      try {
        await expectLater(
          m.withdrawFrom(amount: 250),
          throwsA(isA<WithdrawErrorException>()),
        );
        expect(await m.withdrawFrom(amount: 40), 60);
      } finally {
        await m.dispose();
      }
    });

    test('panic surfaces as BridgePanicException, attributably', () {
      expect(
        () => alwaysPanics(),
        throwsA(
          isA<BridgePanicException>().having(
            (e) => e.message,
            'message',
            contains('deliberate panic'),
          ),
        ),
      );
    });
  });

  group('async execution', () {
    test('computation happens off-thread and completes', () async {
      expect(await sumSquares(n: 1000), 333833500);
    });

    test('many concurrent calls all complete correctly', () async {
      final results = await Future.wait(
        List.generate(50, (i) => sumSquares(n: i + 1)),
      );
      for (var i = 0; i < 50; i++) {
        final n = i + 1;
        expect(results[i], n * (n + 1) * (2 * n + 1) ~/ 6);
      }
    });
  });

  group('pool replenishment', () {
    test('a pool panic fails exactly its own future', () async {
      await expectLater(
        poolPanic(),
        throwsA(
          isA<BridgePanicException>().having(
            (e) => e.message,
            'message',
            contains('deliberate pool panic'),
          ),
        ),
      );
      // The bridge stays usable after the panic.
      expect(await sumSquares(n: 10), 385);
    });

    test('a panic never narrows the pool', () async {
      if (!Frustrate.instance.asyncIsParallel) {
        markTestSkipped('single-threaded web: async inline, no pool');
        return;
      }
      final width = poolWidth();
      // Exact, not timing-based: `width` concurrent probes all return true
      // iff `width` workers ran them simultaneously; on a narrowed pool the
      // excess probes deterministically report false instead. The timeout
      // only bounds the broken run.
      Future<void> expectFullWidth() async {
        rendezvousReset();
        final ok = await Future.wait(
          List.generate(
            width,
            (_) => poolRendezvous(width: width, maxWaitMs: 10000),
          ),
        );
        expect(ok, everyElement(isTrue));
      }

      await expectFullWidth(); // baseline: spawns and exercises the pool
      await expectLater(poolPanic(), throwsA(isA<BridgePanicException>()));
      await expectFullWidth(); // width restored — replenished if threaded
    });

    test('pool survives a panic storm at full width', () async {
      if (!Frustrate.instance.asyncIsParallel) {
        markTestSkipped('single-threaded web: async inline, no pool');
        return;
      }
      // More panics than the pool is wide: replacement workers are
      // themselves replaceable, not just the originals.
      for (var i = 0; i < 5; i++) {
        await expectLater(poolPanic(), throwsA(isA<BridgePanicException>()));
      }
      final width = poolWidth();
      rendezvousReset();
      final ok = await Future.wait(
        List.generate(
          width,
          (_) => poolRendezvous(width: width, maxWaitMs: 10000),
        ),
      );
      expect(ok, everyElement(isTrue));
    });
  });

  group('Confined: TextDoc', () {
    test('sync constructor, sync methods, &mut splice', () {
      final doc = TextDoc.new_();
      expect(doc.text(), '');
      final patches = doc.splice(index: 0, delete: 0, insert: 'hello world');
      expect(patches, hasLength(1));
      expect(doc.text(), 'hello world');
      doc.splice(index: 5, delete: 6, insert: '!');
      expect(doc.text(), 'hello!');
      expect(doc.lenChars(), 6);
    });

    test('async constructor transfers ownership', () async {
      final doc = await TextDoc.load(initial: 'prefilled');
      expect(doc.text(), 'prefilled');
    });

    test('splice error carries context', () {
      final doc = TextDoc.new_();
      expect(
        () => doc.splice(index: 5, delete: 1, insert: 'x'),
        throwsA(
          isA<BridgeException>().having(
            (e) => e.message,
            'message',
            contains('out of bounds'),
          ),
        ),
      );
    });

    test('borrowed param in sync free function', () {
      final doc = TextDoc.new_();
      doc.splice(index: 0, delete: 0, insert: 'hello');
      expect(docStartsWith(doc: doc, prefix: 'he'), isTrue);
      expect(docStartsWith(doc: doc, prefix: 'x'), isFalse);
    });

    test('unicode editing: emoji count as single chars', () {
      final doc = TextDoc.new_();
      doc.splice(index: 0, delete: 0, insert: 'a😀b');
      // 3 Unicode scalar values (Rust chars); 4 UTF-16 code units in Dart.
      expect(doc.lenChars(), 3);
      expect('a😀b'.length, 4);
    });

    test('dispose is idempotent; use-after-dispose is loud', () {
      final doc = TextDoc.new_();
      doc.dispose();
      doc.dispose();
      expect(() => doc.text(), throwsStateError);
    });
  });

  group('Frozen: Snapshot', () {
    test('sync reads and async method share the Arc', () async {
      final snap = Snapshot.build(words: ['a', 'bb', 'ccc']);
      expect(snap.wordCount(), 3);
      expect(await snap.join(sep: '-'), 'a-bb-ccc');
      expect(await snapshotTotalLen(snap: snap), 6);
    });

    test('borrowed returns copy into the response inside the guard', () {
      final snap = Snapshot.build(words: ['a', 'bb', 'ccc']);
      // `&str` and `&Vec<String>` both cross as the value they point at; the
      // reference never leaves Rust.
      expect(snap.firstWord(), 'a');
      expect(snap.allWords(), ['a', 'bb', 'ccc']);
      final empty = Snapshot.build(words: []);
      expect(empty.firstWord(), '');
      expect(empty.allWords(), isEmpty);
      snap.dispose();
      empty.dispose();
    });

    test('Option<opaque> return: some and none', () async {
      final found = await findSnapshot(make: true);
      expect(found, isNotNull);
      expect(found!.wordCount(), 1);
      expect(await findSnapshot(make: false), isNull);
    });

    test(
      'handle stays valid while async call in flight after dispose',
      () async {
        final snap = Snapshot.build(words: ['x', 'y']);
        final pending = snap.join(sep: '+'); // Arc cloned during the call
        snap.dispose();
        expect(await pending, 'x+y'); // Rust object kept alive by the Arc
      },
    );

    test(
      'rust async fn: awaits a self-driving future, resolves intact',
      () async {
        // `asyncWordCount` is a Rust `async fn`; its future runs on the
        // cooperative executor — on EVERY platform now, including
        // single-threaded web. Its body `.await`s a future that returns Pending
        // once before Ready, so the executor must poll more than once and
        // resume the suspended task. (Previously native-only: the old model
        // parked a pool thread per future, which single-threaded web has none
        // of.)
        final snap = Snapshot.build(words: ['alpha', 'beta', 'gamma']);
        expect(await snap.asyncWordCount(), 3);
      },
    );

    test('1000 concurrent async fn calls all resolve (multiplexing)', () async {
      // The load-bearing proof that the executor multiplexes many suspended
      // futures on few threads. Decisive on single-threaded web: a
      // block_on-per-call model would need 1000 threads (there is one), so it
      // would deadlock — the cooperative executor holds all 1000 suspended
      // tasks as heap data and drives them via microtasks on the one thread.
      final snaps = List.generate(
        1000,
        (i) => Snapshot.build(words: List.generate(i % 7, (j) => 'w$j')),
      );
      final counts = await Future.wait(snaps.map((s) => s.asyncWordCount()));
      for (var i = 0; i < 1000; i++) {
        expect(counts[i], i % 7, reason: 'call $i');
      }
    });

    test('rust async fn panicking mid-poll rejects, never hangs', () async {
      // The body `.await`s a self-driving future (Pending on the first poll)
      // and then panics on the RESUMED poll, so the panic lands mid-poll on a
      // later executor drain — never on the initial frustrate_call_async. Each
      // platform reaches the trap by a different route: native catches it with
      // catch_unwind; single-threaded web's trap escapes the synchronous call
      // frame to the frustrate_drain microtask; threaded web's trap kills a
      // pool worker. All three must reject the returned future with a
      // BridgePanicException naming this call (attribution via
      // frustrate_current_drain_call). The timeout turns a regression — the
      // future left pending forever — into a loud failure, not a hung suite.
      await expectLater(
        asyncPanicsAfterYield(),
        throwsA(
          isA<BridgePanicException>().having(
            (e) => e.message,
            'message',
            contains('deliberate async panic after yield'),
          ),
        ),
      );
    }, timeout: const Timeout(Duration(seconds: 10)));

    test(
      'a mid-poll panic does not strand other in-flight async fns',
      () async {
        // One panicking async fn in flight must not poison the executor for its
        // neighbours: the run queue keeps draining, so a concurrently-issued
        // async fn still resolves. Single-threaded web is the decisive case —
        // one thread, one shared run queue, each ready task riding its own
        // scheduled microtask, so abandoning the trapping drain does not lose
        // the rest.
        final snap = Snapshot.build(words: ['a', 'b', 'c']);
        final doomed = asyncPanicsAfterYield();
        final healthy = snap.asyncWordCount();
        await expectLater(doomed, throwsA(isA<BridgePanicException>()));
        expect(await healthy, 3);
      },
      timeout: const Timeout(Duration(seconds: 10)),
    );
  });

  group('Locked: Counter', () {
    test('async mutation and reads', () async {
      final c = Counter.new_();
      expect(await c.add(delta: 5), 5);
      expect(await c.add(delta: -2), 3);
      expect(await c.get(), 3);
    });

    // The two tests below pin the single-threaded build's divergent Locked
    // semantics: a wasm instance running async bodies inline-to-completion
    // can never observe lock contention, and a Rust panic (trap, no
    // unwinding) while holding a lock leaves the lock held forever.

    test('web: the blocking contract is compile-time absent', () {
      // The web variant omits `on_contention = "block"` under both wasm
      // builds: sync bridge calls originate on the main thread, and a blocking
      // acquisition waits there by declaration. Its try-lock sibling refuses
      // instead of waiting and is present — locked_sync_test.dart runs it here.
      //
      // Extension resolution proves the absence: a real instance member would
      // win over the extension below and fail this expectation. On native the
      // member exists (native_blocking_test.dart).
      final c = Counter.new_();
      expect(c.blockingGet(), _BlockingGetAbsentOnWeb.absent);
      expect(c.tryGet(), 0, reason: 'the try-lock is a real member on web');
    }, testOn: 'browser');

    test('web: the opt-in member is present but throws UnsupportedError', () {
      // `blockingRead` carries #[bridge(web = "runtime_fail")]: the informed
      // opt-in that INCLUDES this native-only member in the web build so
      // portable code naming it compiles for web (this test referencing it IS
      // that proof — the file would not compile for a browser platform
      // otherwise). Its web body throws a loud, attributable UnsupportedError on
      // the Dart side before any attempt to cross — never a hang or a trap. On
      // native the same member is an ordinary blocking read
      // (native_blocking_test.dart).
      final c = Counter.new_();
      expect(
        () => c.blockingRead(),
        throwsA(
          isA<UnsupportedError>().having(
            (e) => e.message,
            'message',
            allOf(contains('blockingRead'), contains('runtime_fail')),
          ),
        ),
      );
    }, testOn: 'browser');

    test(
      'single-threaded: trapping while holding a lock poisons the object',
      () async {
        if (Frustrate.instance.asyncIsParallel) {
          // Threaded: thread::sleep is real on worker threads (futex with
          // timeout), so holdWrite holds and releases instead of trapping —
          // the contention test above covers those semantics.
          markTestSkipped(
            'threaded wasm: sleep does not trap on worker threads',
          );
          return;
        }
        final c = Counter.new_();
        // holdWrite calls thread::sleep, which panics (traps) on
        // wasm32-unknown-unknown — attributably, while holding the write lock.
        await expectLater(
          c.holdWrite(millis: 10),
          throwsA(isA<BridgePanicException>()),
        );
        // No unwinding ran, so the write lock is still held — permanently, and
        // both acquisition modes say so. The try-lock reports it at once and
        // attributably, which is the sharper observable; the dispatched sibling
        // suspends on a grant that can never come and never completes.
        expect(() => c.tryGet(), throwsA(isA<ContentionException>()));
        await expectLater(
          c.get().timeout(const Duration(milliseconds: 250)),
          throwsA(isA<TimeoutException>()),
        );
      },
      testOn: 'browser',
    );
  });

  group('Data + Locked: Note, one Rust struct as two Dart classes', () {
    test('the value class carries fields and the handle class carries the object', () async {
      // The value half: an ordinary generated data class. Its members take the
      // receiver as a value on the wire, so `wordCount` reads the copy this
      // call decoded and nothing outlives it.
      const n = Note(title: 'draft', body: 'one two three');
      expect(n.wordCount(), 3);
      expect(noteHeadline(n: n), 'draft: one two three');
      // …with the value semantics a data class has: equality by fields, and
      // `copyWith` rather than mutation.
      expect(n, const Note(title: 'draft', body: 'one two three'));
      expect(n.copyWith(body: 'x').body, 'x');
      expect(Note.blank(title: 'empty').body, '');

      // The handle half: the same Rust struct, reached by id. `append` is the
      // point of the pair — it mutates the one Rust object, where the value
      // half's mutation would land on a decoded copy and be discarded (FR0013
      // refuses writing it at all).
      final h = NoteHandle.open(title: 'draft', body: 'one two');
      expect(await h.append(more: 'three'), 3);
      expect(await h.append(more: 'four'), 4);

      // Handle → value: a snapshot the caller keeps after the object is gone.
      final snap = await h.snapshot();
      expect(snap, isA<Note>());
      expect(snap.wordCount(), 4);
      h.dispose();
      // The snapshot is a value, so disposing the handle does not touch it.
      expect(snap.body, 'one two three four');
    });

    test('a value crosses into a handle and back out', () async {
      // Value → handle: minting is the author's one-liner, and what comes
      // back is a live object, not a copy of the value.
      final h = noteReopen(
        n: const Note(title: 't', body: 'a b'),
      );
      expect(await h.append(more: 'c'), 3);
      expect((await h.snapshot()).body, 'a b c');
      h.dispose();
    });

    test('the two classes are distinct Dart types', () {
      // The whole point of the pair: one is a value, the other a handle, and
      // Dart's own type system tells them apart. `NoteHandle` is the name the
      // declaration gave the handle half — both halves derive `Note`, so one
      // had to be renamed.
      final h = NoteHandle.open(title: 't', body: 'b');
      expect(h, isA<OpaqueHandle>());
      expect(h, isNot(isA<Note>()));
      expect(const Note(title: 't', body: 'b'), isNot(isA<OpaqueHandle>()));
      h.dispose();
    });

    test('`Self` is the block\'s self type as written, on each half', () async {
      // `impl Data<Note>`: `Self` is the value class, by value and one level
      // in under a borrow.
      const a = Note(title: 'a', body: 'one two three');
      const b = Note(title: 'b', body: 'one');
      expect(a.longerThan(other: b), isTrue);
      expect(b.longerThan(other: a), isFalse);
      expect(
        a.totalWords(
          rest: [
            b,
            const Note(title: 'c', body: 'x y'),
          ],
        ),
        6,
      );

      // `impl Locked<Note>`: `Self` is the handle class. Borrowed here, so the
      // caller keeps both objects.
      final h = NoteHandle.open(title: 't', body: 'x');
      final other = NoteHandle.open(title: 't', body: 'y');
      expect(await h.sameTitleAs(other: other), isTrue);
      // And by value, which is a consume — the Dart type is
      // `Consumed<NoteHandle>`, exactly as writing the type's name gives.
      expect(await h.absorb(other: other.take()), 2);
      expect(other.isDisposed, isTrue);
      h.dispose();
    });
  });

  group('traits (trait objects across the bridge)', () {
    test('one factory, two behaviors behind one Dart type', () {
      // Confined trait: methods derive sync and run on the caller. The
      // polymorphism proof: both handles have static type Tally, behavior
      // diverges through the vtable.
      final step = newTally(kind: 'step', step: 5);
      final square = newTally(kind: 'square', step: 0);
      expect(step.bump(), 5);
      expect(step.bump(), 10);
      expect(square.bump(), 1);
      expect(square.bump(), 4);
      expect(step.total(), 10);
      expect(square.total(), 4);
    });

    test('default-bodied trait methods bridge like required ones', () {
      final step = newTally(kind: 'step', step: 3)..bump();
      final square = newTally(kind: 'square', step: 0)..bump();
      // StepTally uses the trait's default body; SquareTally overrides it —
      // both dispatch through the vtable.
      expect(step.describe(), 'tally at 3');
      expect(square.describe(), '1^2 = 1');
    });

    test('factory errors are attributable BridgeExceptions', () {
      expect(
        () => newTally(kind: 'nope', step: 1),
        throwsA(
          isA<BridgeException>().having(
            (e) => e.message,
            'message',
            contains('unknown tally kind'),
          ),
        ),
      );
    });

    test('dispose is idempotent; use-after-dispose is loud', () {
      final t = newTally(kind: 'step', step: 1);
      t.dispose();
      t.dispose();
      expect(() => t.bump(), throwsStateError);
    });

    test(
      'frozen trait: sync and async methods on a shared trait object',
      () async {
        final g = newGreeter(kind: 'plain');
        expect(g.greet(name: 'ada'), 'hello ada');
        // Async methods run over the shared Arc (concurrently where the pool
        // is real; inline on single-threaded web — same contract).
        final many = await Future.wait([
          g.greetMany(names: ['a', 'b']),
          g.greetMany(names: ['c']),
        ]);
        expect(many[0], ['hello a', 'hello b']);
        expect(many[1], ['hello c']);
      },
    );

    test('trait methods can mint new trait-object handles', () async {
      final g = newGreeter(kind: 'plain');
      final louder = await g.louder();
      final loudest = await louder.louder();
      expect(loudest.greet(name: 'ada'), 'hello ada!!');
      // The originals are untouched (fresh objects, fresh handles).
      expect(g.greet(name: 'ada'), 'hello ada');
    });

    test('borrowed &dyn param on a free function', () async {
      final pirate = newGreeter(kind: 'pirate');
      expect(
        await greetCrowd(g: pirate, names: ['jim', 'silver']),
        'ahoy jim, ahoy silver',
      );
      final plain = newGreeter(kind: 'plain');
      expect(await greetCrowd(g: plain, names: ['jim']), 'hello jim');
    });

    test('locked trait: shared mutation with the usual contracts', () async {
      final store = await openStore(kind: 'mem');
      await store.put(key: 'k', value: 'v');
      expect(await store.get(key: 'k'), 'v');
      expect(await store.get(key: 'missing'), isNull);
      // The trait's sync `size` is in locked_sync_test.dart; the async round
      // trip above is what proves the mutation landed here.
      // Divergent impl through the same Dart type.
      final shout = await openStore(kind: 'shout');
      await shout.put(key: 'k', value: 'v');
      expect(await shout.get(key: 'k'), 'V');
    });

    test('bridged impls make the class implement the trait interface', () {
      // One Dart list holds dyn handles and concrete handles; behavior
      // stays per-object.
      final tallies = <Tally>[newTally(kind: 'step', step: 2), Abacus.new_()];
      expect(tallies[0].bump(), 2);
      expect(tallies[1].bump(), 10);
      expect(tallies.map((t) => t.total()), [2, 10]);
    });

    test('unwritten default methods are synthesized onto implementors', () {
      final a = Abacus.new_()..bump();
      // `describe` is not in the impl block; the trait's default body runs
      // through UFCS static dispatch.
      expect(a.describe(), 'tally at 10');
    });

    test('concrete handles cross tagged &dyn params (confined, mut)', () {
      final a = Abacus.new_();
      expect(bumpTwice(t: a), 20);
      // The dyn handle crosses the same parameter.
      expect(bumpTwice(t: newTally(kind: 'step', step: 3)), 6);
      expect(a.total(), 20);
    });

    test('concrete handles cross tagged &dyn params (frozen, async)', () async {
      final robot = RobotGreeter.build(id: 7);
      expect(robot.greet(name: 'ada'), 'BEEP ada [unit 7]');
      expect(await greetCrowd(g: robot, names: ['jim']), 'BEEP jim [unit 7]');
      // Trait-object returns from a concrete implementor's method are dyn
      // handles — and they cross the same param.
      final louder = await robot.louder();
      expect(await greetCrowd(g: louder, names: ['jim']), 'BEEP jim [unit 8]');
      expect(
        await greetCrowd(
          g: newGreeter(kind: 'pirate'),
          names: ['jim'],
        ),
        'ahoy jim',
      );
    });

    test(
      'concrete handles cross tagged &dyn params (locked, all shapes)',
      () async {
        final counting = CountingStore.fresh();
        await storeFill(s: counting, n: 3); // &mut dyn: write lock on the pool
        expect(
          await storeProbe(s: counting, key: 'k1'),
          'v1',
        ); // &dyn: read lock
        expect(await counting.puts(), 3, reason: 'inherent methods coexist');
        // The dyn handle crosses identically.
        final dynStore = await openStore(kind: 'mem');
        await storeFill(s: dynStore, n: 2);
        expect(await storeProbe(s: dynStore, key: 'k0'), 'v0');
        // The sync contract-marked shape (`storeSizeNow`) crosses a tagged
        // `&dyn` param too; locked_sync_test.dart drives that arm on every
        // target.
      },
    );
  });

  group('actors', () {
    test('spawn, call, state, dispose lifecycle', () async {
      final m = await Miner.new_(label: 'alpha');
      expect(await m.label(), 'alpha');
      expect(await m.nthPrime(n: 100), 541);
      expect(await m.nthPrime(n: 10), 29);
      expect(await m.calls(), 2);
      await m.dispose();
      expect(() => m.label(), throwsStateError);
      await m.dispose(); // idempotent
    });

    test('a call on a shut-down host rejects with StateError on every platform', () async {
      // Driven against the runtime surface directly: a generated method can
      // never reach this, because its own `_handle` guard throws first. This
      // is the layer underneath, and it used to disagree — web refused with a
      // *synchronous* StateError while native let the call through to Rust and
      // came back with a BridgePanicException from the dead-host reply.
      final host = await Frustrate.instance.spawnActorHost();
      await host.shutdown();

      // Deliberately not wrapped: the refusal must arrive through the future.
      // A Future-returning bridge API never throws synchronously, so that
      // `unawaited(…)`, `Future.wait([…])` and store-then-await-later behave.
      late final Future<BinaryReader> refused;
      expect(() => refused = host.call(0, 0, (w) {}), returnsNormally);
      await expectLater(
        refused,
        throwsA(
          isA<StateError>().having(
            (e) => e.message,
            'message',
            contains('actor host was shut down'),
          ),
        ),
      );
      // And the refused issue registered nothing: no pending completer, and on
      // native therefore no keep-alive pin left latched on.
      expect(Frustrate.instance.openChannelCount, 0);
    });

    test(
      'errors and panics stay attributable across the actor boundary',
      () async {
        final m = await Miner.new_(label: 'boom');
        expect(await m.checkedDiv(a: 10, b: 2), 5);
        await expectLater(
          m.checkedDiv(a: 1, b: 0),
          throwsA(
            isA<BridgeException>().having(
              (e) => e.message,
              'message',
              contains('division by zero in miner `boom`'),
            ),
          ),
        );
        await expectLater(
          m.explode(),
          throwsA(
            isA<BridgePanicException>().having(
              (e) => e.message,
              'message',
              contains('exploded'),
            ),
          ),
        );
        // The executor survives a panicking call (native: caught unwind; web:
        // the instance stays callable after a trap).
        expect(await m.label(), 'boom');
        await m.dispose();
      },
    );

    test('a panicking Drop is loud and strands no executor', () async {
      final before = actorHostCount();
      final g = await Grenade.new_();
      await expectLater(
        g.dispose(),
        throwsA(
          isA<BridgePanicException>().having(
            (e) => e.message,
            'message',
            contains('grenade'),
          ),
        ),
      );
      expect(
        actorHostCount(),
        before,
        reason:
            'dispose must release the executor even when the drop '
            'call itself panics',
      );
    });

    test('a panicking constructor is loud and strands no executor', () async {
      final before = actorHostCount();
      await expectLater(
        Miner.flawed(label: 'dud'),
        throwsA(
          isA<BridgePanicException>().having(
            (e) => e.message,
            'message',
            contains('failed to assemble'),
          ),
        ),
      );
      expect(
        actorHostCount(),
        before,
        reason:
            'a failed spawn must tear down the executor it created, '
            'not strand it',
      );
    });

    test(
      'calls are FIFO on one executor (no await between submissions)',
      () async {
        final m = await Miner.new_(label: 'fifo');
        final slow = m.nthPrime(n: 2000);
        final observed = m.calls(); // enqueued behind the CPU work
        expect(await observed, 1, reason: 'must observe nthPrime\'s effect');
        await slow;
        await m.dispose();
      },
    );

    test('each actor owns an independent executor', () async {
      final a = await Miner.new_(label: 'a');
      final b = await Miner.new_(label: 'b');
      final results = await Future.wait([
        a.nthPrime(n: 500),
        b.nthPrime(n: 500),
      ]);
      expect(results[0], results[1]);
      expect(await a.calls(), 1);
      expect(await b.calls(), 1);
      await a.dispose();
      await b.dispose();
    });

    // The actor request is the only one that crosses a `postMessage`, and it
    // is the writer's *view*, not the writer's buffer: `takeBytes()` returns
    // `sublistView(_buf, 0, _len)` over a buffer that starts at 64 bytes and
    // doubles, so almost every request is narrower than what backs it. The
    // transport must frame by that view's length, never by the buffer's size
    // — otherwise the writer's unused capacity rides along as trailing
    // payload bytes.
    //
    // The sizes below walk the writer's capacity ladder: a request is an
    // 8-byte handle plus an 8-byte length prefix plus the payload, so they
    // land on maximum slack (just past a doubling), minimum slack, and exact
    // fit at 64/128/256/1024. Rust's `assert_consumed()` at the end of every
    // generated request decode is what makes an over-read fail here instead
    // of being absorbed; on today's dart2wasm `Uint8List.toJS` happens to
    // copy into an exactly-sized buffer, so this is coverage of the framing,
    // not a differential against the old code.
    test(
      'an actor request is framed by its length, not its buffer capacity',
      () async {
        final m = await Miner.new_(label: 'framing');
        // digest() returns data.length + every 4096th byte; under 4 KiB that is
        // data.length + data[0], and 0 for an empty payload.
        for (final n in const [0, 1, 47, 48, 49, 111, 112, 113, 1000]) {
          final payload = Uint8List(n);
          if (n > 0) payload[0] = 7;
          expect(
            await m.digest(data: payload),
            n == 0 ? 0 : n + 7,
            reason:
                'payload of $n byte(s) (request ${n + 16}, writer '
                'capacity is the next power of two ≥ 64)',
          );
        }
        await m.dispose();
      },
    );
  });

  group('Vec<Opaque> returns', () {
    test('list of concrete frozen handles, each callable', () {
      final snaps = snapshots(counts: Int64List.fromList([0, 1, 3]));
      expect(snaps, hasLength(3));
      expect(snaps.map((s) => s.wordCount()), [0, 1, 3]);
      // Each is an independent, live handle — dispose them individually.
      for (final s in snaps) {
        s.dispose();
      }
      expect(() => snaps.first.wordCount(), throwsStateError);
    });

    test('empty list of handles', () {
      expect(snapshots(counts: Int64List(0)), isEmpty);
    });

    test(
      '`Self` inside the return names the impl type, and stays a static',
      () {
        // `-> Option<Self>` is a factory that may decline. It lands as a static
        // returning a nullable handle, never a constructor — Dart has no
        // constructor spelling that can return null either.
        expect(Snapshot.maybeBuild(words: const []), isNull);
        final s = Snapshot.maybeBuild(words: const ['a', 'b']);
        expect(s, isNotNull);
        expect(s!.wordCount(), 2);
        s.dispose();

        // `-> Vec<Self>` mints one handle per element through the same
        // substitution.
        final each = Snapshot.splitWords(words: const ['x', 'y', 'z']);
        expect(each.map((h) => h.firstWord()), ['x', 'y', 'z']);
        for (final h in each) {
          h.dispose();
        }
      },
    );

    test('list of trait-object handles is polymorphic and callable', () {
      final gs = greeters(kinds: ['plain', 'pirate', 'plain']);
      expect(gs, hasLength(3));
      expect(gs[0].greet(name: 'ada'), 'hello ada');
      expect(gs[1].greet(name: 'ada'), 'ahoy ada');
      expect(gs[2].greet(name: 'bo'), 'hello bo');
      for (final g in gs) {
        g.dispose();
      }
    });

    test('a map and a set of handles mint one wrapper per entry', () {
      // Values.
      final byName = tagsByName(names: const ['a', 'bb', 'ccc']);
      expect(byName.keys.toList()..sort(), ['a', 'bb', 'ccc']);
      expect(byName['bb']!.name(), 'bb');
      for (final t in byName.values) {
        t.dispose();
      }

      // Elements, through the BTree container, so the order is the sorted one
      // Rust iterated in and Dart's insertion-ordered Set preserved.
      final set = tagSet(names: const ['c', 'a', 'b']);
      expect(set.map((t) => t.name()), ['a', 'b', 'c']);
      // Each element is its own live handle, not one shared wrapper.
      expect(set.length, 3);
      for (final t in set) {
        t.dispose();
      }

      // Keys. A handle's Dart equality is identity, so this map is for
      // iterating, not for looking a key up — the same as `List<Tag>.contains`.
      final lengths = tagLengths(names: const ['x', 'yy']);
      expect(lengths.entries.map((e) => (e.key.name(), e.value)), [
        ('x', 1),
        ('yy', 2),
      ]);
      for (final t in lengths.keys) {
        t.dispose();
      }
    });

    test('a `Box` around a handle is transparent in a returned struct', () {
      final b = boxedTag(label: 't');
      expect(b.label, 't');
      // The Dart field is a `Tag`, not a wrapper, and it is a live handle.
      expect(b.tag.name(), 't');
      b.tag.dispose();
    });

    test('a fixed array of handles mints one wrapper per slot', () {
      final pair = tagPair(a: 'l', b: 'r');
      expect(pair.map((t) => t.name()), ['l', 'r']);
      for (final t in pair) {
        t.dispose();
      }
      expect(() => pair.first.name(), throwsStateError);
    });

    test('a handle-owning data type carries receiverless members', () {
      // A receiver would be decoded out of the request, which is the
      // direction this type cannot travel; a static decodes nothing.
      expect(Workspace.defaultLabel(), 'untitled');
    });

    test(
      'a returned struct carries several handles of different types',
      () async {
        final w = openWorkspace(label: 'one', words: ['a', 'b']);
        expect(w.label, 'one');
        // Both are live, independent handles with their own models — a frozen
        // sync read and a locked async one, portable to both platforms.
        expect(w.snapshot.wordCount(), 2);
        expect(await w.counter.get(), 0);
        // A handle is not a value, so the class takes identity equality — two
        // workspaces over different Rust objects are not the same workspace,
        // and comparing the handles themselves would mean nothing.
        final other = openWorkspace(label: 'one', words: ['a', 'b']);
        expect(w == other, isFalse);
        expect(w == w, isTrue);
        for (final h in [w.snapshot, other.snapshot]) {
          h.dispose();
        }
        for (final h in [w.counter, other.counter]) {
          h.dispose();
        }
        expect(() => w.snapshot.wordCount(), throwsStateError);
      },
    );

    test(
      'a returned tuple carries handles, alone and beside a value',
      () async {
        final (snap, counter) = snapshotAndCounter(words: ['a', 'b']);
        expect(snap.wordCount(), 2);
        expect(await counter.get(), 0);
        snap.dispose();
        counter.dispose();

        final (label, s2) = labelledSnapshot(label: 'tag');
        expect(label, 'tag');
        expect(s2.wordCount(), 1);
        s2.dispose();
      },
    );

    test('handle-carrying values nest in a list and in an enum variant', () {
      final ws = openWorkspaces(labels: ['a', 'b']);
      expect(ws.map((w) => w.label), ['a', 'b']);
      expect(ws.map((w) => w.snapshot.wordCount()), [1, 1]);
      for (final w in ws) {
        w.snapshot.dispose();
        w.counter.dispose();
      }

      final ss = slots(counts: Int64List.fromList([-1, 2]));
      expect(ss[0], isA<SlotEmpty>());
      final filled = ss[1] as SlotFilled;
      expect(filled.at, 2);
      expect(filled.snapshot.wordCount(), 2);
      filled.snapshot.dispose();
    });
  });

  group('GC-finalizer reclamation', () {
    // LiveProbe exposes its Rust-object lifetime through a process-global
    // count: `new` increments, Drop decrements. Both the explicit dispose()
    // path and the GC-finalizer path (`frustrate_finalize_LiveProbe`) run
    // Drop, so both return the count to baseline.

    test('explicit dispose runs the Rust-side Drop (all platforms)', () {
      final base = liveProbeCount();
      final p = LiveProbe.new_();
      expect(liveProbeCount(), base + 1);
      p.dispose();
      expect(
        liveProbeCount(),
        base,
        reason: 'dispose() must drop the Rust object',
      );
      p.dispose(); // idempotent
      expect(liveProbeCount(), base);
    });

    test('un-disposed handles are reclaimed by the GC finalizer', () async {
      // The claim the whole finalizer machinery exists for: a handle dropped
      // WITHOUT dispose() still frees its Rust object once the Dart object is
      // collected. GC is nondeterministic, so this drives it under allocation
      // pressure and gives the NativeFinalizer (which runs between event-loop
      // turns) turns to fire, polling until the count returns to baseline.
      // Empirically reclamation completes in the first churn iteration on the
      // standalone VM; the generous deadline only bounds a pathological run.
      final base = liveProbeCount();

      // Create handles in an inner scope and drop every reference without
      // calling dispose() — the objects become unreachable when `make`
      // returns.
      void make(int n) {
        var probes = List.generate(n, (_) => LiveProbe.new_());
        expect(liveProbeCount(), base + n);
        probes = const [];
        // Touch it so the assignment is not optimised away.
        if (probes.isNotEmpty) fail('unreachable');
      }

      make(100);

      void churnMemory() {
        for (var i = 0; i < 200; i++) {
          final junk = List<List<int>>.generate(
            500,
            (j) => List<int>.filled(64, j),
          );
          if (junk.length == -1) fail('unreachable');
        }
      }

      final deadline = DateTime.now().add(const Duration(seconds: 30));
      while (liveProbeCount() > base && DateTime.now().isBefore(deadline)) {
        churnMemory();
        await Future<void>.delayed(const Duration(milliseconds: 2));
      }

      expect(
        liveProbeCount(),
        base,
        reason:
            'the GC finalizer must drop the Rust objects behind handles '
            'that were never disposed, once they are collected',
      );
    }, testOn: 'vm');
    // Web note: the wasm builds use dart:core Finalizer, whose callbacks are
    // also GC-driven; forcing GC deterministically under dart2wasm/Chrome is
    // not reliable enough for a non-flaky assertion, so reclamation is pinned
    // on the native VM only. The deterministic dispose() path above runs on
    // every platform.
  });

  group('external types (custom bytes codec)', () {
    test('domain-typed round trip through user codecs on both sides', () {
      final bumped = bumpPlan(p: FakePlan('launch', 1));
      expect(bumped.title, 'launch');
      expect(bumped.revision, 2);
    });

    test('externs in collections, through the pool', () async {
      final merged = await mergePlans(
        plans: [FakePlan('a', 3), FakePlan('b', 7)],
      );
      expect(merged.title, 'a+b');
      expect(merged.revision, 8);
    });

    test('externs nested inside ordinary data structs', () {
      final line = schedule(
        m: Meeting(room: 'R4', plan: FakePlan('kickoff', 2)),
      );
      expect(line, 'kickoff rev2 in R4');
    });
  });

  group('callbacks', () {
    test('fire-and-forget: closure sees every call, in order', () async {
      final got = <int>[];
      await notifyN(n: 4, cb: got.add);
      await Future<void>.delayed(Duration.zero);
      expect(got, [1, 2, 3, 4]);
    });

    test('zero-arg callback', () async {
      var pings = 0;
      await ping(times: 3, done: () => pings++);
      await Future<void>.delayed(Duration.zero);
      expect(pings, 3);
    });

    test('closures never run during the bridge call that fired them', () async {
      // Identical contract on every platform: native delivery is a later
      // event-loop turn, web delivery is deferred to a microtask (the post
      // import fires mid-call; running the closure there could re-enter
      // the bridge against live borrows).
      final seen = <int>[];
      final sum = tally(items: Int64List.fromList([1, 2, 3]), cb: seen.add);
      expect(sum, 6);
      expect(
        seen,
        isEmpty,
        reason: 'the closure must not run during the sync call',
      );
      await Future<void>.delayed(Duration.zero);
      expect(seen, [1, 2, 3]);
    });

    test('stored closure: the on_change pattern', () async {
      final doc = TextDoc.new_();
      final lengths = <int>[];
      doc.onChange(cb: lengths.add);
      doc.splice(index: 0, delete: 0, insert: 'hello');
      doc.splice(index: 5, delete: 0, insert: '!');
      await Future<void>.delayed(Duration.zero);
      expect(lengths, [5, 6]);
      // Dispose retires the stored callback: a Rust-held
      // callback keeps the isolate alive until its owner drops. GC-finalize
      // does now retire it — with a `LeakedChannelError` naming the holder —
      // but only if the collector ever runs, and a pinned-idle isolate
      // allocates nothing, so dispose stays mandatory.
      doc.dispose();
    });

    test('callback from an actor executor', () async {
      final m = await Miner.new_(label: 'caller');
      await m.nthPrime(n: 10);
      final reports = <String>[];
      await m.report(cb: reports.add);
      await Future<void>.delayed(Duration.zero);
      expect(reports, ['caller:1']);
      await m.dispose();
    });

    test('web: refine (returning callback) is compile-time absent', () async {
      // An ACTOR DartFunction member can only use the blocking `call` (actor
      // methods cannot be `async fn`), which parks the actor's worker against
      // this thread's event loop — genuinely native-only, enforced
      // structurally like blockingGet. The portable path is `transform` below.
      final m = await Miner.new_(label: 'surface');
      expect(m.refine(x: 1, f: (v) => v), _RefineAbsentOnWeb.absent);
      await m.dispose();
    }, testOn: 'browser');

    // The awaitable value-returning callback: a Rust `async fn` awaits
    // `call_async` on the cooperative executor, so it runs on EVERY platform
    // — native VM, single-threaded Chrome, and threaded Chrome. (This is the
    // web-portable counterpart to the native-only blocking `transformSum` /
    // `refine`; those stay native-only because they cannot `.await`.)
    test(
      'transform: an awaited DartFunction returns the closure result',
      () async {
        expect(await transform(x: 21, f: (v) => v * 2), 42);
        expect(await transform(x: -5, f: (v) => v * v), 25);
      },
    );

    test(
      'transform: a throwing closure surfaces as this call\'s panic',
      () async {
        await expectLater(
          transform(x: 1, f: (v) => throw StateError('closure broke')),
          throwsA(
            isA<BridgePanicException>().having(
              (e) => e.message,
              'message',
              allOf(contains('callback threw'), contains('closure broke')),
            ),
          ),
        );
        // The bridge survives: later awaited calls still work.
        expect(await transform(x: 9, f: (v) => v + 1), 10);
      },
    );

    test('transform: the closure may re-enter the bridge (web deferral pin)', () async {
      // On single-threaded web the invocation event fires INSIDE the Rust
      // poll's synchronous `post` import; the router defers the closure to a
      // microtask so it runs on a clean stack. Proof: the closure itself makes
      // a bridge call — which would re-enter the module mid-poll if it ran
      // synchronously. Harmless on native (the closure already runs a port hop
      // away from the pool worker's poll).
      expect(await transform(x: 20, f: (v) => addI32(a: v, b: 1)), 21);
    });
  });

  // The mirror of a typed error on a bridged member: there Rust returns
  // `Err(E)` and
  // Dart catches `EException`; here Dart throws `EException` and the Rust body
  // receives `Err(E)`. Portable — `call_async` runs on native VM,
  // single-threaded Chrome and threaded Chrome — so these run everywhere, and
  // the blocking twin is pinned VM-only in native_blocking_test.dart.
  group('fallible callbacks (a Dart closure Rust can let fail)', () {
    test('a declared refusal reaches the Rust body as a value', () async {
      // ask_dart maps each outcome to a distinct number, so these three
      // assertions tell "handled as Err" apart from "returned a value" and
      // from "the call blew up".
      expect(
        await askDart(x: 3, f: (v) => v + 1),
        40,
        reason: 'Ok(4) -> 4 * 10',
      );
      expect(
        await askDart(
          x: 3,
          f: (v) => throw RefusalErrorException(
            const RefusalErrorBusy(retryInMs: 250),
          ),
        ),
        -250,
        reason: 'Err(Busy{250}) — the payload crossed, not just the tag',
      );
      expect(
        await askDart(
          x: 3,
          f: (v) => throw RefusalErrorException(const RefusalErrorNotAllowed()),
        ),
        -1,
        reason: 'the other variant, so the tag is read too',
      );
    });

    test('an UNDECLARED throw is still this call\'s panic', () async {
      // Hard contract: only the declared error is a value. A bug in the closure
      // must never reach Rust dressed as a business failure.
      await expectLater(
        askDart(x: 1, f: (v) => throw StateError('a bug, not a refusal')),
        throwsA(
          isA<BridgePanicException>().having(
            (e) => e.message,
            'message',
            allOf(contains('callback threw'), contains('a bug, not a refusal')),
          ),
        ),
      );
      // The bridge survives it.
      expect(await askDart(x: 2, f: (v) => v), 20);
    });

    test('a DIFFERENT typed exception is undeclared here, so it is a panic', () async {
      // The sharp version: `WithdrawErrorException` is a real generated
      // exception of this interface — just not the one `ask_dart` declares. If
      // the test were `is BridgeException` rather than `is RefusalErrorException`
      // this would silently become an `Err`, which is exactly the confusion the
      // typed-only rule exists to prevent.
      await expectLater(
        askDart(
          x: 1,
          f: (v) =>
              throw WithdrawErrorException(const WithdrawErrorAccountFrozen()),
        ),
        throwsA(
          isA<BridgePanicException>().having(
            (e) => e.message,
            'message',
            contains('callback threw'),
          ),
        ),
      );
    });

    test('retry: Rust asks again after a refusal', () async {
      // The pattern the gap made unwritable — the loop can only exist because
      // the failure is a value it can inspect.
      final seen = <int>[];
      final out = await askDartWithRetry(
        attempts: 3,
        f: (v) {
          seen.add(v);
          if (v < 3) {
            throw RefusalErrorException(RefusalErrorBusy(retryInMs: v * 10));
          }
          return v * 100;
        },
      );
      expect(seen, [1, 2, 3], reason: 'Rust drove three invocations');
      expect(out, 'ok after 3: 300');

      // And giving up: every attempt refuses.
      expect(
        await askDartWithRetry(
          attempts: 2,
          f: (v) => throw RefusalErrorException(
            const RefusalErrorBusy(retryInMs: 20),
          ),
        ),
        'gave up after 2 (busy 20ms)',
      );
    });

    test('Result<(), E>: no value, but the refusal still crosses', () async {
      var ran = 0;
      expect(await tellDart(x: 5, f: (v) => ran++), 'accepted');
      expect(ran, 1);
      expect(
        await tellDart(
          x: 5,
          f: (v) => throw RefusalErrorException(const RefusalErrorNotAllowed()),
        ),
        'not allowed',
      );
      expect(
        await tellDart(
          x: 5,
          f: (v) =>
              throw RefusalErrorException(const RefusalErrorBusy(retryInMs: 7)),
        ),
        'busy 7',
      );
    });
  });

  group('streams', () {
    test(
      'items in order, then done (drop-without-close ends the stream)',
      () async {
        final c = StreamController<int>();
        final items = c.stream.toList();
        await countTo(n: 5, sink: c);
        expect(await items, [1, 2, 3, 4, 5]);

        final empty = StreamController<int>();
        final none = empty.stream.toList();
        await countTo(n: 0, sink: empty);
        expect(await none, isEmpty);
      },
    );

    test('a broadcast controller is rejected loudly, and registers nothing', () async {
      // A broadcast controller has no pause protocol. The guard rejects it
      // *before* the handle is registered, so the loud reject leaks no
      // registration (the tearDown open-channel guard proves it — a throw after
      // openObject would orphan one and pin the isolate).
      final c = StreamController<int>.broadcast();
      await expectLater(
        countTo(n: 5, sink: c),
        throwsA(
          isA<StateError>().having(
            (e) => e.message,
            'message',
            contains('broadcast'),
          ),
        ),
      );
    });

    test('a controller with a preset onPause is rejected loudly', () async {
      // frustrate owns onPause/onResume for backpressure, so a preset one is a
      // loud error, not a silent clobber — same as onCancel.
      final c = StreamController<int>(onPause: () {});
      await expectLater(countTo(n: 5, sink: c), throwsA(isA<StateError>()));
    });

    test('terminal error arrives after the items that crossed', () async {
      final c = StreamController<int>();
      final events = <Object>[];
      final done = Completer<void>();
      c.stream.listen(
        events.add,
        onError: events.add,
        onDone: done.complete,
        cancelOnError: false,
      );
      await failAfter(n: 2, sink: c);
      await done.future;
      expect(events, hasLength(3));
      expect(events.sublist(0, 2), [1, 2]);
      expect(
        events[2],
        isA<BridgeException>().having(
          (e) => e.message,
          'message',
          contains('deliberate stream failure'),
        ),
      );
    });

    test('a producer panic rejects the call; native ends the stream, web '
        'leaves it open', () async {
      final c = StreamController<int>();
      final seen = <int>[];
      var done = false;
      c.stream.listen(seen.add, onDone: () => done = true);

      // The panic is attributed to the *call* on every platform (surface 1),
      // which is where a developer catches it or forwards it to Sentry.
      await expectLater(
        panicAfter(n: 2, sink: c),
        throwsA(isA<BridgePanicException>()),
      );
      // Let any terminal event drain to the listener (web delivers via a
      // microtask / postMessage hop).
      await Future<void>.delayed(const Duration(milliseconds: 100));

      // The two items that crossed before the panic arrive everywhere.
      expect(seen, [1, 2]);
      if (panicClosesStreams) {
        // Native: the panic unwinds, the sink drops, and drop-retire ends the
        // stream cleanly — a consumer awaiting `onDone` learns the producer is
        // gone rather than hanging forever.
        expect(
          done,
          isTrue,
          reason:
              'native: a producer panic drops the sink and ends the '
              'stream',
        );
        expect(c.isClosed, isTrue);
      } else {
        // Web (`panic=abort`): no destructor runs, so the stream is left open
        // by design. The panic is not lost — it was reported on the call
        // above; it simply does not close the stream.
        expect(
          done,
          isFalse,
          reason:
              'web: panic=abort runs no destructor, so the stream stays '
              'open — the panic is reported on the call, not the stream',
        );
        expect(c.isClosed, isFalse);
      }
    });

    test('an async producer panic ends the stream on native too', () async {
      // The async analogue: the panic lands on a resumed executor poll, so the
      // future (holding the sink) is dropped inside the executor's
      // `catch_unwind` *while unwinding* — the exact condition the deleted drop
      // guard used to short-circuit. Native must still end the stream; web
      // leaves it open like every panic there.
      final c = StreamController<int>();
      final seen = <int>[];
      var done = false;
      c.stream.listen(seen.add, onDone: () => done = true);

      await expectLater(
        panicAfterAsync(n: 2, sink: c),
        throwsA(isA<BridgePanicException>()),
      );
      await Future<void>.delayed(const Duration(milliseconds: 100));

      expect(seen, [1, 2]);
      if (panicClosesStreams) {
        expect(
          done,
          isTrue,
          reason:
              'native: an async producer panic drops the future during '
              'the unwind, and drop-retire ends the stream',
        );
        expect(c.isClosed, isTrue);
      } else {
        expect(done, isFalse);
        expect(c.isClosed, isFalse);
      }
    });

    test('a panic in a producer with sinks nested in a struct', () async {
      // The composed case: these two sinks are not parameters at all, so no
      // glue-level inspection of the signature could find them — only
      // drop-retire (native, on the unwind) closes them, which is why the
      // native fix belongs in `Drop` and not in per-signature glue.
      final evens = StreamController<int>();
      final odds = StreamController<int>();
      final evensSeen = <int>[];
      final oddsSeen = <int>[];
      var evensDone = false;
      var oddsDone = false;
      evens.stream.listen(evensSeen.add, onDone: () => evensDone = true);
      odds.stream.listen(oddsSeen.add, onDone: () => oddsDone = true);

      await expectLater(
        panicWithNestedSinks(
          out: Fanout(evens: evens, odds: odds),
        ),
        throwsA(isA<BridgePanicException>()),
      );
      await Future<void>.delayed(const Duration(milliseconds: 100));

      expect(evensSeen, [0]);
      expect(oddsSeen, [1]);
      if (panicClosesStreams) {
        expect(evensDone, isTrue);
        expect(oddsDone, isTrue);
      } else {
        expect(evensDone, isFalse);
        expect(oddsDone, isFalse);
      }
    });

    test(
      'fallible opener: the Err rejects the call, and only the call',
      () async {
        final ok = StreamController<int>();
        final okItems = ok.stream.toList();
        await guardedStream(ok: true, sink: ok);
        expect(await okItems, [1]);

        // An Err is an ordinary return, not an unwind, so the sink drops on the
        // way out and its end event closes the stream cleanly. The error lives
        // on the call, where every other member puts it — the bridge does not
        // also push a copy into the stream. Contrast the panic above, which
        // unwinds past the drop and strands the sink.
        final bad = StreamController<int>();
        final badItems = bad.stream.toList();
        await expectLater(
          guardedStream(ok: false, sink: bad),
          throwsA(
            isA<BridgeException>().having(
              (e) => e.message,
              'message',
              contains('guard was false'),
            ),
          ),
        );
        expect(await badItems, isEmpty);
      },
    );

    test('a fallible member treats all its sinks alike', () async {
      // The Err used to be routed into whichever StreamController the glue
      // found first, so these two were treated differently purely by
      // parameter order: `a` got a terminal error, `b` a clean close.
      final a = StreamController<int>();
      final b = StreamController<int>();
      final aItems = a.stream.toList();
      final bItems = b.stream.toList();
      await expectLater(
        guardedPair(a: a, b: b),
        throwsA(
          isA<BridgeException>().having(
            (e) => e.message,
            'message',
            contains('pair refused after one item each'),
          ),
        ),
      );
      // Both keep the item that crossed and both end cleanly — no asymmetry.
      expect(await aItems, [1]);
      expect(await bItems, [2]);
    });

    test(
      'the opening call is issued eagerly, never deferred to a listen',
      () async {
        // The deleted sugar issued nothing until the first listen. A stream
        // member is now an ordinary call: it fires when called, even if the
        // controller is never listened to.
        final before = streamOpens();
        final c = StreamController<int>();
        await countTo(n: 3, sink: c);
        expect(streamOpens(), before + 1);
      },
    );

    test(
      'the watch pattern: stored sink, pushed by later sync calls',
      () async {
        final doc = TextDoc.new_();
        final patches = <TextPatch>[];
        final c = StreamController<TextPatch>();
        doc.watch(sink: c);
        final sub = c.stream.listen(patches.add);
        doc.splice(index: 0, delete: 0, insert: 'hello');
        doc.splice(index: 0, delete: 5, insert: 'bye');
        // Items were posted during the splice calls; drain the event queue.
        await Future<void>.delayed(Duration.zero);
        expect(patches, hasLength(3)); // splice, then delete + splice
        expect((patches[0] as TextPatchSplice).text, 'hello');
        expect(patches[1], isA<TextPatchDelete>());
        expect((patches[2] as TextPatchSplice).text, 'bye');

        // After cancel nothing is delivered, even though the doc keeps
        // mutating — and the next push prunes the stored sink Rust-side.
        await sub.cancel();
        doc.splice(index: 0, delete: 0, insert: 'x');
        await Future<void>.delayed(const Duration(milliseconds: 20));
        expect(patches, hasLength(3));
      },
    );

    test('two watchers each get their own stream', () async {
      final doc = TextDoc.new_();
      final a = <TextPatch>[];
      final b = <TextPatch>[];
      final ca = StreamController<TextPatch>();
      final cb = StreamController<TextPatch>();
      doc.watch(sink: ca);
      doc.watch(sink: cb);
      ca.stream.listen(a.add);
      cb.stream.listen(b.add);
      doc.splice(index: 0, delete: 0, insert: 'hi');
      await Future<void>.delayed(Duration.zero);
      expect(a, hasLength(1));
      expect(b, hasLength(1));
      // Dispose retires both stored sinks (drop-retire) — otherwise the two
      // open registrations pin the isolate.
      doc.dispose();
    });

    test('cancel stops a live producer (real worker threads only)', () async {
      if (!Frustrate.instance.asyncIsParallel) {
        // Single-threaded web: the producer runs inline during the opening
        // call, so a cancel can never race it — the whole stream completes
        // before the subscription could cancel. Cooperative cancellation
        // is exercised where a real thread runs the producer (native,
        // threaded web); the delivery-side cancel is covered by the watch
        // test everywhere.
        return;
      }
      final got = <int>[];
      final c = StreamController<int>();
      final sub = c.stream.listen(got.add);
      final running = streamUntilCancelled(sink: c);
      final deadline = DateTime.now().add(const Duration(seconds: 5));
      while (got.length < 3 && DateTime.now().isBefore(deadline)) {
        await Future<void>.delayed(const Duration(milliseconds: 5));
      }
      expect(
        got.length,
        greaterThanOrEqualTo(3),
        reason: 'expected the producer to be streaming',
      );
      await sub.cancel();
      final observedAt = got.length;
      // The Rust producer observes the flag and stops.
      final observeBy = DateTime.now().add(const Duration(seconds: 5));
      while (!cancelObserved() && DateTime.now().isBefore(observeBy)) {
        await Future<void>.delayed(const Duration(milliseconds: 5));
      }
      expect(
        cancelObserved(),
        isTrue,
        reason: 'producer must observe the cancel flag',
      );
      // And nothing is delivered after cancel.
      await Future<void>.delayed(const Duration(milliseconds: 20));
      expect(got.length, observedAt);
      // The producer returned, so the call it opened completes normally.
      await running;
    });

    test(
      'pausing the subscription applies backpressure to the producer',
      () async {
        if (!Frustrate.instance.asyncIsParallel) {
          // Single-threaded web runs the producer inline during the opening
          // call, so there is no moment at which a pause could reach it — a
          // send() there never parks.
          return;
        }
        // A `send().await` producer, not `add`: only an awaited send observes
        // backpressure; a sync `add` loop is unbounded by design.
        final c = StreamController<int>();
        final sub = c.stream.listen((_) {});
        final running = streamWithBackpressure(sink: c);
        // cancel + drain in a finally: a producer left parked (paused, never
        // resumed) holds an open registration and pins
        // this isolate forever, so a failed assertion must not orphan it.
        try {
          final deadline = DateTime.now().add(const Duration(seconds: 5));
          while (streamPosted() < 3 && DateTime.now().isBefore(deadline)) {
            await Future<void>.delayed(const Duration(milliseconds: 5));
          }
          expect(
            streamPosted(),
            greaterThanOrEqualTo(3),
            reason: 'expected the producer to be streaming',
          );

          sub.pause();
          final atPause = streamPosted();
          await Future<void>.delayed(const Duration(milliseconds: 500));
          // Pausing the subscription fires onPause → pauseStream, and the
          // producer parks at its next send().await rather than running ahead
          // into an unbounded Dart-side buffer. Slack for the item already in
          // flight when the pause was signalled — but not half a second of it.
          expect(
            streamPosted() - atPause,
            lessThanOrEqualTo(8),
            reason: 'a paused subscription must park the awaiting producer',
          );

          // Resume wakes the parked producer; it advances again before cancel.
          sub.resume();
          final deadline2 = DateTime.now().add(const Duration(seconds: 5));
          while (streamPosted() <= atPause &&
              DateTime.now().isBefore(deadline2)) {
            await Future<void>.delayed(const Duration(milliseconds: 5));
          }
          expect(
            streamPosted(),
            greaterThan(atPause),
            reason: 'resume must wake the producer so it keeps sending',
          );
        } finally {
          await sub.cancel();
          await running;
        }
      },
    );

    // Primes to find per round in the progress-relay test. Sized so the four
    // rounds take tens of milliseconds even at `-c opt`, which is what keeps
    // the item→item spread an order of magnitude above the 2 ms floor. It is a
    // work knob, not a timing bar: raising it widens the margin, and the only
    // thing that shrinks it is the machine getting faster at trial division.
    const progressRounds = 30000;

    test('actor stream: progress items relay while the method runs', () async {
      final m = await Miner.new_(label: 'prospector');
      final sw = Stopwatch()..start();
      final at = <Duration>[];
      final items = <int>[];
      final done = Completer<void>();
      final c = StreamController<int>();
      c.stream.listen((p) {
        at.add(sw.elapsed);
        items.add(p);
      }, onDone: done.complete);
      await m.mineProgress(rounds: 4, n: progressRounds, sink: c);
      await done.future;
      expect(items, hasLength(4));
      expect(items.toSet(), hasLength(1), reason: 'same n, same prime');
      expect(await m.calls(), 4);
      // Real-time relay: each item leaves as its round finishes, so the four
      // arrivals are spread across the method's compute. Batched-at-completion
      // delivery would land all four in one port drain — successive microtasks,
      // tens of microseconds apart at most.
      //
      // The measure is first item → last item, NOT call → first item. Those are
      // not the same test: call→first is dominated by fixed call setup (~5 ms),
      // which the compute has to out-scale for the comparison to mean anything.
      // It did not at `-c opt`, where Rust is 10x faster and the whole compute
      // fell to ~0.6 ms against a 3.9 ms bar — so the old form of this assertion
      // failed 2 runs in 6 there while passing at `fastbuild`. Item→item
      // contains no setup at all, so it stays honest at any optimisation level,
      // and `progressRounds` keeps the true value ~16x the floor below
      // (measured `-c opt`: arrivals 19.5 / 30.4 / 41.5 / 52.7 ms).
      final spread = at.last - at.first;
      expect(
        spread,
        greaterThan(const Duration(milliseconds: 2)),
        reason:
            'items must stream during the method, not be batched at the '
            'end of it (arrivals: $at)',
      );
      await m.dispose();
    });

    test('cancelling an actor-owned stream stops delivery; native observes '
        'the flag', () async {
      final m = await Miner.new_(label: 'canceller');
      final c = StreamController<int>();
      final got = <int>[];
      final sub = c.stream.listen(got.add);
      // The budget is platform-split because cancel *observability* is too.
      // Native's producer reads the flag out of a
      // process-global registry the calling thread writes directly, so it
      // returns early and a huge budget costs nothing while guaranteeing it is
      // still mining at cancel time. A web actor's cancel is a pump message its
      // worker cannot process while a blocking method holds the only thread, so
      // the producer mines its whole budget: keep that bounded (the fixture is
      // a debug wasm build, ~2ms/round), but far larger than the handful of
      // rounds it takes to cancel below — so the post-cancel assertion is made
      // against a producer that is unquestionably still running.
      final running = m.mineUntilCancelled(
        budget: isNativeVm ? 20000 : 500,
        sink: c,
      );
      final deadline = DateTime.now().add(const Duration(seconds: 5));
      while (got.length < 3 && DateTime.now().isBefore(deadline)) {
        await Future<void>.delayed(const Duration(milliseconds: 5));
      }
      expect(
        got.length,
        greaterThanOrEqualTo(3),
        reason: 'expected the actor to be streaming',
      );

      await sub.cancel();
      // `onCancel` fires and tombstones the id within the awaited `cancel()`,
      // so nothing can slip in between here and the count.
      final atCancel = got.length;
      final observedAt = await running;

      // Universal, and the guarantee both languages actually make: cancel stops
      // *delivery*. On web that is asserted against the ~485 rounds the producer
      // kept mining afterwards, every one of them dropped by the tombstone.
      expect(
        got.length,
        atCancel,
        reason: 'nothing may be delivered after cancel',
      );
      if (isNativeVm) {
        // Producer *observation* is the part that is not uniform. Native's flag
        // is immediate, so a cooperative producer — one that checks `add`'s
        // return, as this one does — stops. On web an actor owns its wasm
        // instance and its own cancel registry, reachable only by a message the
        // busy worker never gets to; cancel is advisory there by contract,
        // exactly as a synchronous Dart producer or a
        // never-yielding Rust future is uncancellable. Deliberately not
        // asserting -1 on web: that would pin the limitation, not the contract.
        expect(
          observedAt,
          isNot(-1),
          reason: 'the actor\'s own producer must observe the cancel flag',
        );
      }
      await m.dispose();
    }, timeout: const Timeout(Duration(seconds: 60)));
  });

  group('tuples as records', () {
    test('a 2-tuple round-trips as a positional record', () {
      final r = echoPair(t: (7, 'hi'));
      expect(r, (7, 'hi'));
      expect(r.$1, 7);
      expect(r.$2, 'hi');
      expect(r, isA<(int, String)>());
    });

    test('a 3-tuple (arity > 2, mixed types) round-trips', () {
      expect(echoTriple(t: (9, true, 'z')), (9, true, 'z'));
      expect(echoTriple(t: (-1, false, '')), (-1, false, ''));
    });

    test('a tuple nested in a List round-trips element-wise', () {
      final r = echoPairs(ts: [(1, 2), (3, 4), (-5, 6)]);
      expect(r, [(1, 2), (3, 4), (-5, 6)]);
      expect(r, isA<List<(int, int)>>());
    });

    test('a nested tuple ((i32, i32), i32) composes', () {
      expect(echoNestedTuple(t: ((1, 2), 3)), ((1, 2), 3));
    });

    test('a tuple inside Option: null and present both survive', () {
      expect(echoOptPair(x: null), isNull);
      expect(echoOptPair(x: (5, 'q')), (5, 'q'));
    });

    test('a tuple as a Map value round-trips', () {
      final r = echoMapOfPair(m: {'a': (1, 2), 'b': (3, 4)});
      expect(r, {'a': (1, 2), 'b': (3, 4)});
      expect(r, isA<Map<String, (int, int)>>());
    });

    test('a record-typed struct field compares by value', () {
      final a = echoLabelled(v: const Labelled(id: 1, at: (10, 20)));
      expect(a, const Labelled(id: 1, at: (10, 20)));
      // Structural equality through the record field: distinct instances with
      // equal contents are equal, and hash equal (usable as a Set element).
      expect(a.at, (10, 20));
      expect({a}, contains(const Labelled(id: 1, at: (10, 20))));
      expect(a, isNot(const Labelled(id: 1, at: (10, 21))));
    });
  });

  group('BTreeMap/BTreeSet/VecDeque as Map/Set/List', () {
    test('BTreeMap round-trips as a Dart Map', () {
      final r = echoBtreeMap(m: {'a': 1, 'b': 2, 'c': 3});
      expect(r, {'a': 1, 'b': 2, 'c': 3});
      expect(r, isA<Map<String, int>>());
    });

    test('BTreeMap comes back in sorted key order', () {
      // Sent in insertion order z, a, m; a BTreeMap iterates sorted, and Dart's
      // insertion-ordered Map preserves that, so it returns a, m, z.
      final r = echoBtreeMap(m: {'z': 1, 'a': 2, 'm': 3});
      expect(r.keys.toList(), ['a', 'm', 'z']);
    });

    test('a BTreeMap built from unsorted pairs returns sorted', () {
      final r = sortedMap(pairs: [('banana', 2), ('apple', 1), ('cherry', 3)]);
      expect(r.keys.toList(), ['apple', 'banana', 'cherry']);
      expect(r, {'apple': 1, 'banana': 2, 'cherry': 3});
    });

    test('BTreeSet round-trips as a Dart Set', () {
      // No duplicate here, and none is possible: a Dart `Set` literal dedupes
      // at construction, so a repeated element never reaches the wire and
      // BTreeSet's own dedup is not observable from this side.
      final r = echoBtreeSet(s: {3, 1, 2});
      expect(r, {1, 2, 3});
      expect(r, isA<Set<int>>());
    });

    test('VecDeque round-trips as a Dart List', () {
      final r = echoDeque(d: Int64List.fromList([5, 4, 3]));
      expect(r, [5, 4, 3]);
      expect(r, isA<List<int>>());
    });
  });

  // Every shape here was a codegen error before endpoints became data. They
  // are tested as behaviour, not just as "it generates".
  group('Dart-object handles compose', () {
    test('a struct of two sinks IS two sinks — no cross-talk', () async {
      final evens = StreamController<int>();
      final odds = StreamController<int>();
      final gotEvens = evens.stream.toList();
      final gotOdds = odds.stream.toList();

      await splitParity(
        n: 6,
        out: Fanout(evens: evens, odds: odds),
      );

      expect(await gotEvens, [2, 4, 6]);
      expect(await gotOdds, [1, 3, 5]);
    });

    test('a handle rides beside a real return value', () async {
      // What FR0017 forbade: the stream is no longer forced to be the
      // function's only data channel.
      final echo = StreamController<int>();
      final echoed = echo.stream.toList();

      final total = await teeSum(
        values: Int64List.fromList([2, 3, 4]),
        echo: echo,
      );

      expect(total, 9, reason: 'the return value crosses as usual');
      expect(await echoed, [2, 3, 4], reason: 'and the channel carries too');
    });

    test('a Vec of sinks mints one channel per element, in order', () async {
      final controllers = List.generate(3, (_) => StreamController<int>());
      final firsts = controllers.map((c) => c.stream.toList()).toList();

      await broadcastIndex(sinks: controllers);

      for (var i = 0; i < controllers.length; i++) {
        expect(await firsts[i], [i], reason: 'element $i got its own channel');
      }
    });

    test('EventSink.addError is non-terminal; the stream continues', () async {
      final seen = <Object>[];
      final done = Completer<void>();
      final c = StreamController<int>();
      c.stream.listen(seen.add, onError: seen.add, onDone: done.complete);

      await sinkThenErrorThenMore(out: c);
      await done.future;

      expect(seen.length, 3, reason: 'item, error, item: $seen');
      expect(seen[0], 1);
      expect(seen[1], isA<BridgeException>());
      expect(seen[2], 2, reason: 'addError did NOT end the stream');
    });

    test('a plain dart:core Sink works as the tightest write end', () async {
      // Any Sink implementation — here a controller's own .sink.
      final c = StreamController<int>();
      final got = c.stream.toList();
      await fillPlainSink(n: 3, out: c.sink);
      expect(await got, [0, 1, 2]);
    });

    test('a returning Dart method reached through a struct', () async {
      final doubled = await applyTransform(
        x: 21,
        t: Transforms(double: (x) => x * 2),
      );
      expect(doubled, 42);
    });

    test(
      'stored handles outlive their call, then drop-retire closes them',
      () async {
        final evens = StreamController<int>();
        final odds = StreamController<int>();
        final gotEvens = evens.stream.toList();
        final gotOdds = odds.stream.toList();

        await stashFanout(
          out: Fanout(evens: evens, odds: odds),
        );
        // The opening call has returned; the channels are still live.
        expect(await pokeStashedFanout(v: 7), isTrue);
        expect(await pokeStashedFanout(v: 8), isTrue);

        await dropStashedFanout();

        // Dropping the Rust handles closes both Dart streams — the terminal
        // is what retires the registration, so `toList` completes.
        expect(await gotEvens, [7, 8]);
        expect(await gotOdds, [-7, -8]);
        expect(await pokeStashedFanout(v: 9), isFalse);
      },
    );

    test('cancelling one nested channel leaves its sibling running', () async {
      // The claim behind mirroring StreamController rather than a write-end
      // interface: the consumer's own StreamSubscription is the cancel
      // handle, and it reaches exactly one of the two Rust producers.
      if (!Frustrate.instance.asyncIsParallel) {
        // Single-threaded web runs the producer inline during the opening
        // call, so a cancel can never race it (same reason the top-level
        // cancel test skips here).
        return;
      }
      final evens = StreamController<int>();
      final odds = StreamController<int>();
      final gotEvens = <int>[];
      final gotOdds = <int>[];
      final evensSub = evens.stream.listen(gotEvens.add);
      odds.stream.listen(gotOdds.add);

      unawaited(
        streamBothUntilCancelled(
          out: Fanout(evens: evens, odds: odds),
        ),
      );
      final deadline = DateTime.now().add(const Duration(seconds: 5));
      while (gotEvens.length < 3 && DateTime.now().isBefore(deadline)) {
        await Future<void>.delayed(const Duration(milliseconds: 5));
      }
      expect(
        gotEvens.length,
        greaterThanOrEqualTo(3),
        reason: 'expected both producers to be streaming',
      );

      await evensSub.cancel();
      final observeBy = DateTime.now().add(const Duration(seconds: 5));
      while (nestedCancelState() & 1 == 0 &&
          DateTime.now().isBefore(observeBy)) {
        await Future<void>.delayed(const Duration(milliseconds: 5));
      }

      expect(
        nestedCancelState() & 1,
        1,
        reason: 'the cancelled channel must observe its own flag',
      );
      expect(
        nestedCancelState() & 2,
        0,
        reason: 'its sibling must be untouched',
      );
      final oddsAt = gotOdds.length;
      await Future<void>.delayed(const Duration(milliseconds: 50));
      expect(
        gotOdds.length,
        greaterThan(oddsAt),
        reason: 'the sibling producer keeps delivering',
      );
      await odds.close();
    });

    test('a declared interface round-trips against a stateful Dart object', () async {
      // The whole point of the declared form over a struct of closures: the
      // Dart side is a real class. `_Ledger` carries state BETWEEN methods and
      // between calls, which a bag of closures cannot without closing over
      // something the caller had to build by hand.
      final ledger = _Ledger(limit: 200);

      final out = await audit(amount: 120, currency: 'usd', a: ledger);

      expect(out, 'true/80', reason: 'approve returned true; reserve left 80');
      expect(ledger.notes, [
        'reviewing 120 usd',
        'approved',
      ], reason: 'two void method calls, in order, around the returning one');
      expect(ledger.approvals, [
        (120, 'usd'),
      ], reason: 'the tuple item arrived as TWO positional arguments');
      expect(ledger.reserved, 120, reason: 'state survives across methods');

      // The same object again — state accumulates across calls, which is the
      // capability a struct of closures does not have.
      final second = await audit(amount: 500, currency: 'eur', a: ledger);
      expect(
        second,
        'false/-420',
        reason: 'over the limit, and the ledger is now overdrawn',
      );
      expect(ledger.notes.last, 'denied');
      expect(ledger.reserved, 620);
    });

    test(
      'a declared refusal from an interface METHOD is a value, not a panic',
      () async {
        // Same contract as a fallible closure parameter, reached through a
        // method declaration — where the `Fallible` alias cannot go, so the
        // doc comment is the only thing carrying it.
        final ledger = _Ledger(limit: 1000, busyFor: 250);
        expect(
          await audit(amount: 10, currency: 'usd', a: ledger),
          'true/busy 250',
        );
      },
    );

    test('an interface nests in a struct and rides a Vec', () async {
      // "Handles are data" is unchanged by the new surface: one Dart object
      // per element, each with its own registrations.
      final a = _Ledger(limit: 100);
      final b = _Ledger(limit: 10);

      final approved = await runAudit(
        run: AuditRun(label: 'quarterly', auditors: [a, b]),
        amount: 50,
      );

      expect(approved, 1, reason: 'a approves 50, b does not');
      expect(a.notes, ['quarterly']);
      expect(b.notes, ['quarterly'], reason: 'no cross-talk between elements');
      expect(a.approvals, [(50, 'usd')]);
    });

    test('one Dart object registers one channel per method, each named', () async {
      // The cost of the per-method channel, made legible rather than left to
      // be discovered: the leak report names N entries for one object, so each
      // carries its own method. Native-only — `openChannelLabels` is the
      // native isolate-pinning diagnostic.
      if (!isNativeVm) return;
      final ledger = _Ledger(limit: 5);

      await stashAuditor(a: ledger);
      final labels = Frustrate.instance.openChannelLabels;
      expect(labels, hasLength(3), reason: 'three methods, three channels');
      expect(labels, contains('stash_auditor Auditor.note'));
      expect(labels, contains('stash_auditor Auditor.approve'));
      expect(labels, contains('stash_auditor Auditor.reserve'));

      expect(
        await pokeStashedAuditor(msg: 'later'),
        isTrue,
        reason: 'the object outlives the call that opened it',
      );
      expect(ledger.notes, ['later']);

      // Dropping the Rust struct drops all three handles together, so the
      // object is retired whole. (Moving one field out would retire only
      // that method.)
      await dropStashedAuditor();
      expect(await pokeStashedAuditor(msg: 'never'), isFalse);
    });

    test('a struct carrying a channel compares its channels by identity', () {
      // A channel is not data: two Fanouts over *different* controllers must
      // not compare equal, and nothing about a channel may be compared except
      // which one it is. That falls out of the ordinary structural `==`,
      // because `frDeepEquals` ends at `==` and a StreamController does not
      // override it.
      final a = StreamController<int>();
      final b = StreamController<int>();
      final one = Fanout(evens: a, odds: b);
      final two = Fanout(evens: a, odds: b);
      expect(one == two, isTrue, reason: 'the same two channels');
      expect(one.hashCode, two.hashCode);
      expect(
        Fanout(evens: b, odds: a) == one,
        isFalse,
        reason: 'different channels, by identity',
      );
      expect(one.copyWith(odds: a).odds, same(a));
      expect(one.toString(), contains('Fanout('));
      // Not left dangling for the next test.
      a.close();
      b.close();
    });
  });

  // A `#[bridge(data)] struct Page<T>` is ONE Dart class, `Page<Item>` is a
  // type on it, and the wire is one expanded declaration per instantiation.
  // These cross the shapes the expansion has to get right: a struct argument,
  // a container argument, a primitive one (where the class says `List<T>` and
  // the codec still takes the typed-list fast path), a generic enum at two
  // different argument pairs, a template through a template, and a handle.
  group('generic data types', () {
    test('one generic class serves every instantiation', () {
      final p = itemPage(n: 3);
      expect(p, isA<Page<Item>>());
      expect(p.total, 3);
      expect(p.items.map((i) => i.label), ['item0', 'item1', 'item2']);
      // `copyWith` and `toString` are the generic class's, so they hold at
      // every instantiation. Equality is too — see the last test in this group
      // for why `Page`'s is identity while `Wrapper`'s is by value.
      expect(p.copyWith(total: 9).total, 9);
      expect(p.copyWith(total: 9).items, p.items);
      expect(p.toString(), startsWith('Page('));
      expect(
        Wrapper<int>(
          page: const Page<int>(items: [1], total: 1),
          tag: 't',
        ),
        equals(
          Wrapper<int>(
            page: const Page<int>(items: [1], total: 1),
            tag: 't',
          ),
        ),
      );
    });

    test('a numeric argument keeps the typed-list fast path', () {
      // The class declares `List<T>`, so this value is a plain `List<int>` and
      // not an `Int32List` — and `Vec<i32>` still crosses on the bulk codec,
      // because the bulk writers test the representation at runtime.
      final r = doublePage(p: const Page<int>(items: [1, 2, 3], total: 7));
      expect(r.items, [2, 4, 6]);
      expect(r.total, 14);
      // …and a typed list is accepted where the same parameter is written,
      // which is what says the widening kept the memcpy path reachable.
      final typed = doublePage(
        p: Page<int>(items: Int32List.fromList([4, 5]), total: 1),
      );
      expect(typed.items, [8, 10]);
    });

    test('a container argument is its own instantiation', () {
      expect(
        flattenPage(
          p: const Page<List<String>>(
            items: [
              ['a', 'b'],
              ['c'],
            ],
            total: 2,
          ),
        ),
        ['a', 'b', 'c'],
      );
    });

    test('a generic enum is a sealed hierarchy at every instantiation', () {
      expect(eitherSum(e: EitherLeft<int, Item>(7)), 7);
      expect(
        eitherSum(e: EitherRight<int, Item>(Item(id: 42, label: 'x'))),
        42,
      );
      // A different argument pair is a different wire shape and the same two
      // Dart classes.
      final flipped = eitherFlip(e: EitherLeft<String, bool>('hi'));
      expect(flipped, isA<EitherRight<bool, String>>());
      expect((flipped as EitherRight<bool, String>).field0, 'hi');
      expect(
        eitherFlip(e: EitherRight<String, bool>(true)),
        equals(EitherLeft<bool, String>(true)),
      );
    });

    test('a template through a template expands to a fixpoint', () {
      // Nothing writes `Page<i64>`; it is reached only through `Wrapper<i64>`'s
      // own field, which is what makes the expansion a fixpoint.
      final w = rewrap(
        a: Wrapper<Item>(
          page: Page<Item>(items: [Item(id: 5, label: 'five')], total: 1),
          tag: 't',
        ),
      );
      expect(w, isA<Wrapper<int>>());
      expect(w.page.items, [5]);
      expect(w.tag, 't');
    });

    test('a generic that recurs at the same argument terminates', () {
      expect(
        chainLen(
          c: const Chain<int>(
            value: 1,
            next: Chain<int>(value: 2, next: Chain<int>(value: 3, next: null)),
          ),
        ),
        3,
      );
    });

    test('a generic typed error is one parameterized exception class', () {
      // `Refusal<i64>` and `Refusal<String>` share `RefusalException<T>`,
      // because `Refusal<i64>Exception` is not a Dart identifier. Both throw
      // it, each at its own argument, and `error` is the sealed value at that
      // argument — switchable, like any other typed error.
      expect(reserveSeats(n: 4), 4);
      expect(
        () => reserveSeats(n: 12),
        throwsA(
          isA<RefusalException<int>>().having(
            (e) => e.error,
            'error',
            equals(RefusalBusy<int>(2)),
          ),
        ),
      );
      expect(
        () => reserveSeats(n: -1),
        throwsA(
          isA<RefusalException<int>>().having(
            (e) => e.error,
            'error',
            isA<RefusalGone<int>>(),
          ),
        ),
      );
      expect(reserveNamed(name: 'ana'), 'ana');
      expect(
        () => reserveNamed(name: ''),
        throwsA(
          isA<RefusalException<String>>().having(
            (e) => e.error,
            'error',
            equals(RefusalBusy<String>('anonymous')),
          ),
        ),
      );
      // Still a BridgeException, so code that catches only that still catches
      // these — the property the generic form must not lose.
      expect(() => reserveSeats(n: 12), throwsA(isA<BridgeException>()));
    });

    test('a handle argument makes that instantiation return-only', () {
      // `Page<TextDoc>` reaches an opaque, so Rust mints one handle per item and
      // Dart owns exactly one wrapper for each — the same exactly-once a
      // `Vec<TextDoc>` return gets, one declaration deeper.
      final p = docPage(n: 2);
      expect(p.total, 2);
      expect(p.items, hasLength(2));
      for (final d in p.items) {
        expect(d.lenChars(), 0);
        d.dispose();
      }
      // One class serves every instantiation, and one equality rule serves
      // them all: `Page<TextDoc>` compares its handle field by identity (two
      // pages over different Rust objects are not equal) and `Page<Item>`
      // compares its values — neither reaching back into the other.
      final a = docPage(n: 1);
      final b = docPage(n: 1);
      expect(a == b, isFalse, reason: 'different handles, by identity');
      expect(a == a, isTrue);
      expect(
        itemPage(n: 3) == itemPage(n: 3),
        isTrue,
        reason: 'no handle in this instantiation, and no action at a distance',
      );
      for (final d in [...a.items, ...b.items]) {
        d.dispose();
      }
    });

    test('a generic impl puts each member on every instantiation', () {
      // `#[bridge] impl<T: Clone> Chain<T>` is written once and lands on
      // `Chain<int>` and `Chain<String>` as two extensions with two dispatch
      // ids each, calling two Rust monomorphizations. The extension resolves
      // from the STATIC type, which is what supplies the id.
      const ints = Chain<int>(
        value: 1,
        next: Chain<int>(value: 2, next: Chain<int>(value: 3, next: null)),
      );
      const words = Chain<String>(
        value: 'a',
        next: Chain<String>(value: 'b', next: null),
      );
      expect(ints.depth(), 3);
      expect(words.depth(), 2);
      // A parameter in return position: `int` at one instantiation, `String`
      // at the other, from one written member.
      expect(ints.head, 1);
      expect(words.head, 'a');
      // A `Self` return is the block's self type substituted per
      // instantiation, and the parameter comes back typed with it.
      expect(ints.grow(value: 0), const Chain<int>(value: 0, next: ints));
      expect(words.grow(value: 'z').head, 'z');
      // A static rides the extension and is reached through its name.
      expect(Chain$i64.one(value: 7), const Chain<int>(value: 7, next: null));
      expect(
        Chain$String.one(value: 'q'),
        const Chain<String>(value: 'q', next: null),
      );
    });

    test('a concrete impl adds its own instantiation and members', () {
      // `#[bridge] impl Page<Item>` bridges onto that one instantiation.
      // `Page` cannot take a generic impl: it is instantiated at `i32` and
      // `i64`, which are one Dart type, and at `TextDoc`, whose receiver
      // cannot be decoded Dart to Rust.
      final p = itemPage(n: 2);
      expect(p.labelOf(i: 1), 'item1');
      expect(p.labelOf(i: 9), '');
      // A by-value receiver moves a value the wire decoded, so the caller's
      // own object is untouched.
      final joined = p.concat(other: itemPage(n: 1));
      expect(joined.total, 3);
      expect(joined.items, hasLength(3));
      expect(
        p.items,
        hasLength(2),
        reason: 'the receiver was never that value',
      );
    });
  });
}

/// A Dart object implementing a generated `abstract interface class` — the
/// declared-mirror form, from `#[bridge(dart_interface)]`.
///
/// It is a real class, and that is the whole point: `notes`, `approvals` and
/// `reserved` are state its methods *share*, across methods and across calls.
/// A struct of closure fields can only carry state its constructor closed over,
/// which means the caller had to build the object anyway — here the type system
/// asks for it directly, and `implements` makes a missing method a compile
/// error naming the method rather than a missing named argument.
class _Ledger implements Auditor {
  _Ledger({required this.limit, this.busyFor});

  final int limit;

  /// When set, `reserve` refuses with the declared error instead of reserving.
  final int? busyFor;

  final List<String> notes = [];

  /// Both arguments of the two-parameter method, so a spread tuple that
  /// arrived in the wrong order would fail here rather than pass unnoticed.
  final List<(int, String)> approvals = [];

  int reserved = 0;

  @override
  void note(String line) => notes.add(line);

  @override
  bool approve(int amount, String currency) {
    approvals.add((amount, currency));
    return amount <= limit;
  }

  @override
  int reserve(int amount) {
    final busy = busyFor;
    if (busy != null) {
      throw RefusalErrorException(RefusalErrorBusy(retryInMs: busy));
    }
    reserved += amount;
    return limit - reserved;
  }
}

/// Capability pin (web): if the web surface ever regains `blockingGet`,
/// the instance member shadows this extension and the browser test above
/// fails. On native the instance member exists and this extension is
/// (correctly) shadowed — the file compiles on every platform.
///
/// The `ignore`s below are that shadowing seen from the analyzer's side: it
/// analyses as the VM, where the real member wins, so the extension method
/// reads as dead. It is not — the browser test calls it. Removing either
/// declaration silently un-pins the capability, which is what happened when
/// this was first "cleaned up".
extension _BlockingGetAbsentOnWeb on Counter {
  static const absent = 'blockingGet is absent from the web surface';
  // ignore: unused_element
  String blockingGet() => absent;
}

/// Same pin for the returning-callback member (the signature matches the
/// real one so the call site compiles on every platform; on native the
/// instance member shadows this).
extension _RefineAbsentOnWeb on Miner {
  static const absent = 'refine is absent from the web surface';
  // ignore: unused_element
  String refine({required int x, required int Function(int) f}) => absent;
}
