/// Corrupt-envelope / malformed-input decode tests.
///
/// The wire-schema fingerprint guards *structural* drift between a
/// generator and its bindings. It says nothing about how the decode path
/// behaves on a *malformed byte stream* that would still pass the fingerprint:
/// a truncated buffer, a bad status byte, an out-of-range length prefix,
/// garbage where a known type is expected. The bar for every such input is a
/// **loud, attributable error** — never a silent wrong value, never a bare
/// crash whose message doesn't name the codec.
///
/// These tests inject crafted bytes straight into the runtime decode path
/// (`BinaryReader` in binary_codec.dart and the response-envelope split in
/// envelope.dart) — no bridge, no native library, no init. That keeps them
/// pure-Dart and platform-agnostic, so they run identically on the VM and on
/// dart2wasm/web. Every generated `_decX` in the bindings ultimately delegates
/// to these primitives, so pinning them here pins the whole decode surface's
/// failure mode at its root.
library;

import 'dart:convert';
import 'dart:typed_data';

import 'package:frustrate/src/binary_codec.dart';
// envelope.dart is runtime-internal (not re-exported from frustrate.dart), so
// reach it by its implementation URI — this is a white-box test of the decode
// path, exactly the layer that must fail loudly.
import 'package:frustrate/src/envelope.dart';
import 'package:frustrate/frustrate.dart'
    show BridgeException, BridgePanicException, ContentionException;
import 'package:test/test.dart';

/// A response buffer with `status` and no payload.
Uint8List _status(int status) => Uint8List.fromList([status]);

/// A response buffer: status byte followed by an i64-length-prefixed string.
Uint8List _statusWithString(int status, String message) {
  final w = BinaryWriter();
  w.writeU8(status);
  w.writeString(message);
  return w.takeBytes();
}

/// Matches a StateError whose message contains all [needles].
Matcher _codecError(List<String> needles) => throwsA(
  isA<StateError>().having(
    (e) => e.message,
    'message',
    allOf([contains('frustrate codec'), ...needles.map(contains)]),
  ),
);

