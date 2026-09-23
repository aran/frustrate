/// Which encoder `writeString` uses, and that the choice is not observable in
/// the bytes.
///
/// On dart2wasm a string at or above `BinaryWriter.segmentThreshold` is
/// encoded by the browser; everywhere else, and below that length, by
/// `utf8.encode`. Both must emit the same wire bytes, so the choice is an
/// implementation detail and nothing downstream — Rust's `read_string` least
/// of all — can tell which ran.
///
/// `utf8_equivalence_test` pins the wire format across the strings where two
/// conforming encoders could disagree, but it is satisfied by either encoder
/// and so cannot say which is installed. That is what the first case here
/// specifies. The byte equality is checked against `utf8.encode` directly
/// rather than through a round trip, because a round trip agrees just as
/// happily when both ends are wrong.
library;

import 'dart:convert';
import 'dart:typed_data';

import 'package:frustrate/frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

/// Long enough to clear `BinaryWriter.segmentThreshold`, and awkward enough
/// that an encoder with a choice to make has to make it: a leading BOM, an
/// interior BOM, a lone high surrogate, an astral pair, and combining marks.
const _awkward =
    '\u{FEFF}head'
    '\u{1F600}é'
    'a\uD800b'
    '\u{FEFF}interior'
    'tail';

String get _long => _awkward * 64;

Uint8List _encoded(String s) {
  final w = BinaryWriter(16)..writeString(s);
  return w.takeBytes();
}

void main() {
  setUpAll(initBridge);

  test('the browser encoder is installed on web and absent on the VM', () {
    const onWeb = bool.fromEnvironment('dart.library.js_interop');
    expect(
      BinaryWriter.stringEncodeHook != null,
      onWeb,
      reason: onWeb
          ? 'the web runtime must install the browser encoder at init — '
                'without it writeString walks a JS string one code unit at a '
                'time and the optimization is silently absent'
          : 'the VM has no browser encoder to install, and binary_codec must '
                'stay free of dart:js_interop',
    );
  });

  test(
    'at or above the threshold, the browser encoder emits the same bytes',
    () {
      final long = _long;
      expect(
        long.length,
        greaterThan(BinaryWriter.segmentThreshold),
        reason:
            'below the threshold the other encoder runs and this case '
            'would be specifying the wrong one',
      );

      final viaWriter = _encoded(long);

      final ref = BytesBuilder();
      final refBytes = utf8.encode(long);
      final lenWriter = BinaryWriter(16)..writeLen(refBytes.length);
      ref.add(lenWriter.takeBytes());
      ref.add(refBytes);

      expect(
        viaWriter,
        ref.toBytes(),
        reason:
            'the two encoders disagree on the wire. Whichever runs, the '
            'bytes must be identical — Rust read_string is the peer and it '
            'does not negotiate',
      );
    },
  );

  test('below the threshold, `utf8.encode` runs and emits the same bytes', () {
    const short = '\u{FEFF}hi\uD800';
    expect(short.length, lessThan(BinaryWriter.segmentThreshold));
    final viaWriter = _encoded(short);
    final refBytes = utf8.encode(short);
    expect(viaWriter.sublist(viaWriter.length - refBytes.length), refBytes);
  });
}
