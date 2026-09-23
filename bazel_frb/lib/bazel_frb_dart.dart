/// The package's public surface: FRB's generated entrypoint plus the generated
/// API. Hand-written (three lines), so the generated files can stay exactly as
/// `flutter_rust_bridge_codegen generate` wrote them.
library;

export 'src/api/simple.dart';
export 'src/frb_generated.dart' show RustLib;
