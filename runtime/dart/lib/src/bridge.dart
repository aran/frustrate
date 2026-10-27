/// The name a bridge goes by on both sides of the wire.
library;

/// One bridge: the bindings codegen produced for one Rust crate.
///
/// Generated code declares it as `frustrateBridge` and names it on every call
/// (`Frustrate.of`). Several bridges can be linked into one library and served
/// by one Rust runtime; each keeps its own exports, because every export a
/// bridge's glue defines carries its crate name ([symbol]).
///
/// Equal by crate name, so a bridge is the same bridge however it was
/// constructed.
final class FrustrateBridge {
  /// The Rust crate name, as codegen was given it.
  final String crate;

  const FrustrateBridge(this.crate);

  /// The name this bridge's glue exports [entry] under:
  /// `frustrate_<crate>_<entry>`. Codegen spells the same names
  /// (`Interface::export_symbol`).
  String symbol(String entry) => 'frustrate_${crate}_$entry';

  @override
  bool operator ==(Object other) =>
      other is FrustrateBridge && other.crate == crate;

  @override
  int get hashCode => crate.hashCode;

  @override
  String toString() => 'the `$crate` bridge';
}
