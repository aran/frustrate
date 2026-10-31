/// Two bridges composed into one library (`frustrate_library`), on the VM:
/// one transport, one Rust runtime, each bridge with its own exports, schema
/// and handles.
@TestOn('vm')
library;

import 'dart:async';
import 'dart:ffi';

import 'package:compose_alpha/compose_alpha.frustrate.dart' as alpha;
import 'package:compose_beta/compose_beta.frustrate.dart' as beta;
import 'package:frustrate/frustrate.dart';
import 'package:frustrate/testing.dart';
import 'package:test/test.dart';

final class _FakeBeta extends beta.FakeComposeBeta {
  @override
  int betaGet() => 99;
}

@Native<Uint64 Function()>(
  symbol: RuntimeSymbol.runtimeAbi,
  assetId: 'package:compose_alpha/alone',
)
external int _aloneRuntimeAbi();

/// Alpha built alone, as its own library (`:alpha_alone`): a second image,
/// with a Rust runtime of its own.
final class _AlphaAlone implements BridgeLibrary {
  const _AlphaAlone();

  @override
  String get name => 'package:compose_alpha/alone';

  @override
  FrustrateBridge get bridge => alpha.frustrateBridge;

  @override
  Pointer<Void> address(NativeExport export) => switch (export) {
    NativeExport.runtimeAbi =>
      Native.addressOf<NativeFunction<Uint64 Function()>>(_aloneRuntimeAbi)
          .cast(),
    _ => throw ArgumentError('the test asks alpha alone only for its ABI'),
  };

  @override
  HandleExports handle(String dropSymbol) =>
      throw ArgumentError('the test asks alpha alone only for its ABI');
}

/// Everything a stream-taking call delivers, through to its end.
Future<List<int>> _items(
  Future<void> Function(StreamController<int> sink) call,
) async {
  final sink = StreamController<int>();
  final items = sink.stream.toList();
  await call(sink);
  return items;
}

void main() {
  setUpAll(() {
    FrustrateNative.init(alpha.composeAlphaBridge);
    alpha.checkFrustrateSchema();
    FrustrateNative.init(beta.composeBetaBridge);
    beta.checkFrustrateSchema();
  });

  test('both bridges resolve into one image', () {
    // What the runtime decides "one image" by: a runtime export resolves to
    // one address through both bridges' tables.
    expect(
      beta.composeBetaBridge.address(NativeExport.runtimeAbi).address,
      alpha.composeAlphaBridge.address(NativeExport.runtimeAbi).address,
    );
  });

  test('the second bridge attaches to the transport the first built', () {
    final a = Frustrate.of(alpha.frustrateBridge);
    final b = Frustrate.of(beta.frustrateBridge);
    expect(identical(a, b), isFalse, reason: 'each bridge has its runtime');
    expect(identical(a.bridgeIdentity, b.bridgeIdentity), isTrue);
  });

  test('state one bridge writes, the other reads: one Rust runtime', () {
    alpha.alphaPut(v: 41);
    expect(beta.betaGet(), 41);
    beta.betaPut(v: 7);
    expect(alpha.alphaGet(), 7);
  });

  test('an async call on each bridge completes through the one port', () async {
    expect(await alpha.alphaTwice(x: 21), 42);
    expect(await beta.betaTwice(x: 5), 10);
  });

  test('a stream on each bridge', () async {
    expect(await _items((s) => alpha.alphaCount(n: 3, sink: s)), [1, 2, 3]);
    expect(await _items((s) => beta.betaCount(n: 2, sink: s)), [1, 2]);
    expect(Frustrate.of(alpha.frustrateBridge).openChannelCount, 0);
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
    expect(alpha.alphaDrops(), alphaBefore + 1);
  });

  test('a bridge from a separately built library is refused', () {
    expect(
      () => FrustrateNative.init(const _AlphaAlone()),
      throwsA(
        isA<StateError>().having(
          (e) => e.message,
          'message',
          contains('frustrate_library'),
        ),
      ),
    );
    alpha.alphaPut(v: 5);
    expect(alpha.alphaGet(), 5, reason: 'the composed library still serves');
  });

  test('a real handle of the faked bridge is refused by the handle check', () {
    final box = beta.BetaBox.new_(value: 4);
    final keep = alpha.AlphaBox.new_(value: 5);
    Frustrate.activate(FakeRuntime(beta.FakeComposeBetaBridge(_FakeBeta())));
    try {
      expect(
        box.value,
        throwsA(
          isA<StateError>().having(
            (e) => e.message,
            'message',
            contains('different bridge'),
          ),
        ),
      );
      expect(
        keep.value(),
        5,
        reason: 'alpha is still served by what minted it',
      );
    } finally {
      Frustrate.reset(beta.frustrateBridge);
    }
    expect(box.value(), 4, reason: 'usable again once its runtime is back');
    box.dispose();
    keep.dispose();
  });

  test('faking beta does not wait for alpha\'s work', () async {
    final held = StreamController<int>();
    final heldItems = held.stream.toList();
    await alpha.alphaHold(sink: held);
    alpha.alphaCloseGate();
    final waiting = alpha.alphaWait();
    final alphaRuntime = Frustrate.of(alpha.frustrateBridge);
    expect(alphaRuntime.inFlightCallCount, 1);
    expect(alphaRuntime.openChannelCount, 1);
    expect(Frustrate.of(beta.frustrateBridge).inFlightCallCount, 0);
    expect(Frustrate.of(beta.frustrateBridge).openChannelCount, 0);

    Frustrate.activate(FakeRuntime(beta.FakeComposeBetaBridge(_FakeBeta())));
    expect(beta.betaGet(), 99);
    Frustrate.reset(beta.frustrateBridge);

    alpha.alphaOpenGate();
    expect(await waiting, 1);
    alpha.alphaEndHeld();
    expect(await heldItems, isEmpty);
  });

  test('faking beta is refused while beta has work', () async {
    StateError? refusal() {
      try {
        Frustrate.activate(
          FakeRuntime(beta.FakeComposeBetaBridge(_FakeBeta())),
        );
      } on StateError catch (e) {
        return e;
      }
      Frustrate.reset(beta.frustrateBridge);
      return null;
    }

    beta.betaCloseGate();
    final waiting = beta.betaWait();
    expect(refusal()?.message, contains('1 call(s) in flight'));
    beta.betaOpenGate();
    expect(await waiting, 1);

    final held = StreamController<int>();
    final heldItems = held.stream.toList();
    await beta.betaHold(sink: held);
    expect(refusal()?.message, contains('1 open channel(s)'));
    beta.betaEndHeld();
    expect(await heldItems, isEmpty);
    expect(refusal(), isNull, reason: 'quiet again, so the swap goes through');
  });

  test('one bridge is faked while the other stays real', () {
    alpha.alphaPut(v: 3);
    Frustrate.activate(FakeRuntime(beta.FakeComposeBetaBridge(_FakeBeta())));
    addTearDown(() => Frustrate.reset(beta.frustrateBridge));
    expect(beta.betaGet(), 99, reason: 'beta answers from the fake');
    expect(alpha.alphaGet(), 3, reason: 'alpha still reaches Rust');
    final box = alpha.AlphaBox.new_(value: 9);
    expect(box.value(), 9);
    box.dispose();
  });
}
