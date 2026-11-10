import 'package:frustrate/frustrate.dart';
import 'package:models_a/models_a.frustrate.dart' as a;
import 'package:models_b/models_b.frustrate.dart' as b;
import 'package:test/test.dart';

void main() {
  setUpAll(() {
    FrustrateNative.init(a.modelsALibrary);
    a.checkFrustrateSchema();
    FrustrateNative.init(b.modelsBLibrary);
    b.checkFrustrateSchema();
  });

  test('an object A returns goes through B and back into A', () {
    final block = a.makeBlock(id: 7, text: 'seven');
    expect(block.weight(), 6);

    final echoed = b.echo(block: block);
    expect(echoed, block);
    expect(echoed.runtimeType, block.runtimeType);

    final graph = b.wrap(block: block, shape: const a.ShapeSquare(2));
    expect(graph, isA<a.Graph>());
    expect(graph.blocks.first, block);
    expect(graph.blocks.last.weight(), 6);

    expect(a.describe(graph: graph), '7,8 square 2 Loud');
  });
}
