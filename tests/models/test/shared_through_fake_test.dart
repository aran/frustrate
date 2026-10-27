import 'package:frustrate/frustrate.dart';
import 'package:frustrate/testing.dart';
import 'package:models_a/models_a.frustrate.dart' as a;
import 'package:models_b/models_b.frustrate.dart' as b;
import 'package:test/test.dart';

final class _B extends b.FakeModelsB {
  final List<a.Block> received = [];

  @override
  a.Block echo({required a.Block block}) {
    received.add(block);
    return block;
  }

  @override
  a.Graph wrap({required a.Block block, required a.Shape shape}) {
    received.add(block);
    return a.Graph(blocks: [block], shape: shape, mood: a.Mood.calm);
  }
}

void main() {
  setUpAll(() {
    FrustrateNative.init(a.modelsABridge);
    a.checkFrustrateSchema();
  });

  test('a value from A crosses B and comes back as A\'s class', () {
    final block = a.makeBlock(id: 7, text: 'seven');
    expect(block.weight(), 6);

    final fake = _B();
    Frustrate.activate(FakeRuntime(b.FakeModelsBBridge(fake)));
    final echoed = b.echo(block: block);
    final graph = b.wrap(block: block, shape: const a.ShapeSquare(2));
    Frustrate.reset(b.frustrateBridge);

    expect(fake.received, [block, block]);
    expect(fake.received.first, isA<a.Block>());
    expect(echoed, isA<a.Block>());
    expect(echoed.runtimeType, block.runtimeType);
    expect(echoed, block);
    // B re-exports the classes it takes, so they are one class under either
    // import.
    expect(echoed, isA<b.Block>());

    // B's answer goes straight into A.
    expect(a.describe(graph: graph), '7 square 2 Calm');
  });
}
