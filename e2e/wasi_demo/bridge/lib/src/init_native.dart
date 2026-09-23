import 'package:frustrate/frustrate.dart';

import 'package:wasi_bridge/wasi_rust.frustrate.dart';

/// Loads the bundled Rust library — `libwasi_rust.dylib` in
/// Contents/Frameworks, on the rpath.
///
/// Note what is *not* here: any mention of wasi. The platform choice lives
/// entirely in `//:wasi_rust.wasm`'s `platform` attribute and nothing about it
/// reaches Dart.
Future<void> initBridge() async {
  FrustrateNative.init('libwasi_rust.dylib');
  // Loud stale-bindings guard: the loaded library and these bindings must
  // agree on the wire schema before any call reaches an unsafe deref.
  checkFrustrateSchema();
}
