/// The trap-explanation heuristic (`src/trap_explain.dart`).
///
/// `tests/dart_integration/test/trap_attribution_test.dart` covers the seam
/// this feeds: a Rust `panic!` reaching Dart as a `BridgePanicException`. That
/// path never gets here, because a panic ships its message through the
/// `frustrate.panic` hook and the runtime prefers it. This covers the other
/// branch — a trap or abort, where no hook ran and the engine's text is all
/// that survives.
///
/// Kept as a unit test rather than an end-to-end one because reaching the real
/// branch means causing a real trap, and neither shape is cheap to stage: the
/// `Atomics.wait` case needs a threaded-web build plus genuine cross-thread
/// contention, and the abort case needs an `rtabort!`, which on native would
/// take the test process down with it. What can drift is the mapping and the
/// never-replace property, and both are here.
library;

import 'package:frustrate/src/trap_explain.dart';
import 'package:test/test.dart';

/// Chromium's actual wording, which is what the browser suites run against.
const _atomicsTrap =
    'RuntimeError: Atomics.wait cannot be called in this context';
const _unreachableTrap = 'RuntimeError: unreachable';

void main() {
  group('recognised shapes are explained', () {
    test('a main-thread wait names the cause and the ways out', () {
      final out = explainTrap(_atomicsTrap);
      expect(out, contains('browser main thread'));
      // Each remedy the runtime actually offers. If one is renamed or dropped,
      // this is the reminder that the advice went stale.
      expect(out, contains('async fn'));
      expect(out, contains('Actor'));
      expect(out, contains('native_only'));
      expect(out, contains('on_contention'));
    });

    test('a bare abort names the reentrant-RwLock cause', () {
      final out = explainTrap(_unreachableTrap);
      expect(out, contains('RwLock'));
      expect(out, contains('rtabort!'));
      // Why the message is missing rather than merely unhelpful: an abort runs
      // no panic hook, and the stock std has nowhere to put the reason.
      expect(out, contains('drops the reason'));
      // And the way out. This guess exists only because the real string was
      // dropped; the custom std's `stdio` facility puts it in the console
      // verbatim, so the explanation has to say so or a reader stops here.
      expect(out, contains('stdio'));
    });
  });

  group('the engine message is never lost', () {
    // The whole safety argument for matching on engine text: a wrong guess
    // must not be able to hide what actually happened.
    for (final trap in [_atomicsTrap, _unreachableTrap]) {
      test('"$trap" survives verbatim', () {
        expect(explainTrap(trap), startsWith(trap));
      });
    }

    test('an unrecognised trap is returned untouched', () {
      const other = 'RuntimeError: memory access out of bounds';
      expect(explainTrap(other), other);
    });

    test('an empty message is not decorated', () {
      expect(explainTrap(''), '');
    });
  });

  test('the two shapes do not both match one message', () {
    // `Atomics.wait` is checked first. Were a future engine to word the
    // main-thread trap using the word "unreachable" as well, the more specific
    // explanation must still win — the generic one would send a reader looking
    // for a lock bug that is not there.
    const both = 'RuntimeError: Atomics.wait ... unreachable';
    expect(explainTrap(both), contains('browser main thread'));
    expect(explainTrap(both), isNot(contains('rtabort!')));
  });
}
