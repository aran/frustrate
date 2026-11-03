/// Compiles only if `Vec<i64>` is `Int64List` in the bindings: a
/// `List<int>`-declared return is wider, so the tear-off is refused. Built by
/// `dart_wasm_binary`; never run.
library;

import 'dart:typed_data';

import 'package:frustrate_integration/test_api.frustrate.dart';

void main() {
  final Int64List Function({required Int64List xs}) f = echoI64s;
  print(f.runtimeType);
}
