import 'package:models_a_data/models_a.frustrate.data.dart';
import 'package:test/test.dart';

void main() {
  test('the data classes are values without the bridge', () {
    expect(const ShapeCircle(r: 1.5), const ShapeCircle(r: 1.5));
    expect(const ShapeSquare(2).hashCode, const ShapeSquare(2).hashCode);
    expect(const ShapeCircle(r: 1.5), isNot(const ShapeSquare(1.5)));
    expect(const ShapeCircle(r: 1).copyWith(r: 2.0), const ShapeCircle(r: 2));
    expect(Mood.values, [Mood.calm, Mood.loud]);
  });
}
