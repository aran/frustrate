/// The repeated-init contract's decision procedure, and the native entry
/// point's wiring to it.
///
/// The contract itself is stated on `Frustrate` (runtime_core.dart): naming the
/// same bridge again is a declared no-op, naming a different one is a
/// `StateError`. Its end-to-end pins run against a real bridge — see
/// `tests/dart_integration/test/init_contract_test.dart` (native) and
/// `init_contract_web_test.dart` (web). This file covers the two things those
/// cannot reach:
///
///   * **Strong idempotence.** "A second init constructs nothing" is invisible
///     against a real library, because resolving it a second time succeeds.
///     Here the installed source is a library that cannot be resolved, so a
///     second init returning normally *is* the proof that it never looked
///     anything up — which would have thrown.
///   * **A failed init leaves the slot clean.** The source is recorded by
///     `install`, not by the guard, so an init that threw on the way up is not
///     remembered and the next attempt is a first init rather than a
///     disagreement with a bridge that never existed. Each of these needs
///     virgin statics, so they run in a fresh isolate — which is also the shape
///     a hot restart presents (a restarted app runs its *first* init, because
///     these are static fields and the VM restarts the isolate under them).
@TestOn('vm')
library;

import 'dart:ffi';
import 'dart:isolate';

import 'package:frustrate/frustrate.dart';
import 'package:test/test.dart';

/// A stand-in transport. No *bridge call* here is ever made: these tests are
/// about which init calls reach a constructor at all, never about what the
/// transport then does. It answers `bridgeIdentity` because `install` records
/// it — a transport is its own bridge, which is what both real transports say
/// too (runtime_core.dart, [FrustrateRuntime.bridgeIdentity]).
const _testBridge = FrustrateBridge('test');

final class _FakeRuntime implements FrustrateRuntime {
  @override
  Object get bridgeIdentity => this;

  @override
  FrustrateBridge get servedBridge => _testBridge;

  @override
  dynamic noSuchMethod(Invocation invocation) => throw StateError(
    'the fake transport was called: ${invocation.memberName}',
  );
}

/// What an absent library throws from every lookup. Its own type, so a test can
/// require that an init got as far as asking the library for an export, rather
/// than accepting any failure as the expected one.
final class _LibraryAbsent implements Exception {
  @override
  String toString() => 'the library is absent';
}

/// A bridge whose library cannot be resolved: every lookup fails.
final class _AbsentBridge implements BridgeLibrary {
  @override
  final String name;

  const _AbsentBridge(this.name);

  @override
  FrustrateBridge get bridge => _testBridge;

  @override
  Pointer<Void> address(NativeExport export) => throw _LibraryAbsent();

  @override
  HandleExports handle(String dropSymbol) => throw _LibraryAbsent();
}

const _bridgeA = _AbsentBridge('bridge A');
const _bridgeB = _AbsentBridge('bridge B');

