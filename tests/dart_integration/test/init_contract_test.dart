/// The repeated-init contract, against a real bridge library.
///
/// `FrustrateNative.init` is idempotent per isolate, and idempotent in the
/// strong sense — a second call constructs nothing: a second `NativeRuntime`
/// re-registers the isolate-exit listener on the same port, repointing its
/// response at an orphan id while every live callback invocation still carries
/// the first, so the sweep would miss all of them.
///
/// The other half is a repeated init that names a **different** bridge.
/// Nothing is corrupted — the
/// first library stays installed and keeps working — but every subsequent call
/// goes to a library the caller no longer believes it is using, and neither the
/// schema-hash guard nor anything else notices, because the schema being
/// checked is the *installed* library's. Silently serving the wrong library is
/// the failure this refuses.
///
/// Native-only because it names `FrustrateNative`; the web half of the same
/// contract is `init_contract_web_test.dart`.
@TestOn('vm')
library;

import 'dart:ffi';

import 'package:frustrate/frustrate.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

import 'init_native.dart';

void main() {
  setUpAll(initBridge);

  test('a second init naming the same library is a declared no-op', () {
    final installed = Frustrate.instance;
    expect(() => FrustrateNative.init(bridgeLibraryPath()), returnsNormally);
    // Same transport object: the second call did not install a replacement,
    // and (see the strong-idempotence pin in runtime/dart/test) did not build
    // one to discard either.
    expect(identical(Frustrate.instance, installed), isTrue);
  });

  test('a second init naming a different library is refused', () {
    expect(
      () => FrustrateNative.init('/nonexistent/libnot_the_bridge.dylib'),
      throwsA(
        isA<StateError>().having(
          (e) => e.message,
          'message',
          allOf(
            contains('already initialized'),
            contains('libnot_the_bridge.dylib'),
          ),
        ),
      ),
    );
    // The refusal is a report, not a teardown: the first library is still
    // installed and still serving.
    expect(Frustrate.isInstalled, isTrue);
    expect(checkFrustrateSchema, returnsNormally);
  });

  test('a second init through the other entry point is refused too', () {
    // `initWithLibrary(DynamicLibrary.process())` is a different bridge by any
    // reading — the global symbol namespace is not the file `init` opened. The
    // two entry points share one slot, so the guard has to span them.
    expect(
      () => FrustrateNative.initWithLibrary(DynamicLibrary.process()),
      throwsA(
        isA<StateError>().having(
          (e) => e.message,
          'message',
          contains('already initialized'),
        ),
      ),
    );
    expect(Frustrate.isInstalled, isTrue);
  });
}
