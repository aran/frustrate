/// A build with no web compiler declares `Int64List`, the VM's type.
library;

import 'dart:typed_data';

import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:test/test.dart';

void main() {
  test('Vec<i64> is Int64List without a web compiler', () {
    final Int64List Function({required Int64List xs}) f = echoI64s;
    expect(f, isNotNull);
  });
}
