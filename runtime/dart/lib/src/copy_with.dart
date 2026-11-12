/// Sentinel support for generated `copyWith` on data classes.
///
/// A generated `copyWith` must distinguish "argument omitted" from "argument
/// explicitly `null`" for a field that can hold `null` — otherwise it can never
/// null that field out. A plain `T? field` default of `null` collapses the two
/// cases. Such a field's parameter is typed `Object?` and defaults to
/// [copyWithUnset]; the body keeps the old value when the argument is identical
/// to the sentinel and otherwise takes the passed value (`null` included). So
/// `copyWith(field: null)` nulls the field while `copyWith()` preserves it.
library;

/// The sentinel type. A single const instance ([copyWithUnset]) is the only
/// value, so `identical(x, copyWithUnset)` is true exactly when `x` is the
/// sentinel.
class _Unset {
  const _Unset();
}

/// The shared `copyWith` "argument omitted" sentinel. Generated `copyWith`
/// parameters default to this; the body compares with `identical`.
const Object copyWithUnset = _Unset();
