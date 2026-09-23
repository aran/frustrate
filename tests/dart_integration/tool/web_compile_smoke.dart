/// Compile guard: the generated bindings and package:frustrate must compile
/// for web targets (dart2js and dart2wasm) even before the web transport
/// lands. Touching an opaque type, a sync function, and an async function
/// pulls the whole generated file through the web compiler.
///
/// Run: `dart compile js tool/web_compile_smoke.dart -o /dev/null` and
/// `dart compile wasm tool/web_compile_smoke.dart -o /tmp/smoke.wasm`.
/// Until the chrome test suite lands, this is the web check.
library;

import 'package:frustrate_integration/test_api.frustrate.dart';

Future<void> main() async {
  print(addI32(a: 1, b: 2));
  // The u64/BigInt path deliberately avoids 64-bit ByteData accessors; keep
  // it flowing through the dart2js compile.
  print(u64Extremes());
  final doc = await TextDoc.load(initial: 'hello');
  print(doc.text());
  doc.dispose();
}
