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

/// A bridge whose library is not there: every lookup fails. Stands in for a
/// second, different library without needing one to exist.
final class _AbsentBridge implements BridgeLibrary {
  @override
  String get name => 'package:not_the_bridge/not_the_bridge';

  @override
  Pointer<Void> lookup(String symbol) =>
      throw StateError('the absent bridge was asked for $symbol');
}

void main() {
  setUpAll(initBridge);

  test('a second init naming the same library is a declared no-op', () {
    final installed = Frustrate.instance;
    expect(() => FrustrateNative.init(testApiBridge), returnsNormally);
    // Same transport object: the second call did not install a replacement,
    // and (see the strong-idempotence pin in runtime/dart/test) did not build
    // one to discard either.
    expect(identical(Frustrate.instance, installed), isTrue);
  });

  test('a second init naming a different library is refused', () {
    expect(
      () => FrustrateNative.init(_AbsentBridge()),
      throwsA(
        isA<StateError>().having(
          (e) => e.message,
          'message',
          allOf(contains('already initialized'), contains('not_the_bridge')),
        ),
      ),
    );
    // The refusal is a report, not a teardown: the first library is still
    // installed and still serving.
    expect(Frustrate.isInstalled, isTrue);
    expect(checkFrustrateSchema, returnsNormally);
  });

  test(
    'a hand-loaded library is a different bridge from the generated one',
    () {
      // The generated bridge resolves through the VM's asset lookup; a wrapper
      // over the process's own symbols is a different bridge by any reading.
      expect(
        () => FrustrateNative.init(
          BridgeLibrary.dynamic(DynamicLibrary.process(), name: 'process'),
        ),
        throwsA(
          isA<StateError>().having(
            (e) => e.message,
            'message',
            contains('already initialized'),
          ),
        ),
      );
      expect(Frustrate.isInstalled, isTrue);
    },
  );
}
