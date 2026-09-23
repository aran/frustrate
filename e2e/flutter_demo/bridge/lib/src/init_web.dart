import 'package:frustrate/frustrate.dart';

import 'package:demo_bridge/demo_rust.frustrate.dart';

/// Fetches the wasm module bundled as a web asset at the site root.
Future<void> initBridge() async {
  await FrustrateWeb.initFromUrl('demo_rust.wasm');
  // Loud stale-bindings guard: the wasm module and these bindings must agree
  // on the wire schema before any call reaches an unsafe deref.
  checkFrustrateSchema();
}
