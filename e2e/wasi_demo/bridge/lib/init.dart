/// Platform-neutral bridge initialization. App code calls [initBridge] once
/// before using the bindings; which transport that resolves to is decided
/// here, not in app code.
library;

export 'src/init_native.dart'
    if (dart.library.js_interop) 'src/init_web.dart';
