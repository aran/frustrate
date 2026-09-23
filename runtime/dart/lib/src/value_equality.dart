/// Deep structural equality and hashing for generated data classes.
///
/// Dart's `==`/`hashCode` on `List` and `Map` are identity-based, so a struct
/// or enum variant with a collection field can't get correct value semantics
/// from field-wise `==` alone. Generated `==`/`hashCode` route every field
/// through these helpers so equal-by-value instances compare equal (and hash
/// equal) — including nested lists, maps, byte lists, and nested data classes
/// (which themselves have generated deep `==`).
library;

/// Structural equality: `identical`, then element-wise for `List`/`Map`,
/// otherwise `==` (which for nested generated data classes is itself deep).
///
/// That last fallback is also how a **handle** field compares. A generated
/// handle class does not override `==`, so it is Dart's identity — two values
/// holding different Rust objects are unequal, two holding the same one are
/// equal, and no handle is ever compared by its contents. [frDeepHash] agrees,
/// hashing such a field by its identity `hashCode`.
bool frDeepEquals(Object? a, Object? b) {
  if (identical(a, b)) return true;
  if (a is List && b is List) {
    if (a.length != b.length) return false;
    for (var i = 0; i < a.length; i++) {
      if (!frDeepEquals(a[i], b[i])) return false;
    }
    return true;
  }
  if (a is Map && b is Map) {
    if (a.length != b.length) return false;
    for (final k in a.keys) {
      if (!b.containsKey(k) || !frDeepEquals(a[k], b[k])) return false;
    }
    return true;
  }
  return a == b;
}

/// Structural hash consistent with [frDeepEquals]: order-sensitive for lists,
/// order-independent for maps.
int frDeepHash(Object? x) {
  if (x is List) return Object.hashAll(x.map(frDeepHash));
  if (x is Map) {
    var h = 0;
    for (final e in x.entries) {
      h ^= Object.hash(frDeepHash(e.key), frDeepHash(e.value));
    }
    return h;
  }
  return x.hashCode;
}
