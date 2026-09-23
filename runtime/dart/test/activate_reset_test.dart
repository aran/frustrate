/// The installed/active split: which runtime `Frustrate.instance` returns,
/// what may replace it, and when a replacement is refused.
///
/// `install` and `activate` answer two different questions and this file is
/// mostly about keeping them apart. **Installed** is "which platform transport
/// this isolate built" — first-wins, never replaced, never rebuilt, and the
/// thing `isInstalled` reports (the web transport's orphan reap keys on it).
/// **Active** is "where generated bindings send calls" — swappable, so a test
/// can put a fake in front of the bindings and a production build can put a
/// tracing decorator there.
///
/// The rule that keeps a swap from producing a garbage dereference is stated
/// once and applied to both entry points: a swap that **changes bridge
/// identity** requires the outgoing runtime quiescent, and a swap that keeps it
/// (a decorator going on or coming off) is always allowed. Quiescence is
/// exactly what a bridge change would strand — a call the transport still owes
/// a completion, or a channel a Rust producer may still feed.
///
/// The empty-slot cases run in fresh isolates, because `install` is
/// deliberately irreversible: there is no way back to "nothing installed"
/// inside an isolate that has one, and pretending otherwise would need a
/// teardown hook whose only caller is a test.
@TestOn('vm')
library;

import 'dart:isolate';

import 'package:frustrate/frustrate.dart';
import 'package:test/test.dart';

/// A transport with settable liveness, so the quiescence rule can be tested
/// against every combination without a real bridge.
final class _Bridge implements FrustrateRuntime {
  final String name;
  int inFlight = 0;
  List<String> channels = const [];

  _Bridge(this.name);

  @override
  Object get bridgeIdentity => this;

  @override
  int get inFlightCallCount => inFlight;

  @override
  int get openChannelCount => channels.length;

  @override
  List<String> get openChannelLabels => channels;

  @override
  String toString() => 'bridge $name';

  @override
  dynamic noSuchMethod(Invocation invocation) =>
      throw StateError('$this: unexpected ${invocation.memberName}');
}

/// Same bridge, different object — the shape a tracing decorator has.
final class _Decorator implements FrustrateRuntime {
  final FrustrateRuntime inner;

  _Decorator(this.inner);

  @override
  Object get bridgeIdentity => inner.bridgeIdentity;

  @override
  int get inFlightCallCount => inner.inFlightCallCount;

  @override
  int get openChannelCount => inner.openChannelCount;

  @override
  List<String> get openChannelLabels => inner.openChannelLabels;

  @override
  dynamic noSuchMethod(Invocation invocation) =>
      throw StateError('decorator: unexpected ${invocation.memberName}');
}

