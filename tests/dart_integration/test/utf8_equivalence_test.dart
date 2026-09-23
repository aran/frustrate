/// `BinaryWriter.writeString` emits one exact byte sequence, on every
/// platform, for the strings where an encoder has a choice to make.
///
/// This pins the **wire format** against the reference encoding and against
/// the Rust peer — what any change to the encoder has to preserve, and what
/// nothing else in the suite checks at this granularity. It compares against a
/// `utf8.encode`-built reference rather than only round-tripping, because a
/// round trip passes just as happily when both ends agree on the wrong bytes,
/// and Rust's `read_string` would not.
///
/// On dart2wasm `writeString` encodes through the browser rather than through
/// `utf8.encode` above `BinaryWriter.segmentThreshold`, so the `long-` rows
/// take that path and the short rows do not. Both must produce the same bytes.
///
/// It does not check that the browser path *fires* — a correct slow path
/// satisfies every row here. `string_encoder_hook_test` checks that.
///
/// The interesting rows are the ones where two conforming encoders could
/// legitimately disagree, and they are the reason this file exists:
///
///   * **Unpaired surrogates.** A lone high or low surrogate is not a Unicode
///     scalar and cannot be encoded. Dart's encoder substitutes U+FFFD; the
///     WHATWG encoding standard requires the same; nothing forces them to
///     agree on how many. Three cases here (high, low, trailing) plus a
///     reversed pair, which is two lone surrogates adjacent — the shape that
///     would catch an encoder pairing greedily.
///   * **A byte-order mark.** `readString` already documents that Dart's
///     *decoder* strips a leading BOM and that a length-prefixed wire field
///     must not, so a BOM is ordinary content here. Both a leading and an
///     interior one are checked, because an encoder that special-cased
///     position would only show up on one of them.
///
/// Runs on the VM here and on web via `web_test`. Both matter: the VM and the
/// web compile the same `binary_codec.dart` but not the same `String`, and the
/// BOM rows in particular exercise a workaround (`readString` putting back the
/// marks Dart's decoder strips) that has no other test.
library;

import 'dart:convert';
import 'dart:typed_data';

import 'package:frustrate/frustrate.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

/// Strings chosen so a disagreement between two conforming encoders shows up.
/// Named, because a failure should say which property broke.
final Map<String, String> _cases = {
  'ascii': 'hello world',
  'empty': '',
  'latin1': 'héllo çafé naïve',
  'bmp-cjk': '日本語テキスト',
  'astral-emoji': '👍🏽🇯🇵𝔘𝔫𝔦',
  'combining': 'éà',
  'bom-leading': '﻿document',
  'bom-interior': 'mid﻿dle',
  'spaces': 'a b',
  'lone-high-surrogate': 'x\uD83Dy',
  'lone-low-surrogate': 'x\uDE00y',
  'trailing-high-surrogate': 'x\uD83D',
  'reversed-pair': '\uDE00\uD83D',
  'max-bmp': '�￿',
  'mixed': 'aé日\u{1F600}z',
  // Long variants of each class. They exist because an encoder swap is likely
  // to be thresholded on length — the one measured here was — and a property
  // tested only below the threshold is not tested on the path that changed.
  // Found the hard way: with only the short BOM cases, a deliberately
  // BOM-eating encoder passed the whole suite.
  'long-ascii': 'abcdefghij0123456789abcdefghij0123456789',
  'long-astral': '👍👍👍👍👍👍👍👍👍👍👍👍👍👍👍👍👍👍👍👍👍👍',
  'long-mixed': 'aé日\u{1F600}z' * 12,
  'long-lone-surrogate': 'x\uD83Dy' * 16,
  'long-bom-leading': '﻿${'document padding to cross the threshold'}',
  'long-bom-interior': 'padding to cross the threshold mid﻿dle',
  'long-reversed-pair': '\uDE00\uD83D' * 24,
};

/// The bytes `writeString` is supposed to emit: an i64 little-endian length
/// prefix, then the UTF-8. Built independently of the writer on purpose — a
/// test that derived the expectation from the code under test would only ever
/// prove it agrees with itself.
Uint8List _reference(String s) {
  final bytes = utf8.encode(s);
  final out = BytesBuilder();
  final len = ByteData(8)..setInt64(0, bytes.length, Endian.little);
  out.add(len.buffer.asUint8List());
  out.add(bytes);
  return out.toBytes();
}

void main() {
  setUpAll(initBridge);

  group('writeString emits the reference encoding', () {
    _cases.forEach((name, value) {
      test(name, () {
        final w = BinaryWriter()..writeString(value);
        expect(
          w.takeBytes(),
          _reference(value),
          reason:
              'writeString did not emit the reference encoding of '
              '"$name" — the wire format is not platform-dependent, and a '
              'Rust peer decoding this would see the difference',
        );
      });
    });
  });

  group('and the bytes survive a round trip through Rust', () {
    _cases.forEach((name, value) {
      test(name, () async {
        // Rust's `read_string` validates UTF-8 and panics on malformed input,
        // so this also proves the encoder never emits an invalid sequence —
        // the failure mode a lone surrogate would produce if it were passed
        // through rather than substituted.
        //
        // The expectation comes from the runtime's own `BinaryReader`, not
        // from `utf8.decode`. Two reasons, and the first cost a red run:
        // `utf8.decode` **strips a leading BOM**, which is a document
        // convention that does not apply to a length-prefixed wire field —
        // `readString` documents this and deliberately puts the marks back, so
        // `echoString('\u{FEFF}x')` correctly returns '\u{FEFF}x' and only the
        // naive expectation disagreed. Second, a lone surrogate cannot survive
        // any UTF-8 round trip (it is not a scalar value), so the input itself
        // is not the expectation either. Reading back our own bytes gives the
        // right answer for both, and the group above has already pinned those
        // bytes against `utf8.encode`.
        final w = BinaryWriter()..writeString(value);
        final expected = BinaryReader(w.takeBytes()).readString();
        expect(
          await echoString(s: value),
          expected,
          reason: 'round trip changed "$name"',
        );
      });
    });
  });
}
