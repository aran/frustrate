/// The trap-attribution seam: a Rust `panic!` on a sync call reaches Dart as a
/// [BridgePanicException] carrying the Rust message, on both platforms.
///
/// On **web** this is the only thing the JS-owned frame in `frustrate.js`
/// exists for. Under `panic=abort` the panic becomes
/// a wasm trap, and a trap is uncatchable from inside another wasm module — so
/// a JS frame around every export call, plus the `frustrate.panic` hook that
/// ships the message before the trap, is what turns a payload-less
/// `RuntimeError: unreachable` into an attributable exception. Anything that
/// changes that frame's *calling convention* (its arity, how it signals the
/// failure, whether it returns or rethrows) has to keep this green: the frame
/// is on the hot path of every bridge call, so it is a standing optimization
/// target, and this test is the thing that says what may not be traded away.
///
/// On **native** the same call is `panic=unwind`, caught by `envelope::run`,
/// and the message arrives in the response envelope instead. One test for both
/// because the developer-facing contract is the same on both.
///
/// The file holds exactly one call on purpose. Web is `panic=abort`, so the
/// trap leaves the wasm instance's Rust state arbitrary; nothing else may be
/// asserted after it in the same page.
library;

import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

void main() {
  setUpAll(initBridge);

  test(
    'a Rust panic on a sync call arrives as an attributed panic exception',
    () {
      expect(
        alwaysPanics,
        throwsA(
          isA<BridgePanicException>().having(
            (e) => e.message,
            'message',
            contains('deliberate panic for the integration test'),
          ),
        ),
      );
    },
  );
}
