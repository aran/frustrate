/// Two bridges of one module initialized concurrently, on a page nothing has
/// initialized yet: the second init waits for the transport the first is
/// building and attaches to it, rather than building a second one.
///
/// Served like compose_web_test.dart, by
/// `bazel run //tests/dart_integration:web_test`.
@TestOn('browser')
library;

import 'package:frustrate/frustrate.dart';
import 'package:frustrate/frustrate_web.dart';
import 'package:compose_alpha/compose_alpha.frustrate.dart' as alpha;
import 'package:compose_beta/compose_beta.frustrate.dart' as beta;
import 'package:test/test.dart';

void main() {
  test('concurrent inits of two bridges share one transport', () async {
    const module = '../packages/web_test_fixtures/composed.wasm';
    await Future.wait([
      FrustrateWeb.initFromUrl(alpha.frustrateBridge, module),
      FrustrateWeb.initFromUrl(beta.frustrateBridge, module),
    ]);
    final a = Frustrate.of(alpha.frustrateBridge);
    final b = Frustrate.of(beta.frustrateBridge);
    expect(identical(a.bridgeIdentity, b.bridgeIdentity), isTrue);
    alpha.alphaPut(v: 3);
    expect(beta.betaGet(), 3);
  });
}
