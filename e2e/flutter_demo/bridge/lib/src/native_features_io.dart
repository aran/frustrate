import 'package:demo_bridge/demo_rust.frustrate.dart';
import 'package:frustrate/frustrate.dart' show toI64List;

/// True on every native target: the value-returning callback surface exists.
const bool kHasNativeReturningCallbacks = true;

/// Rust calls back into Dart to map each value — blocking its pool worker per
/// item — then sums the results. Native-only: `transformSum` is compile-time
/// absent from the wasm surface, so this reference only ever compiles here.
Future<int> nativeTransformSum(List<int> values, int Function(int) f) =>
    transformSum(values: toI64List(values), f: f);
