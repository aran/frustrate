/// `I64List`, the Dart type of `Vec<i64>` and `[i64; N]`: `Int64List`, for the
/// VM and dart2wasm, where `int` is 64 bits.
///
/// This is the `typed` variant of package `frustrate_i64_list`; the `js` one
/// is beside it. `@rules_dart//dart/constraints:web_compiler` picks between them in the
/// `deps` of `//runtime/dart:frustrate`, and `package:frustrate/frustrate.dart`
/// exports the result, so nothing imports this package by name.
library;

import 'dart:typed_data';

/// The Dart type of a `Vec<i64>` or `[i64; N]`.
typedef I64List = Int64List;

/// [values] as an [I64List]: the same list here, and a copy only where the
/// type is `Int64List` and [values] is not one.
I64List toI64List(List<int> values) =>
    values is Int64List ? values : Int64List.fromList(values);
