/// Native test bootstrap: initialize the bridge, which the build supplies as
/// the `package:frustrate_integration/test_api` code asset.
library;

import 'package:frustrate/frustrate.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';

/// On native a Rust panic *unwinds*, so a producer panic drops the sink and
/// drop-retire ends the bound stream cleanly (`onDone`). On web
/// (`panic=abort`) no destructor runs, so the stream is left open by design —
/// the panic is still reported on the call. Tests that assert the stream's
/// terminal after a producer panic branch on this.
const bool panicClosesStreams = true;

/// True on the native VM. The open-channel leak guard (which asserts a test
/// disposed its stored sinks/callbacks) is native-only: only there does an open
/// registration keep the isolate alive and hang process exit. On web the page
/// is always alive and retirement timing differs, so the guard would misfire.
const bool isNativeVm = true;

Future<void> initBridge() async {
  FrustrateNative.init(testApiLibrary);
  // Fail loudly here if the loaded dylib and these bindings disagree on the
  // wire schema, rather than dispatching wrong fn_ids into unsafe derefs.
  checkFrustrateSchema();
}
