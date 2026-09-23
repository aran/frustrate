/// The repeated-init contract's decision procedure, and the native entry
/// points' wiring to it.
///
/// The contract itself is stated on `Frustrate` (runtime_core.dart): naming the
/// same bridge again is a declared no-op, naming a different one is a
/// `StateError`. Its end-to-end pins run against a real bridge — see
/// `tests/dart_integration/test/init_contract_test.dart` (native) and
/// `init_contract_web_test.dart` (web). This file covers the two things those
/// cannot reach:
///
///   * **Strong idempotence.** "A second init constructs nothing" is invisible
///     against a real library, because opening it a second time succeeds. Here
///     the installed source names a library that does not exist, so a second
///     init returning normally *is* the proof that it never reached
///     `DynamicLibrary.open` — which would have thrown.
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
final class _FakeRuntime implements FrustrateRuntime {
  @override
  Object get bridgeIdentity => this;

  @override
  dynamic noSuchMethod(Invocation invocation) => throw StateError(
    'the fake transport was called: ${invocation.memberName}',
  );
}

/// A library path that cannot be opened, so *reaching* `DynamicLibrary.open`
/// is observable as a throw.
const String _absentLibrary = '/nonexistent/libfrustrate_test_bridge.dylib';

void main() {
  // Every test in here needs a bridge installed and none of them changes what
  // is installed, so the state is built once and no test depends on running
  // before or after any other. The empty-slot cases live in the fresh-isolate
  // group below, which is the only honest way to have both in one file.
  group('with a bridge installed', () {
    setUpAll(() {
      Frustrate.install(
        _FakeRuntime(),
        source: _absentLibrary,
        description: 'the bridge library A',
      );
    });

    test('the same source: the init is a no-op', () {
      expect(
        Frustrate.initializedFrom(_absentLibrary, 'the bridge library A'),
        isTrue,
      );
    });

    test('a different source: a StateError naming both', () {
      expect(
        () => Frustrate.initializedFrom('/other/libB.dylib', 'the library B'),
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
        Frustrate.initializedFrom(_absentLibrary, 'the bridge library A'),
        isTrue,
      );
    });

    test('FrustrateNative.init with the installed path constructs nothing', () {
      // The installed source is a path that cannot be opened. Returning
      // normally therefore proves the guard ran *before* the construction — a
      // second `NativeRuntime` would have thrown here, and in a real app it
      // would instead have silently re-registered the isolate-exit listener.
      expect(() => FrustrateNative.init(_absentLibrary), returnsNormally);
    });

    test('FrustrateNative.init with another path is refused', () {
      expect(
        () => FrustrateNative.init('/nonexistent/libsomething_else.dylib'),
        throwsA(
          isA<StateError>().having(
            (e) => e.message,
            'message',
            contains('libsomething_else.dylib'),
          ),
        ),
      );
    });

    test('the two native entry points share one slot', () {
      expect(
        () => FrustrateNative.initWithLibrary(DynamicLibrary.process()),
        throwsA(
          isA<StateError>().having(
            (e) => e.message,
            'message',
            contains('initWithLibrary'),
          ),
        ),
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
  complaints.addAll(_dlopenFailure(_absentLibrary));
  if (Frustrate.isInstalled) {
    complaints.add('a failed init installed a transport');
  }
  // Nothing was recorded, so a *different* path is a first init and fails the
  // same way — it is not reported as a disagreement with a bridge that was
  // never installed.
  complaints.addAll(_dlopenFailure('/nonexistent/libanother.dylib'));
  return complaints;
}

/// Init [path] and require the failure to be **dlopen's**, by type.
///
/// Matched exactly rather than caught broadly, because "it threw something" is
/// not the property under test: the point is that the call got all the way to
/// `DynamicLibrary.open` and failed there. A contract `StateError`, or any
/// future refusal raised before the library is reached, has to show up as a
/// complaint rather than pass for the expected failure.
List<String> _dlopenFailure(String path) {
  try {
    FrustrateNative.init(path);
    return ['opening a nonexistent library should have thrown'];
  } on ArgumentError catch (e) {
    // dart:ffi's refusal: "Failed to load dynamic library ... (no such file)".
    return e.toString().contains('Failed to load dynamic library')
        ? const []
        : ['an ArgumentError, but not dlopen\'s: $e'];
  } catch (e) {
    return ['expected dlopen to refuse $path; got ${e.runtimeType}: $e'];
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
