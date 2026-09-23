/// Bridge initialization. App code calls [initBridge] once before using the
/// bindings.
///
/// A conditional export, restored by C10 when web landed: the native and web
/// transports have nothing in common but their name. `dart:io` does not exist
/// on web and `dart:js_interop` does not exist natively, so the two cannot
/// share one file — this is the same three-file seam e2e/flutter_demo uses.
library;

export 'src/init_native.dart' if (dart.library.js_interop) 'src/init_web.dart';
