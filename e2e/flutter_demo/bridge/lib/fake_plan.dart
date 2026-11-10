/// Stands in for a protoc-generated message class: same conventions the
/// codegen defaults to ([FakePlan.fromBuffer] / [writeToBuffer]), same wire
/// form as the Rust side's BytesCodec impl (bridge/src/api.rs): 8-byte LE
/// revision, then the UTF-8 title.
library;

import 'dart:convert';
import 'dart:typed_data';

const _2p32 = 4294967296;

class FakePlan {
  final String title;
  final int revision;
  FakePlan(this.title, this.revision);

  factory FakePlan.fromBuffer(List<int> bytes) {
    final data = ByteData.sublistView(Uint8List.fromList(bytes));
    // Two 32-bit halves, because dart2js has no `ByteData.getInt64`.
    final revision =
        data.getInt32(4, Endian.little) * _2p32 +
        data.getUint32(0, Endian.little);
    return FakePlan(utf8.decode(bytes.sublist(8)), revision);
  }

  Uint8List writeToBuffer() {
    final titleBytes = utf8.encode(title);
    final out = Uint8List(8 + titleBytes.length);
    final view = ByteData.sublistView(out);
    final lo = revision % _2p32;
    view.setUint32(0, lo, Endian.little);
    view.setInt32(4, (revision - lo) ~/ _2p32, Endian.little);
    out.setRange(8, out.length, titleBytes);
    return out;
  }
}