void main() {
  group('with a transport installed', () {
    final installed = _Bridge('installed');
    final fake = _Bridge('fake');

    setUpAll(
      () => Frustrate.install(
        installed,
        source: installed,
        description: 'the installed transport',
      ),
    );

    setUp(() {
      installed.inFlight = 0;
      installed.channels = const [];
      fake.inFlight = 0;
      fake.channels = const [];
    });

    tearDown(Frustrate.reset);

    test('install makes its transport active, and instance reads active', () {
      expect(identical(Frustrate.instance, installed), isTrue);
      expect(Frustrate.isInstalled, isTrue);
      expect(
        identical(Frustrate.activeBridge, installed.bridgeIdentity),
        isTrue,
      );

      Frustrate.activate(fake);
      expect(identical(Frustrate.instance, fake), isTrue);
      expect(identical(Frustrate.activeBridge, fake.bridgeIdentity), isTrue);
      expect(
        Frustrate.isInstalled,
        isTrue,
        reason:
            'isInstalled reports the platform transport, not the active '
            'one — the web transport reaps orphaned workers on the strength '
            'of it being false',
      );
    });

    test('reset restores the installed transport, and is idempotent', () {
      Frustrate.activate(fake);
      Frustrate.reset();
      expect(identical(Frustrate.instance, installed), isTrue);
      Frustrate.reset();
      expect(identical(Frustrate.instance, installed), isTrue);
    });

    test('activate refuses to stack: one active runtime, declared', () {
      Frustrate.activate(fake);
      expect(
        () => Frustrate.activate(_Bridge('another')),
        throwsA(
          isA<StateError>().having(
            (e) => e.message,
            'message',
            contains('reset'),
          ),
        ),
      );
      // Composition is the caller's to write — `activate(Traced(fake))` — so
      // that what is active is one object with one stated order, rather than a
      // stack the runtime assembled behind their back.
      expect(identical(Frustrate.instance, fake), isTrue);
    });

    group('the quiescence rule', () {
      test('a bridge change is refused while a call is in flight', () {
        installed.inFlight = 2;
        expect(
          () => Frustrate.activate(fake),
          throwsA(
            isA<StateError>().having(
              (e) => e.message,
              'message',
              allOf(contains('2'), contains('in flight')),
            ),
          ),
        );
        expect(
          identical(Frustrate.instance, installed),
          isTrue,
          reason: 'a refused swap changes nothing',
        );
      });

      test(
        'a bridge change is refused while a channel is open, and names it',
        () {
          installed.channels = const ['TextDoc.watch', 'Miner.progress'];
          expect(
            () => Frustrate.activate(fake),
            throwsA(
              isA<StateError>().having(
                (e) => e.message,
                'message',
                allOf(contains('TextDoc.watch'), contains('Miner.progress')),
              ),
            ),
          );
        },
      );

      test('reset is held to the same rule', () {
        Frustrate.activate(fake);
        fake.inFlight = 1;
        expect(
          () => Frustrate.reset(),
          throwsA(
            isA<StateError>().having(
              (e) => e.message,
              'message',
              contains('in flight'),
            ),
          ),
        );
        fake.inFlight = 0;
        Frustrate.reset();
        expect(identical(Frustrate.instance, installed), isTrue);
      });

      test('a decorator goes on and comes off with live work', () {
        // The reason the rule is keyed on bridge identity rather than on the
        // active object: a production decorator over an app with one live
        // stream could otherwise never be removed, and nothing is at stake —
        // handles, channels and hosts all route by bridge, and the bridge has
        // not moved.
        installed.inFlight = 3;
        installed.channels = const ['TextDoc.watch'];
        final traced = _Decorator(installed);
        Frustrate.activate(traced);
        expect(identical(Frustrate.instance, traced), isTrue);
        Frustrate.reset();
        expect(identical(Frustrate.instance, installed), isTrue);
      });
    });

    test('a repeated init still decides on the installed transport', () {
      // With a fake active but a real transport installed, "init names a
      // different bridge" is still a bug: after a reset the installed one
      // would serve while the caller believed the one it named.
      Frustrate.activate(fake);
      expect(
        Frustrate.initializedFrom(installed, 'the installed transport'),
        isTrue,
      );
      expect(
        () => Frustrate.initializedFrom('/other', 'some other bridge'),
        throwsA(
          isA<StateError>().having(
            (e) => e.message,
            'message',
            contains('some other bridge'),
          ),
        ),
      );
    });
  });

  group('in a fresh isolate', () {
    test('a fake can be activated with nothing installed', () async {
      expect(await Isolate.run(_activateWithNothingInstalled), isEmpty);
    });

    test('a platform init under a fake builds nothing', () async {
      expect(await Isolate.run(_initUnderAFakeIsAnEnsure), isEmpty);
    });

    test('reset with nothing installed empties the slot', () async {
      expect(await Isolate.run(_resetWithNothingInstalled), isEmpty);
    });
  });
}

/// Runs in its own isolate: statics start virgin, which is the only way to
/// reach "a fake is active and no platform transport was ever built" — the
/// ordinary shape of a `package:test` file that fakes the bridge.
List<String> _activateWithNothingInstalled() {
  final complaints = <String>[];
  final fake = _Bridge('fake');
  Frustrate.activate(fake);
  if (!identical(Frustrate.instance, fake)) {
    complaints.add('activate did not take effect with an empty installed slot');
  }
  if (Frustrate.isInstalled) {
    complaints.add('activating a fake reported a platform transport installed');
  }
  return complaints;
}

/// Init is an *ensure*. A widget that lazily calls `FrustrateNative.init(path)`
/// under `flutter test`, in a suite that declared a fake, must not dlopen a
/// path that does not exist — so with nothing installed and something active,
/// the guard sends the caller home having built nothing.
List<String> _initUnderAFakeIsAnEnsure() {
  final complaints = <String>[];
  const absent = '/nonexistent/libfrustrate_test_bridge.dylib';
  if (Frustrate.initializedFrom(absent, 'a bridge')) {
    complaints.add('an empty isolate answered a repeated init');
  }
  Frustrate.activate(_Bridge('fake'));
  if (!Frustrate.initializedFrom(absent, 'a bridge')) {
    complaints.add('init under a fake did not short-circuit');
  }
  try {
    FrustrateNative.init(absent);
  } catch (e) {
    complaints.add('init under a fake reached dlopen: $e');
  }
  if (Frustrate.isInstalled) {
    complaints.add('init under a fake installed something');
  }
  return complaints;
}

List<String> _resetWithNothingInstalled() {
  final complaints = <String>[];
  Frustrate.activate(_Bridge('fake'));
  Frustrate.reset();
  if (Frustrate.activeBridge != null) {
    complaints.add('reset with nothing installed left a bridge active');
  }
  try {
    Frustrate.instance;
    complaints.add('instance answered after a reset to nothing');
  } on StateError {
    // Expected: back to "call the platform init first".
  }
  return complaints;
}
