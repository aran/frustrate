/// `I64List`, the Dart type of `Vec<i64>` and `[i64; N]`: `List<int>`, for
/// dart2js and DDC, which cannot construct an `Int64List`.
///
/// This is the `js` variant of package `frustrate_i64_list`; the `typed` one
/// is beside it. `@rules_dart//dart/constraints:web_compiler` picks between them in the
/// `deps` of `//runtime/dart:frustrate`, and `package:frustrate/frustrate.dart`
/// exports the result, so nothing imports this package by name.
library;

/// The Dart type of a `Vec<i64>` or `[i64; N]`.
typedef I64List = List<int>;

/// [values] as an [I64List]: the same list here, and a copy only where the
/// type is `Int64List` and [values] is not one.
I64List toI64List(List<int> values) => values;
