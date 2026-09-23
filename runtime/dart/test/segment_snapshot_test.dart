/// A referenced payload is read at assembly, not at `writeBytes` — and the two
/// must be indistinguishable.
///
/// Above [BinaryWriter.segmentThreshold] a byte payload is recorded by
/// reference rather than copied into the writer's buffer, which is what lets a
/// JS-backed payload reach the transport with its backing intact. The cost is
/// that the bytes are read at assembly rather than at `writeBytes`, so a
/// caller that mutated its buffer in between would send a different request
/// than it wrote.
///
/// Nothing can, on any wired path: encode and assembly sit in one synchronous
/// region with no await and no user code between them. This pins the property
/// that makes that safe — the pieces still describe the bytes as they were —
/// and the arithmetic that a caller-visible mutation would break.
library;

import 'dart:typed_data';

import 'package:frustrate/src/binary_codec.dart';
import 'package:test/test.dart';

void main() {
  test('a referenced payload assembles to exactly what was written', () {
    final payload = Uint8List(BinaryWriter.segmentThreshold * 2);
    for (var i = 0; i < payload.length; i++) {
      payload[i] = (i * 7 + 1) & 0xff;
    }

    final w = BinaryWriter(16);
    w.writeU32(0xfeedface);
    w.writeBytes(payload);
    w.writeU32(0x0badc0de);

    final pieces = w.takePieces();
    expect(
      pieces.length,
      greaterThan(1),
      reason:
          'a payload this size must be referenced, not copied — '
          'otherwise this test is pinning nothing',
    );

    var total = 0;
    for (final p in pieces) {
      total += p.length;
    }
    expect(
      total,
      w.totalLength,
      reason: 'the pieces must account for every byte of the request',
    );

    final r = BinaryReader(w.takeBytes());
    expect(r.readU32(), 0xfeedface);
    expect(r.readBytes(), payload);
    expect(r.readU32(), 0x0badc0de);
  });

  test('the payload is read at assembly, which is why the window must stay shut', () {
    final payload = Uint8List(BinaryWriter.segmentThreshold * 2);
    final w = BinaryWriter(16)..writeBytes(payload);

    // Mutating between writeBytes and assembly changes the request. No wired
    // path can do this — there is no await and no user code in the region —
    // and this is the arithmetic showing what the region protects, so a future
    // path that introduces a suspension point fails here rather than shipping
    // a request its caller never wrote.
    payload[0] = 0xff;
    expect(
      BinaryReader(w.takeBytes()).readBytes()[0],
      0xff,
      reason:
          'a referenced payload reflects assembly-time bytes; any path '
          'that lets the caller run in between must copy instead',
    );
  });

  test(
    'below the threshold the payload is copied, so mutation cannot reach it',
    () {
      final payload = Uint8List(8);
      final w = BinaryWriter(16)..writeBytes(payload);
      payload[0] = 0xff;
      expect(
        BinaryReader(w.takeBytes()).readBytes()[0],
        0,
        reason: 'a copied payload is a snapshot taken at writeBytes',
      );
    },
  );
}
