/// The repeated-init contract on web — the same contract
/// `init_contract_test.dart` pins on native, which is the point of the file.
///
/// Two hazards, and they are different from each other:
///
///  1. **Weak idempotence.** `Frustrate.install` is `??=`, so the *outcome*
///     would be right — the first transport wins — but the argument is
///     evaluated first, so a second init that got that far would compile the
///     module, create the shared memory, instantiate a whole second wasm
///     instance and run its `frustrate_web_init`, then throw it away.
///  2. **A second init naming a different module going silent**, with the
///     consequence that the first module keeps serving every call while the
///     caller believes it swapped bridges.
///
/// The last test is the one that says the claim happens *before* the fetch: a
/// URL that does not exist has to be refused by the contract, not by a 404.
@TestOn('browser')
library;

import 'dart:typed_data';

import 'package:frustrate/frustrate.dart';
import 'package:frustrate/frustrate_web.dart';
import 'package:test/test.dart';

import 'init_web.dart';

void main() {
  setUpAll(initBridge);

  test('a second init naming the same module is a declared no-op', () async {
    final installed = Frustrate.instance;
    await FrustrateWeb.initFromUrl(bridgeModuleUrl);
    expect(identical(Frustrate.instance, installed), isTrue);
  });

  test(
    'a second init naming a different module is refused, before the fetch',
    () async {
      // Nothing is served here. Today's failure would be `HTTP 404`, which is a
      // fetch that already happened; the contract's failure names the module
      // already installed and never reaches the network.
      await expectLater(
        FrustrateWeb.initFromUrl('../build/not_the_bridge.wasm'),
        throwsA(
          isA<StateError>().having(
            (e) => e.message,
            'message',
            allOf(
              contains('already initialized'),
              contains('not_the_bridge.wasm'),
            ),
          ),
        ),
      );
      expect(Frustrate.isInstalled, isTrue);
    },
  );

  test('a second init through the other entry point is refused too', () async {
    // `init(bytes)` and `initFromUrl(url)` name a module in two different ways
    // and the guard compares what it was told, so this is reported as a
    // disagreement — which is also what stops the empty buffer from reaching
    // `WebAssembly.compile`.
    await expectLater(
      FrustrateWeb.init(Uint8List(0)),
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
