import 'dart:typed_data';

import 'package:demo_bridge/demo_rust.frustrate.dart';

/// True on every native target: the value-returning callback surface exists.
const bool kHasNativeReturningCallbacks = true;

/// Rust calls back into Dart to map each value — blocking its pool worker per
/// item — then sums the results. Native-only: `transformSum` is compile-time
/// absent from the wasm surface, so this reference only ever compiles here.
/// `Vec<i64>` maps to `Int64List` (a typed list), so convert at the boundary.
Future<int> nativeTransformSum(List<int> values, int Function(int) f) =>
    transformSum(values: Int64List.fromList(values), f: f);
