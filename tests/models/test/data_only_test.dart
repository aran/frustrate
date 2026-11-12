import 'package:models_a_data/models_a.frustrate.data.dart';
import 'package:test/test.dart';

void main() {
  test('the data classes are values without the bridge', () {
    expect(const ShapeCircle(r: 1.5), const ShapeCircle(r: 1.5));
    expect(const ShapeSquare(2).hashCode, const ShapeSquare(2).hashCode);
    expect(const ShapeCircle(r: 1.5), isNot(const ShapeSquare(1.5)));
    expect(const ShapeCircle(r: 1).copyWith(r: 2), const ShapeCircle(r: 2));
    expect(Mood.values, [Mood.calm, Mood.loud]);
  });

  test('copyWith keeps what it is not given and sets what it is', () {
    const r = Reading(value: 1.5, hint: 0.5, label: 'a');
    expect(r.copyWith(), r);
    expect(
      r.copyWith(value: 2),
      const Reading(value: 2, hint: 0.5, label: 'a'),
    );
    expect(r.copyWith(hint: 3), const Reading(value: 1.5, hint: 3, label: 'a'));
    expect(
      r.copyWith(hint: null, label: null),
      const Reading(value: 1.5, hint: null, label: null),
    );
  });
}