void main() {
  // ---------------------------------------------------------------------------
  // Class 1: truncated buffer — a read runs past the end of the bytes.
  // Every fixed-width and length-prefixed read must raise the codec's own
  // attributable "truncated buffer", never read adjacent/undefined memory.
  // ---------------------------------------------------------------------------
  group('truncated fixed-width reads', () {
    test('readU8 on an empty buffer', () {
      expect(
        () => BinaryReader(Uint8List(0)).readU8(),
        _codecError(['truncated buffer']),
      );
    });

    test('multi-byte reads that lack their full width', () {
      // 3 bytes present, each read wants more than remains.
      expect(
        () => BinaryReader(Uint8List(3)).readU32(),
        _codecError(['truncated buffer']),
      );
      expect(
        () => BinaryReader(Uint8List(3)).readI64(),
        _codecError(['truncated buffer']),
      );
      expect(
        () => BinaryReader(Uint8List(3)).readF64(),
        _codecError(['truncated buffer']),
      );
      expect(
        () => BinaryReader(Uint8List(3)).readU64(),
        _codecError(['truncated buffer']),
      );
      expect(
        () => BinaryReader(Uint8List(3)).readHandle(),
        _codecError(['truncated buffer']),
      );
    });

    test('a read that consumes to the exact end then one byte too far', () {
      final r = BinaryReader(Uint8List(4))..readU32(); // consumes all 4
      expect(r.isAtEnd, isTrue);
      expect(r.readU8, _codecError(['truncated buffer']));
    });
  });

  // ---------------------------------------------------------------------------
  // Class 2: length prefix exceeds the buffer. A String/Vec/Map count is an
  // i64; a corrupt one that promises more bytes than exist must truncate-fail,
  // not allocate/return garbage.
  // ---------------------------------------------------------------------------
  group('out-of-range length prefixes', () {
    test('a plainly-too-large String length', () {
      final w = BinaryWriter()..writeLen(1000000); // no bytes follow
      expect(
        () => BinaryReader(w.takeBytes()).readString(),
        _codecError(['truncated buffer']),
      );
    });

    test('a plainly-too-large Vec/Bytes length', () {
      final w = BinaryWriter()..writeLen(1000000);
      expect(
        () => BinaryReader(w.takeBytes()).readBytes(),
        _codecError(['truncated buffer']),
      );
    });

    test('a NEGATIVE length prefix (i64 high bit set) is rejected as such', () {
      // Rust usize crosses as i64; a negative value can never be a real length.
      // It must be caught for what it is, not silently treated as 0 or huge.
      final w = BinaryWriter()..writeI64(-1);
      expect(
        () => BinaryReader(w.takeBytes()).readString(),
        _codecError(['negative usize']),
      );
    });

    test('a length near i64::MAX does not overflow past the bounds check', () {
      // REGRESSION GUARD. A naive `pos + len > length` check overflows to a
      // negative int for len ~ 2^63 and slips through, surfacing a bare
      // RangeError from sublistView instead of the codec's attributable error.
      // The bounds check must compare against remaining space instead.
      final w = BinaryWriter()..writeI64(0x7FFFFFFFFFFFFFFF);
      expect(
        () => BinaryReader(w.takeBytes()).readString(),
        _codecError(['truncated buffer']),
      );
      final w2 = BinaryWriter()..writeI64(0x7FFFFFFFFFFFFFFF);
      expect(
        () => BinaryReader(w2.takeBytes()).readBytes(),
        _codecError(['truncated buffer']),
      );
    });
  });

  // ---------------------------------------------------------------------------
  // Class 3: garbage where a known type is expected.
  // ---------------------------------------------------------------------------
  group('invalid encodings for known types', () {
    test('a bool byte outside {0, 1} is rejected, not coerced', () {
      // 2..255 are not valid bools; silently mapping them to true/false would
      // be a wrong value. readBool is the validating primitive the generated
      // decode should route option/bool tags through.
      for (final bad in [2, 3, 127, 255]) {
        expect(
          () => BinaryReader(Uint8List.fromList([bad])).readBool(),
          _codecError(['invalid bool byte', '$bad']),
          reason: 'byte $bad must not decode to a bool',
        );
      }
    });

    test('valid bool bytes still decode', () {
      expect(BinaryReader(Uint8List.fromList([0])).readBool(), isFalse);
      expect(BinaryReader(Uint8List.fromList([1])).readBool(), isTrue);
    });

    test('garbage UTF-8 in a String field fails loudly and attributably', () {
      // A valid length prefix followed by bytes that are not valid UTF-8. Dart's
      // utf8.decode throws a bare FormatException; the codec must re-wrap it so
      // the failure names the codec and the buffer offset, and must NOT fall
      // back to a lossy U+FFFD substitution (that would be a silent wrong value
      // reaching the caller as a "successful" decode).
      final b = BytesBuilder()
        ..add((BinaryWriter()..writeLen(3)).takeBytes())
        ..add(Uint8List.fromList([0xFF, 0xFE, 0xFD]));
      expect(
        () => BinaryReader(b.toBytes()).readString(),
        _codecError(['invalid UTF-8', 'offset']),
      );
    });

    test('a lone continuation / overlong sequence is also rejected', () {
      // 0xC0 0x80 is an overlong encoding of NUL — classic malformed UTF-8.
      final b = BytesBuilder()
        ..add((BinaryWriter()..writeLen(2)).takeBytes())
        ..add(Uint8List.fromList([0xC0, 0x80]));
      expect(
        () => BinaryReader(b.toBytes()).readString(),
        _codecError(['invalid UTF-8']),
      );
    });

    test('well-formed UTF-8 (incl. multibyte) still round-trips', () {
      final w = BinaryWriter()..writeString('héllo \u{1F600}');
      expect(BinaryReader(w.takeBytes()).readString(), 'héllo \u{1F600}');
    });
  });

  // ---------------------------------------------------------------------------
  // Class 4: response-envelope status byte. decodeEnvelope reads a leading
  // status; anything outside the ok/error/panic/contention response set must
  // raise loudly rather than return a payload reader positioned at garbage.
  // ---------------------------------------------------------------------------
  group('response envelope status', () {
    test('empty response (no status byte) is a truncated buffer', () {
      expect(
        () => decodeEnvelope(Uint8List(0)),
        _codecError(['truncated buffer']),
      );
    });

    test('an unknown status byte is a loud, named error', () {
      for (final bad in [42, 99, 200, 255]) {
        expect(
          () => decodeEnvelope(_status(bad)),
          throwsA(
            isA<StateError>().having(
              (e) => e.message,
              'message',
              allOf(contains('unknown envelope status'), contains('$bad')),
            ),
          ),
          reason: 'status $bad is not a response status',
        );
      }
    });

    test('a channel-only status (stream item/end/callback) is not a valid '
        'response envelope', () {
      // 4/5/6 are legal on the channel keyed by channel id, but they are never
      // a *response* to a call; if one lands in decodeEnvelope it must reject,
      // not be mistaken for ok/payload.
      for (final channelOnly in [
        statusStreamItem,
        statusStreamEnd,
        statusCallbackCall,
      ]) {
        expect(
          () => decodeEnvelope(_status(channelOnly)),
          throwsA(
            isA<StateError>().having(
              (e) => e.message,
              'message',
              contains('unknown envelope status'),
            ),
          ),
          reason: 'status $channelOnly must not decode as a response',
        );
      }
    });

    test('a well-formed OK envelope yields a reader at the payload', () {
      final w = BinaryWriter()
        ..writeU8(0) // status ok
        ..writeI64(0x0123456789abcdef);
      final r = decodeEnvelope(w.takeBytes());
      expect(r.readI64(), 0x0123456789abcdef);
      expect(r.isAtEnd, isTrue);
    });
  });

  // ---------------------------------------------------------------------------
  // Class 5: error-carrying envelopes. A non-ok status is followed by a
  // string message; each maps to its own attributable exception type, and a
  // truncated message must still fail loudly rather than surface an empty or
  // garbage message.
  // ---------------------------------------------------------------------------
  group('error-carrying envelopes', () {
    test('each non-ok status maps to its typed exception with the message', () {
      expect(
        () => decodeEnvelope(_statusWithString(1, 'boom')),
        throwsA(
          isA<BridgeException>().having((e) => e.message, 'message', 'boom'),
        ),
      );
      expect(
        () => decodeEnvelope(_statusWithString(2, 'kaput')),
        throwsA(
          isA<BridgePanicException>().having(
            (e) => e.message,
            'message',
            'kaput',
          ),
        ),
      );
      expect(
        () => decodeEnvelope(_statusWithString(3, 'busy')),
        throwsA(
          isA<ContentionException>().having(
            (e) => e.message,
            'message',
            'busy',
          ),
        ),
      );
    });

    test(
      'an error status with a truncated message string still fails loudly',
      () {
        // status=1 (error), then a length prefix promising 50 bytes with none
        // present: the message decode must truncate-fail, not hand back a
        // half-built or empty error string.
        final w = BinaryWriter()
          ..writeU8(1)
          ..writeLen(50);
        expect(
          () => decodeEnvelope(w.takeBytes()),
          _codecError(['truncated buffer']),
        );
      },
    );

    test('an error status with garbage-UTF-8 message fails attributably', () {
      final b = BytesBuilder()
        ..add(
          (BinaryWriter()
                ..writeU8(1)
                ..writeLen(2))
              .takeBytes(),
        )
        ..add(Uint8List.fromList([0xFF, 0xFF]));
      expect(() => decodeEnvelope(b.toBytes()), _codecError(['invalid UTF-8']));
    });
  });

  // ---------------------------------------------------------------------------
  // Class 6: documented boundaries — corruption classes whose enforcement is
  // structurally NOT the runtime primitive's job. These tests pin the current
  // safe-enough behavior and the reason, so a future reader knows the gap is
  // deliberate, not missed.
  // ---------------------------------------------------------------------------
  group('boundaries (enforcement lives elsewhere by construction)', () {
    test('the OPTION discriminant is validated because generated decode routes '
        'through readBool — readU8()==1 would have silently coerced', () {
      // The generated bindings now decode Option<T> as `r.readBool() ? .. : null`
      // (codegen fix, emit_dart.rs). A corrupt tag byte of 2 is therefore a
      // loud codec error, not a silent None — pinned end-to-end on the
      // generated path in corrupt_decode_test.dart. Here we keep the primitive
      // contrast so a regression in either read is caught: the old raw
      // readU8()==1 would coerce a 2 to false (silent None); the readBool the
      // generator now emits rejects it.
      final coerced = BinaryReader(Uint8List.fromList([2])).readU8() == 1;
      expect(
        coerced,
        isFalse,
        reason: 'the retired readU8()==1 pattern silently coerced 2 -> None',
      );
      expect(
        () => BinaryReader(Uint8List.fromList([2])).readBool(),
        _codecError(['invalid bool byte']),
        reason: 'readBool is the validating primitive the generator emits',
      );
    });

    test('trailing bytes after a value are tolerated by the ENVELOPE primitive '
        '— the end-of-buffer assertion belongs to the typed (generated) decode', () {
      // decodeEnvelope returns a reader positioned at the payload; it cannot
      // know the caller\'s return type, so it cannot assert the buffer is fully
      // consumed. That check belongs in the generated response decode, which
      // now calls `assertConsumed()` after decoding the return value (codegen
      // gap #2; see corrupt_decode_test.dart for the end-to-end pin). isAtEnd
      // is the same hook, exposed here. At the envelope layer this is not
      // silent CORRUPTION of the decoded value (the consumed prefix decodes
      // correctly) and Dart is memory-safe, so there is no UB — only
      // tolerance. We pin isAtEnd as the available hook.
      final w = BinaryWriter()
        ..writeU8(0) // ok
        ..writeI64(7)
        ..writeI64(999); // trailing garbage a single-value decode won\'t read
      final r = decodeEnvelope(w.takeBytes());
      expect(r.readI64(), 7);
      expect(
        r.isAtEnd,
        isFalse,
        reason: 'the hook a generated decode would check to reject trailers',
      );
    });

    test('a fixed-size numeric read of the wrong width is caught by truncation, '
        'not misread — sanity that width mismatch cannot silently succeed', () {
      // If Rust wrote an i32 (4 bytes) but Dart reads i64 (8), the extra 4
      // bytes aren\'t there -> truncation. The inverse (i64 written, i32 read)
      // is a wrong VALUE but is a schema-fingerprint concern (structural skew),
      // not a byte-corruption one; the fingerprint makes it unreachable in a
      // single-generator build.
      final w = BinaryWriter()..writeI32(5);
      expect(
        () => BinaryReader(w.takeBytes()).readI64(),
        _codecError(['truncated buffer']),
      );
    });
  });

  // A direct sanity check that the wrapped UTF-8 error preserves the underlying
  // reason, so operators can still see *why* it was malformed.
  test('wrapped UTF-8 error carries the decoder reason through', () {
    final b = BytesBuilder()
      ..add((BinaryWriter()..writeLen(1)).takeBytes())
      ..add(Uint8List.fromList([0x80])); // lone continuation byte
    try {
      BinaryReader(b.toBytes()).readString();
      fail('expected a StateError');
    } on StateError catch (e) {
      expect(e.message, contains('frustrate codec'));
      // The original FormatException message (from utf8.decode) is threaded in.
      final ref = () {
        try {
          utf8.decode(Uint8List.fromList([0x80]));
        } on FormatException catch (fe) {
          return fe.message;
        }
        return '';
      }();
      expect(e.message, contains(ref));
    }
  });
}
