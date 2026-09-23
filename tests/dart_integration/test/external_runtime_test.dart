/// Native-only: the "bring your own async runtime" pattern. The fixture
/// `sumOnExternalRuntime` drives a genuinely-suspending future to completion on
/// an external runtime (`pollster`) via `block_on` inside a plain pool fn, then
/// returns a value — so Dart receives it as a resolved `Future`.
///
/// There is no web arm, and since the fixture became `#[bridge(native_only)]`
/// there cannot be one: `sumOnExternalRuntime` is absent from the web Dart
/// surface, so a web arm would not compile. That is the point of the
/// declaration — see the fixture's comment in tests/test_api/src/api.rs.
///
/// The `@TestOn('vm')` below is therefore belt-and-braces rather than the only
/// thing keeping this off web. It used to be the only thing, and the reason it
/// gave was wrong in a way worth recording: it said `block_on` "traps on the
/// single-threaded-web main thread". It does not. It traps on the *threaded*
/// web main thread (`Atomics.wait` is barred there); on single-threaded web
/// there is no futex at all, so `park` is a no-op and the call spins the only
/// thread forever with no trap and no diagnostic. The dangerous configuration
/// was the one the comment named as safe.
@TestOn('vm')
library;

import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart';

void main() {
  setUpAll(initBridge);

  test('a plain fn driving an external runtime resolves as a Future', () async {
    // Runs off the Dart thread on a pool worker; pollster::block_on parks that
    // worker while it polls the suspending body, then returns → Future resolves.
    expect(await sumOnExternalRuntime(a: 40, b: 2), 42);
  });

  test('repeated calls each complete and return their worker', () async {
    // The awaited future self-completes, so block_on returns promptly and frees
    // its pool worker each time; back-to-back calls all resolve (a smoke check
    // that the pattern reuses workers, not a strict leak proof).
    expect(await sumOnExternalRuntime(a: -7, b: 7), 0);
    expect(await sumOnExternalRuntime(a: 1000, b: 1), 1001);
  });
}
