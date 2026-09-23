/// The capability-gate seam. Some bridge members are native-only by
/// derivation (here: value-returning `DartFunction` callbacks) and are
/// compile-time absent from the generated web surface. The shared,
/// byte-identical `main.dart` must never name such a member directly, so it
/// reaches them only through this conditional export — the same technique the
/// init seam uses ([initBridge]).
library;

export 'src/native_features_io.dart'
    if (dart.library.js_interop) 'src/native_features_web.dart';
