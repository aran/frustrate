/// Two bridges with the same type and member names, linked into one library
/// (`frustrate_library`): each generated export carries its crate name, so
/// each call reaches its own crate.
@TestOn('vm')
library;

import 'package:compose_twin_one/compose_twin_one.frustrate.dart' as one;
import 'package:compose_twin_two/compose_twin_two.frustrate.dart' as two;
import 'package:frustrate/frustrate.dart';
import 'package:test/test.dart';

void main() {
  setUpAll(() {
    FrustrateNative.init(one.composeTwinOneBridge);
    one.checkFrustrateSchema();
    FrustrateNative.init(two.composeTwinTwoBridge);
    two.checkFrustrateSchema();
  });

  test('both bridges resolve into one image', () {
    expect(
      two.composeTwinTwoBridge.address(NativeExport.runtimeAbi).address,
      one.composeTwinOneBridge.address(NativeExport.runtimeAbi).address,
    );
  });

  test('a call reaches the crate of the bridge it was made through', () {
    one.put(v: 1);
    two.put(v: 2);
    expect(one.get(), 1);
    expect(two.get(), 2);
  });

  test('an async call reaches its own crate', () async {
    expect(await one.twice(x: 4), 8);
    expect(await two.twice(x: 5), 10);
  });

  test('a handle is freed through its own bridge\'s drop export', () {
    final oneBefore = one.drops();
    final twoBefore = two.drops();
    final a = one.Twin.new_(value: 3);
    final b = two.Twin.new_(value: 4);
    expect(a.value(), 3);
    expect(b.value(), 4);
    a.dispose();
    expect(one.drops(), oneBefore + 1);
    expect(two.drops(), twoBefore);
    b.dispose();
    expect(two.drops(), twoBefore + 1);
    expect(one.drops(), oneBefore + 1);
  });
}
