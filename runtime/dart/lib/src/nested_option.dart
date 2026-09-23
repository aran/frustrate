/// Faithful representation for a nested `Option`.
///
/// A single `Option<T>` maps to Dart `T?` — lossless and convenient. But Dart
/// nullability does not nest: `int??` is invalid, and `Some(None)` vs `None`
/// both collapse to `null`. So whenever an `Option` sits *inside* another
/// `Option`, the generated bindings use this wrapper instead of `?`, which
/// distinguishes the two absences exactly.
///
/// Named `Fr…` to avoid clashing with any `Option`/`Some`/`None` a project
/// (or another package) may already define.
library;

/// A bridged nested option: either [FrSome] carrying a value, or [FrNone].
sealed class FrOption<T> {
  const FrOption();
}

/// The present case of an [FrOption], carrying its [value].
final class FrSome<T> extends FrOption<T> {
  final T value;
  const FrSome(this.value);
}

/// The absent case of an [FrOption].
final class FrNone<T> extends FrOption<T> {
  const FrNone();
}
