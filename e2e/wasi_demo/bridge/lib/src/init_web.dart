import 'package:frustrate/frustrate.dart';

import 'package:wasi_bridge/wasi_rust.frustrate.dart';

/// Fetches the wasm module bundled as a web asset at the site root.
///
/// Identical to the plain-wasm32 case, and that is the point worth noticing:
/// the `wasi_snapshot_preview1` host is supplied by frustrate's own web runtime
/// (`runtime/dart/lib/src/js/frustrate.js`), so a wasip1 module loads through
/// the same one-line call with no extra package, no shim dependency and no
/// build step. The platform choice is made once, in BUILD.bazel, and nothing
/// downstream of it changes.
Future<void> initBridge() async {
  await FrustrateWeb.initFromUrl('wasi_rust.wasm');
  // Loud stale-bindings guard: the wasm module and these bindings must agree
  // on the wire schema before any call reaches an unsafe deref.
  checkFrustrateSchema();
}
