/// Sentinel support for generated `copyWith` on data classes.
///
/// A generated `copyWith` must distinguish "argument omitted" from "argument
/// explicitly `null`" — otherwise it can never null out a nullable field. A
/// plain `T? field` default of `null` collapses the two cases. The fix is a
/// sentinel default: every `copyWith` parameter is typed `Object?` and defaults
/// to [frUnset]; the body keeps the old value when the argument is identical to
/// the sentinel and otherwise takes the passed value (`null` included). So
/// `copyWith(field: null)` nulls the field while `copyWith()` preserves it.
library;

/// The private sentinel type. A single const instance ([frUnset]) is the only
/// value, so `identical(x, frUnset)` is true exactly when `x` is the sentinel.
class FrUnset {
  const FrUnset();
}

/// The shared `copyWith` "argument omitted" sentinel. Generated `copyWith`
/// parameters default to this; the body compares with `identical`.
const Object frUnset = FrUnset();
