/// The web mirror of native_features_io.dart. Same public surface, but it
/// names no native-only member — because on web those symbols do not exist.
/// [kHasNativeReturningCallbacks] is false, so the UI shows the capability-gate
/// card instead of ever calling [nativeTransformSum].

/// False on web: the value-returning callback surface is compile-time absent.
const bool kHasNativeReturningCallbacks = false;

/// Never reached on web (guarded by [kHasNativeReturningCallbacks]); present
/// only so the shared UI can name one symbol on both platforms.
Future<int> nativeTransformSum(List<int> values, int Function(int) f) =>
    throw UnsupportedError(
        'value-returning callbacks are native-only — compile-time absent on web');
