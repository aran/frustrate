import 'package:frustrate/frustrate.dart';

import 'package:iroh_bridge/iroh_rust.frustrate.dart';

/// Fetches the wasm module bundled as a web asset at the site root, and the
/// wasm-bindgen sidecar beside it.
///
/// `bindgenGlueUrl` is the whole difference from the sibling demo's web init,
/// and it is not optional here: this bridge's crate graph contains iroh, which
/// depends on wasm-bindgen structurally, so the module carries an import
/// namespace only that generated file can satisfy. Omit it and instantiation
/// fails with a message naming the namespace.
///
/// Both assets are staged by //:app_web's `extra_web_assets`.
Future<void> initBridge() async {
  await FrustrateWeb.initFromUrl(
    'iroh_rust.wasm',
    bindgenGlueUrl: 'iroh_rust_bg.js',
  );
  // Loud stale-bindings guard: the wasm module and these bindings must agree
  // on the wire schema before any call reaches an unsafe deref.
  checkFrustrateSchema();
}
