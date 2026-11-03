/// Compiles only if `Vec<i64>` is `List<int>` in the bindings: an
/// `Int64List`-declared parameter is narrower, so the tear-off is refused.
/// Built by `dart_js_binary`; never run.
library;

import 'package:frustrate_integration/test_api.frustrate.dart';

void main() {
  final List<int> Function({required List<int> xs}) f = echoI64s;
  print(f.runtimeType);
}
