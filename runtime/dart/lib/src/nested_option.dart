/// Faithful representation for a nested `Option`.
///
/// A single `Option<T>` maps to Dart `T?` — lossless and convenient. But Dart
/// nullability does not nest: `int??` is invalid, and `Some(None)` vs `None`
/// both collapse to `null`. So whenever an `Option` sits *inside* another
/// `Option`, the generated bindings use this wrapper instead of `?`, which
/// distinguishes the two absences exactly.
///
/// Named `Nested…` for the case it exists for, which also keeps it clear of
/// any `Option`/`Some`/`None` a project (or another package) may define.
library;

/// A bridged nested option: either [NestedSome] carrying a value, or [NestedNone].
sealed class NestedOption<T> {
  const NestedOption();
}

/// The present case of an [NestedOption], carrying its [value].
final class NestedSome<T> extends NestedOption<T> {
  final T value;
  const NestedSome(this.value);
}

/// The absent case of an [NestedOption].
final class NestedNone<T> extends NestedOption<T> {
  const NestedNone();
}
