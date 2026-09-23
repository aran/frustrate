/// Stands in for a protoc-generated message class: same conventions the
/// codegen defaults to ([FakePlan.fromBuffer] / [writeToBuffer]), same wire
/// form as the Rust side's BytesCodec impl (tests/test_api/src/api.rs):
/// 8-byte LE revision, then the UTF-8 title.
library;

import 'dart:convert';
import 'dart:typed_data';

class FakePlan {
  final String title;
  final int revision;
  FakePlan(this.title, this.revision);

  factory FakePlan.fromBuffer(List<int> bytes) {
    final data = ByteData.sublistView(Uint8List.fromList(bytes));
    return FakePlan(
      utf8.decode(bytes.sublist(8)),
      data.getInt64(0, Endian.little),
    );
  }

  Uint8List writeToBuffer() {
    final titleBytes = utf8.encode(title);
    final out = Uint8List(8 + titleBytes.length);
    ByteData.sublistView(out).setInt64(0, revision, Endian.little);
    out.setRange(8, out.length, titleBytes);
    return out;
  }
}
