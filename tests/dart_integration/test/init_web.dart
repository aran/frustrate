/// Web test bootstrap: fetch the wasm module staged by
/// `dart run tool/build_web_fixture.dart` and instantiate it.
///
/// [panicClosesStreams] is `false` here: web compiles with `panic=abort`, so a
/// producer panic runs no destructor and leaves the bound stream open by design
/// (the panic is still reported on the call). See the native counterpart.
///
/// The test page is served at <secret>/test/<suite>.html, and the package
/// test server serves package files under the same prefix, so the staged
/// module is one directory up.
library;

import 'package:frustrate/frustrate_web.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';

/// See the doc on the native counterpart: web is `panic=abort`, so a producer
/// panic leaves the bound stream open (the panic is reported on the call).
const bool panicClosesStreams = false;

/// See the native counterpart: the open-channel leak guard is native-only, so
/// this is `false` on web.
const bool isNativeVm = false;

/// Where the staged module is served. Public so `init_contract_web_test.dart`
/// can name the same module a second time (the repeated-init contract).
const String bridgeModuleUrl = '../build/test_api.wasm';

Future<void> initBridge() async {
  await FrustrateWeb.initFromUrl(bridgeModuleUrl);
  // Fail loudly here if the wasm module and these bindings disagree on the
  // wire schema, rather than dispatching wrong fn_ids into unsafe derefs.
  checkFrustrateSchema();
}
