import 'package:frustrate/frustrate.dart';
import 'package:models_a/models_a.frustrate.dart' as a;
import 'package:models_b/models_b.frustrate.dart' as b;
import 'package:test/test.dart';

void main() {
  setUpAll(() {
    FrustrateNative.init(b.modelsBBridge);
    b.checkFrustrateSchema();
  });

  test('B\'s library decodes and returns objects of A\'s classes', () {
    const block = a.Block(id: 3, text: 'three', tags: ['x', 'y']);
    expect(b.echo(block: block), block);

    final graph = b.wrap(block: block, shape: const a.ShapeCircle(r: 1.5));
    expect(graph, isA<a.Graph>());
    expect(graph.blocks, [
      block,
      const a.Block(id: 4, text: 'three!', tags: []),
    ]);
    expect(graph.blocks.last, isA<a.Block>());
    expect(graph.shape, const a.ShapeCircle(r: 1.5));
    expect(graph.mood, a.Mood.loud);
  });
}
