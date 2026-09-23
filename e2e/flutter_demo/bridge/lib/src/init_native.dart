import 'dart:io' show Platform;

import 'package:frustrate/frustrate.dart';

import 'package:demo_bridge/demo_rust.frustrate.dart';

/// Loads the bundled Rust library. The artifact's name and packaging differ
/// per platform, but the transport is the same everywhere:
///   - macOS: `libdemo_rust.dylib` in Contents/Frameworks (on the rpath).
///   - iOS: embedded, signed `demo_rust.framework` (dlopen-legal on device).
///   - Android/Linux: `libdemo_rust.so` bundled per-ABI / next to the runner.
/// This file is only compiled for native targets (init.dart's conditional
/// export routes web to init_web.dart), so `dart:io` is always available here.
Future<void> initBridge() async {
  if (Platform.isIOS) {
    FrustrateNative.init('demo_rust.framework/demo_rust');
  } else if (Platform.isMacOS) {
    FrustrateNative.init('libdemo_rust.dylib');
  } else {
    // Android and Linux.
    FrustrateNative.init('libdemo_rust.so');
  }
  // Loud stale-bindings guard: the loaded library and these bindings must
  // agree on the wire schema before any call reaches an unsafe deref.
  checkFrustrateSchema();
}
