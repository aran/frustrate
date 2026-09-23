import 'dart:io' show Platform;

import 'package:frustrate/frustrate.dart';

import 'package:iroh_bridge/iroh_rust.frustrate.dart';

/// Loads the bundled Rust library. The artifact's name and packaging differ
/// per platform, but the transport is the same everywhere:
///   - macOS: `libiroh_rust.dylib` in Contents/Frameworks (on the rpath).
///   - iOS: embedded, signed `iroh_rust.framework` (dlopen-legal on device).
///   - Android/Linux: `libiroh_rust.so` bundled per-ABI / next to the runner.
Future<void> initBridge() async {
  if (Platform.isIOS) {
    FrustrateNative.init('iroh_rust.framework/iroh_rust');
  } else if (Platform.isMacOS) {
    FrustrateNative.init('libiroh_rust.dylib');
  } else {
    // Android and Linux.
    FrustrateNative.init('libiroh_rust.so');
  }
  // Loud stale-bindings guard: the loaded library and these bindings must
  // agree on the wire schema before any call reaches an unsafe deref.
  checkFrustrateSchema();
}
