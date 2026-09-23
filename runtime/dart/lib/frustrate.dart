/// frustrate runtime for Dart.
///
/// Generated bindings (see frustrate-codegen) import this package and use
/// only its platform-neutral surface (Frustrate/FrustrateRuntime, the codec,
/// OpaqueHandle). Bridge users call the platform init once — on native,
/// `FrustrateNative.init(libraryPath)` — then use the generated API.
///
/// Platform selection: `dart.library.js_interop` is true exactly for web
/// compilers (dart2js and dart2wasm) and false on the VM. Do not condition on
/// `dart.library.ffi` — dart2wasm partially exposes dart:ffi.
library;

export 'src/binary_codec.dart';
export 'src/consumed.dart';
// The two ABI constants, and nothing else from `envelope.dart` — the envelope
// codec itself is the transport's business. A test that pins where the request
// lands inside the transport's block needs to be able to *name* the slab rather
// than repeat `128`, which is the whole difference between pinning a contract
// and pinning a magic number.
export 'src/envelope.dart' show frustrateRespSlabBytes, frustrateRuntimeAbi;
export 'src/copy_with.dart';
export 'src/exceptions.dart';
export 'src/nested_option.dart';
// The cancel token, and nothing else from `pending_calls.dart` — the pending-
// call bookkeeping is the transport's business. The token is public because a
// generated `async fn` binding takes one; it lives in that library because
// cancellation is a pending call's lifecycle (see the library doc there).
export 'src/pending_calls.dart' show FrustrateCancelToken;
export 'src/runtime_core.dart';
export 'src/value_equality.dart';
export 'src/actor_handle_native.dart'
    if (dart.library.js_interop) 'src/actor_handle_web.dart';
export 'src/opaque_handle_native.dart'
    if (dart.library.js_interop) 'src/opaque_handle_web.dart';
export 'src/runtime_native.dart'
    if (dart.library.js_interop) 'src/runtime_web.dart';
