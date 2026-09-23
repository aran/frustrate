/// The strict-CSP delivery path: when
/// `$frustrateGlueUrl` names a served copy of the glue, the runtime creates
/// its workers from that URL instead of blob: URLs (which strict CSP
/// blocks). The suite proves the path positively — `URL.createObjectURL` is
/// instrumented before init, so a silent fallback to blob: fails the run —
/// and exercises both pump protocols through served-URL workers: the actor
/// pump on every web fixture, the threaded pool pump where the pool is
/// real. The page itself runs CSP-free (`dart test`'s server sets no
/// headers); the end-to-end proof under an enforced policy is the Flutter
/// demo's Playwright specs, whose *bundled* index.html carries the CSP meta
/// (build-injected; the source page carries none, so the dev loop can serve
/// it).
///
/// A bad-URL negative is deliberately not tested here: the runtime installs
/// once per page, and the loud `onerror`/SecurityError paths are the same
/// code this suite routes through — the blob-count assertion already proves
/// which path ran.
@TestOn('browser')
library;

import 'dart:js_interop';
import 'dart:js_interop_unsafe';

import 'package:frustrate/frustrate.dart';
import 'package:frustrate/frustrate_web.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

int _blobUrlCount = 0;

/// Wrap `URL.createObjectURL` with a counting delegate. Installed before
/// init, so every worker the runtime creates afterwards is attributable to
/// either the served-URL path (count stays 0) or the blob fallback.
void _instrumentCreateObjectUrl() {
  final url = globalContext.getProperty<JSObject>('URL'.toJS);
  final original = url.getProperty<JSFunction>('createObjectURL'.toJS);
  url.setProperty(
    'createObjectURL'.toJS,
    ((JSAny? blob) {
      _blobUrlCount++;
      return original.callAsFunction(url, blob);
    }).toJS,
  );
}

void main() {
  setUpAll(() async {
    _instrumentCreateObjectUrl();
    // The manual escape hatch: name the served copy before init. Relative
    // URLs resolve against the test document (<secret>/test/<suite>.html),
    // same as init_web.dart's wasm fetch — Worker construction uses the
    // identical resolution rule.
    globalContext.setProperty(
      r'$frustrateGlueUrl'.toJS,
      '../build/frustrate.js'.toJS,
    );
    await FrustrateWeb.initFromUrl('../build/test_api.wasm');
  });

  group('served glue (strict-CSP delivery)', () {
    test(
      'actor round-trip through a worker created from the served URL',
      () async {
        final m = await Miner.new_(label: 'csp');
        expect(await m.label(), 'csp');
        expect(await m.nthPrime(n: 100), 541);
        await m.dispose();
      },
    );

    test('pool workers come from the served URL (threaded fixture)', () async {
      if (!Frustrate.instance.asyncIsParallel) {
        markTestSkipped('single-threaded web: async inline, no pool');
        return;
      }
      // Exercises the glue's pool-init branch: the probe only reports true
      // if real pool workers (spawned from the served script) ran it.
      rendezvousReset();
      final width = poolWidth();
      final ok = await Future.wait(
        List.generate(
          width,
          (_) => poolRendezvous(width: width, maxWaitMs: 10000),
        ),
      );
      expect(ok, everyElement(isTrue));
      expect(await sumSquares(n: 10), 385);
    });

    test('no blob: URL was ever created', () {
      expect(
        _blobUrlCount,
        0,
        reason:
            'the runtime fell back to blob: workers despite '
            '\$frustrateGlueUrl being set before init',
      );
    });
  });
}
