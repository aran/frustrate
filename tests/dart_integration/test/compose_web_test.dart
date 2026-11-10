/// Two bridges composed into one wasm module (`frustrate_shared_library` +
/// `frustrate_wasm_module`, //tests/compose): one module, one transport, each
/// bridge with its own exports, schema and handles. The VM half is
/// //tests/compose:compose_test.
///
/// The module and both bridges' bindings are built by Bazel and served by
/// `bazel run //tests/dart_integration:web_test`; `dart run
/// tool/web_test.dart` has no Bazel outputs to serve and leaves this file out
/// by its tag.
@TestOn('browser')
library;

import 'dart:async';

import 'package:frustrate/frustrate.dart';
import 'package:frustrate/frustrate_web.dart';
import 'package:compose_alpha/compose_alpha.frustrate.dart' as alpha;
import 'package:compose_beta/compose_beta.frustrate.dart' as beta;
import 'package:frustrate/testing.dart';
import 'package:test/test.dart';

/// Everything a stream-taking call delivers, through to its end.
Future<List<int>> _items(
  Future<void> Function(StreamController<int> sink) call,
) async {
  final sink = StreamController<int>();
  final items = sink.stream.toList();
  await call(sink);
  return items;
}

final class _FakeBeta extends beta.FakeComposeBeta {
  @override
  int betaGet() => 99;
}

FakeRuntime _fakeBeta() => FakeRuntime(beta.FakeComposeBetaBridge(_FakeBeta()));

const _module = '../packages/web_test_fixtures/composed.wasm';

/// A bridge the module does not link.
const _absent = FrustrateBridge('compose_gamma');

Future<void> _initBoth() async {
  await FrustrateWeb.initFromUrl(alpha.composeAlphaBridge, _module);
  await FrustrateWeb.initFromUrl(beta.composeBetaBridge, _module);
  alpha.checkFrustrateSchema();
  beta.checkFrustrateSchema();
}

void main() {
  // In order, on a page nothing has initialized yet.
  group('init', () {
    test('a module that does not carry the bridge is refused', () async {
      await expectLater(
        FrustrateWeb.initFromUrl(_absent, _module),
        throwsA(
          isA<StateError>().having(
            (e) => e.message,
            'message',
            allOf(
              contains('does not carry'),
              contains('frustrate_compose_gamma_call_sync'),
            ),
          ),
        ),
      );
      expect(Frustrate.isInstalled, isFalse);
    });

    test('a bridge is served once its own init names it', () async {
      await FrustrateWeb.initFromUrl(alpha.composeAlphaBridge, _module);
      expect(Frustrate.isServed(alpha.composeAlphaBridge), isTrue);
      expect(Frustrate.isServed(beta.composeBetaBridge), isFalse);
      expect(() => Frustrate.of(beta.composeBetaBridge), throwsStateError);

      await FrustrateWeb.initFromUrl(beta.composeBetaBridge, _module);
      expect(Frustrate.isServed(beta.composeBetaBridge), isTrue);
    });

    test('a bridge the module does not carry cannot attach', () async {
      await expectLater(
        FrustrateWeb.initFromUrl(_absent, _module),
        throwsA(
          isA<StateError>().having(
            (e) => e.message,
            'message',
            contains('does not carry'),
          ),
        ),
      );
      expect(Frustrate.isServed(_absent), isFalse);
    });
  });

  group('two bridges', () {
    setUpAll(_initBoth);

    test('one module serves both bridges through one transport', () {
      final a = Frustrate.of(alpha.composeAlphaBridge);
      final b = Frustrate.of(beta.composeBetaBridge);
      expect(identical(a, b), isFalse, reason: 'each bridge has its runtime');
      expect(identical(a.bridgeIdentity, b.bridgeIdentity), isTrue);
      expect(() => Frustrate.instance, throwsStateError);
    });

    test('state one bridge writes, the other reads: one Rust runtime', () {
      alpha.alphaPut(v: 41);
      expect(beta.betaGet(), 41);
      beta.betaPut(v: 7);
      expect(alpha.alphaGet(), 7);
    });

    test('an async call on each bridge', () async {
      expect(await alpha.alphaTwice(x: 21), 42);
      expect(await beta.betaTwice(x: 5), 10);
    });

    test('a stream on each bridge', () async {
      expect(await _items((s) => alpha.alphaCount(n: 3, sink: s)), [1, 2, 3]);
      expect(await _items((s) => beta.betaCount(n: 2, sink: s)), [1, 2]);
    });

    test('each bridge frees its handles through its own drop export', () {
      final alphaBefore = alpha.alphaDrops();
      final betaBefore = beta.betaDrops();
      final a = alpha.AlphaBox.new_(value: 1);
      final b = beta.BetaBox.new_(value: 2);
      expect(a.value(), 1);
      expect(b.value(), 2);
      a.dispose();
      expect(alpha.alphaDrops(), alphaBefore + 1);
      expect(beta.betaDrops(), betaBefore);
      b.dispose();
      expect(beta.betaDrops(), betaBefore + 1);
    });

    test(
      'a real handle of the faked bridge is refused by the handle check',
      () {
        final box = beta.BetaBox.new_(value: 4);
        Frustrate.activate(_fakeBeta());
        try {
          expect(box.value, throwsStateError);
        } finally {
          Frustrate.reset(beta.composeBetaBridge);
        }
        expect(box.value(), 4);
        box.dispose();
      },
    );

    test('faking beta does not wait for alpha\'s work', () async {
      final held = StreamController<int>();
      final heldItems = held.stream.toList();
      await alpha.alphaHold(sink: held);
      alpha.alphaCloseGate();
      final waiting = alpha.alphaWait();
      expect(Frustrate.of(alpha.composeAlphaBridge).inFlightCallCount, 1);
      expect(Frustrate.of(alpha.composeAlphaBridge).openChannelCount, 1);

      Frustrate.activate(_fakeBeta());
      expect(beta.betaGet(), 99);
      Frustrate.reset(beta.composeBetaBridge);

      alpha.alphaOpenGate();
      expect(await waiting, 1);
      alpha.alphaEndHeld();
      expect(await heldItems, isEmpty);
    });

    test('faking beta is refused while beta has work', () async {
      beta.betaCloseGate();
      final waiting = beta.betaWait();
      expect(() => Frustrate.activate(_fakeBeta()), throwsStateError);
      beta.betaOpenGate();
      expect(await waiting, 1);

      final held = StreamController<int>();
      final heldItems = held.stream.toList();
      await beta.betaHold(sink: held);
      expect(() => Frustrate.activate(_fakeBeta()), throwsStateError);
      beta.betaEndHeld();
      expect(await heldItems, isEmpty);
    });
  });
}