void main() {
  // Every test in here needs a bridge installed and none of them changes what
  // is installed, so the state is built once and no test depends on running
  // before or after any other. The empty-slot cases live in the fresh-isolate
  // group below, which is the only honest way to have both in one file.
  group('with a bridge installed', () {
    setUpAll(() {
      Frustrate.install(
        _FakeRuntime(),
        source: _bridgeA,
        description: 'the bridge library A',
      );
    });

    test('the same source: the init is a no-op', () {
      expect(
        Frustrate.initializedFrom(_bridgeA, 'the bridge library A'),
        isTrue,
      );
    });

    test('a different source: a StateError naming both', () {
      expect(
        () => Frustrate.initializedFrom(_bridgeB, 'the library B'),
        throwsA(
          isA<StateError>().having(
            (e) => e.message,
            'message',
            allOf(
              contains('already initialized'),
              contains('the bridge library A'),
              contains('the library B'),
            ),
          ),
        ),
      );
    });

    test('a second install never replaces the first', () {
      final first = Frustrate.instance;
      Frustrate.install(
        _FakeRuntime(),
        source: 'something else',
        description: 'something else',
      );
      expect(identical(Frustrate.instance, first), isTrue);
      // ...and it did not repoint the recorded source either, or the guard
      // would start answering for a transport that was discarded.
      expect(
        Frustrate.initializedFrom(_bridgeA, 'the bridge library A'),
        isTrue,
      );
    });

    test(
      'FrustrateNative.init with the installed bridge constructs nothing',
      () {
        // The installed bridge cannot be looked up in. Returning normally
        // therefore proves the guard ran *before* the construction — a second
        // `NativeRuntime` would have thrown here, and in a real app it would
        // instead have silently re-registered the isolate-exit listener.
        expect(() => FrustrateNative.init(_bridgeA), returnsNormally);
      },
    );

    test('FrustrateNative.init with another bridge is refused', () {
      expect(
        () => FrustrateNative.init(_bridgeB),
        throwsA(
          isA<StateError>().having(
            (e) => e.message,
            'message',
            allOf(contains('the bridge library A'), contains('bridge B')),
          ),
        ),
      );
    });
  });

  group('BridgeLibrary.dynamic', () {
    test('is equal when the libraries and bridges are, whatever it is '
        'called', () {
      final one = BridgeLibrary.dynamic(
        DynamicLibrary.process(),
        name: 'one',
        bridge: _testBridge,
      );
      final two = BridgeLibrary.dynamic(
        DynamicLibrary.process(),
        name: 'two',
        bridge: _testBridge,
      );
      expect(one, equals(two));
      expect(one.hashCode, two.hashCode);
      final other = BridgeLibrary.dynamic(
        DynamicLibrary.process(),
        name: 'one',
        bridge: const FrustrateBridge('other'),
      );
      expect(one, isNot(equals(other)));
    });

    test('names a runtime export as the runtime does and a bridge entry '
        'under its crate', () {
      expect(
        NativeExport.runtimeAbi.symbolFor(_testBridge),
        RuntimeSymbol.runtimeAbi,
      );
      expect(
        NativeExport.callSync.symbolFor(_testBridge),
        'frustrate_test_call_sync',
      );
    });

    test('refuses an export the library lacks', () {
      final bridge = BridgeLibrary.dynamic(
        DynamicLibrary.process(),
        name: 'process',
        bridge: _testBridge,
      );
      expect(
        () => bridge.address(NativeExport.runtimeAbi),
        throwsArgumentError,
      );
      expect(
        () => bridge.handle('frustrate_test_drop_Doc'),
        throwsArgumentError,
      );
      expect(
        () => bridge.handle('frustrate_other_drop_Doc'),
        throwsArgumentError,
      );
    });
  });

  group('in a fresh isolate', () {
    test(
      'a failed init records nothing, so it can simply be retried',
      () async {
        expect(await Isolate.run(_failedInitLeavesTheSlotClean), isEmpty);
      },
    );

    test('a first init is unaffected by any other isolate', () async {
      expect(await Isolate.run(_freshIsolateSeesNoTransport), isEmpty);
    });
  });
}

/// Runs in its own isolate: statics start out virgin, exactly as they do in the
/// isolate a hot restart brings up. Returns the complaints it found, so the
/// parent reports them (a `TestFailure` is not what we want to send back).
List<String> _failedInitLeavesTheSlotClean() {
  final complaints = <String>[];
  complaints.addAll(_lookupFailure(_bridgeA));
  if (Frustrate.isInstalled) {
    complaints.add('a failed init installed a transport');
  }
  // Nothing was recorded, so a *different* bridge is a first init and fails the
  // same way — it is not reported as a disagreement with a bridge that was
  // never installed.
  complaints.addAll(_lookupFailure(_bridgeB));
  return complaints;
}

/// Init [bridge] and require the failure to be **the library's**, by type.
///
/// Matched exactly rather than caught broadly, because "it threw something" is
/// not the property under test: the point is that the call got all the way to
/// asking the library for an export and failed there. A contract `StateError`,
/// or any future refusal raised before the library is reached, has to show up
/// as a complaint rather than pass for the expected failure.
List<String> _lookupFailure(BridgeLibrary bridge) {
  try {
    FrustrateNative.init(bridge);
    return ['initializing an absent library should have thrown'];
  } on _LibraryAbsent {
    return const [];
  } catch (e) {
    return ['expected ${bridge.name} to refuse; got ${e.runtimeType}: $e'];
  }
}

List<String> _freshIsolateSeesNoTransport() {
  final complaints = <String>[];
  if (Frustrate.isInstalled) {
    complaints.add('a fresh isolate inherited a transport');
  }
  if (Frustrate.initializedFrom('any bridge', 'any bridge')) {
    complaints.add('a fresh isolate answered a repeated init');
  }
  return complaints;
}
