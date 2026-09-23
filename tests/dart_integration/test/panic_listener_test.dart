/// The Rust-side panic listener fires for a panic raised by a real bridge
/// call, on both transports.
///
/// The listener is Rust's, and so is everything it is told, so this asserts
/// across the language boundary rather than about it: `watchPanics` registers a
/// counting listener inside the bridge crate, `alwaysPanics` raises a panic
/// through the ordinary dispatch path, and `panicsSeen` reports what the
/// listener heard. Nothing here reaches into the runtime — a Dart caller could
/// write all three.
///
/// **Why the two platforms assert different amounts.** Native is `panic=unwind`:
/// `envelope::run` catches, `envelope::panic_envelope` tells the listener, and
/// the call returns normally, so everything afterwards is ordinary. Web is
/// `panic=abort`: the listener is called from the panic hook and the call then
/// traps, and `trap_attribution_test.dart` states the standing rule that a trap
/// leaves the instance's Rust state arbitrary.
///
/// The count is the one read that escapes that rule rather than bending it. It
/// is a relaxed store to an `AtomicU64` completed inside the hook before the
/// trap, holding no lock and unable to be half-written (`api.rs`, `PANICS_SEEN`).
/// The message and location travel through a `Mutex<String>`, which is a lock
/// this test has no business reasoning about after a trap — so those are
/// asserted on the VM only. What web loses by that is nothing about coverage:
/// a count that moves is proof the hook arm ran, and the content of the report
/// that arm builds is held by `frustrate`'s own
/// `panic::tests::the_hook_arm_reports_the_same_report`, which compiles and
/// drives that arm on every host.
///
/// Its own file, and with the panic last, for the reason `trap_attribution_test`
/// gives: on web the trap ends this page's usefulness, so nothing may follow it
/// but the single narrow read above.
library;

import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart' if (dart.library.js_interop) 'init_web.dart';

void main() {
  setUpAll(initBridge);

  test('a panicking call reaches the registered Rust panic listener', () {
    watchPanics();
    final before = panicsSeen();

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

    expect(
      panicsSeen(),
      before + 1,
      reason:
          'the panic reached Dart as an exception but the Rust listener '
          'was never told: a crash reporter registered on this bridge would '
          'report a clean process while panics were happening',
    );

    // The message and the location ride a `Mutex<String>`; see the header.
    if (const bool.fromEnvironment('dart.library.js_interop')) return;

    expect(
      lastPanicReport(),
      contains('deliberate panic for the integration test'),
      reason:
          'the listener was called with something other than the panic '
          'the caller saw',
    );
    expect(
      lastPanicReport(),
      contains('api.rs:'),
      reason:
          'the report carries no source location, so a reporter has '
          'nothing to group crashes by — the recording panic hook did not '
          'run, or something displaced it',
    );
  });
}
