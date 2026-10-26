import 'package:frustrate/frustrate.dart';

import 'package:demo_bridge/demo_rust.frustrate.dart';

/// Loads the Rust library through the bindings' code asset: the build decides
/// which file backs `package:demo_bridge/demo_rust` on each platform, so the
/// transport is the same everywhere. This file is only compiled for native
/// targets (init.dart's conditional export routes web to init_web.dart).
Future<void> initBridge() async {
  FrustrateNative.init(demoRustBridge);
  // Loud stale-bindings guard: the loaded library and these bindings must
  // agree on the wire schema before any call reaches an unsafe deref.
  checkFrustrateSchema();
}
