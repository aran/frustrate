/// Runs the JS-number codec arm's browser half: `dart test -p chrome -c
/// dart2js` over `test/js_numbers_web_test.dart`.
///
/// Why a `dart_binary` and not a `dart_test`, same as
/// `//tests/dart_integration:web_test`: rules_dart's `dart_test` compiles to a
/// VM kernel and has no browser launcher, and a Chrome run is not hermetic
/// under Bazel. Per the "if it must be a manual run, make it a dart binary
/// (never a shell script)" rule, the invocation lives here.
///
/// It matters because the arm it covers runs on no backend `bazel test`
/// exercises, and its failure mode is silently wrong bytes rather than a
/// crash.
///
/// The codec imports `package:frustrate_i64_list`, which pub does not know:
/// Bazel picks its variant per compiler. The run adds the dart2js variant to
/// pub's package config, in a temporary file outside the source tree.
///
/// Usage:
///   bazel run //runtime/dart:js_arm_web_test
///   bazel run //runtime/dart:js_arm_web_test -- --goldens
///
/// `--goldens` regenerates `test/js_numbers_goldens.dart` from the VM's
/// `setInt64` instead of running the browser. The generated table is checked
/// by `js_numbers_test` on every `bazel test`, so a regeneration that changed
/// a byte would be caught there rather than trusted.
library;

import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

const List<int> _values = [
  0, 1, -1, 2, -2, 127, -128, 255, -256,
  0xFFFF, -0x10000, 0x7FFFFFFF, -0x80000000,
  0xFFFFFFFF, -0xFFFFFFFF, // low half saturated, both signs
  0x100000000, -0x100000000, // exactly one high unit
  0x100000001, -0x100000001, // high unit plus a low bit
  0x1FFFFFFFFF, -0x1FFFFFFFFF,
  9007199254740991, -9007199254740991,
  9007199254740992, -9007199254740992, // 2^53: accepted, both signs
];

/// Wire patterns that must be rejected on read. The first is 2^53 + 1 — the
/// value that rounds down to 2^53 on a JS number, and so slips past any guard
/// written on the reconstructed value rather than on the halves.
final List<int> _outOfRange = [
  9007199254740992 + 1,
  9007199254740992 + 2,
  -9007199254740992 - 2,
  1 << 62,
];

Future<void> main(List<String> args) async {
  final workspace = Platform.environment['BUILD_WORKSPACE_DIRECTORY'];
  final packageDir = workspace != null
      ? '$workspace/runtime/dart'
      : Directory.current.path;

  if (args.contains('--goldens')) {
    _writeGoldens('$packageDir/test/js_numbers_goldens.dart');
    return;
  }

  final scratch = Directory.systemTemp.createTempSync('frustrate_js_arm.');
  final packages = _withJsI64List(packageDir, scratch.path);
  stdout.writeln(
    r'$ dart test -p chrome -c dart2js test/js_numbers_web_test.dart'
    '  (in $packageDir, with the dart2js frustrate_i64_list)',
  );
  final proc = await Process.start(
    'dart',
    [
      '--packages=$packages',
      _testRunner(packages),
      '-p',
      'chrome',
      '-c',
      'dart2js',
      'test/js_numbers_web_test.dart',
    ],
    workingDirectory: packageDir,
    mode: ProcessStartMode.inheritStdio,
  );
  final code = await proc.exitCode;
  scratch.deleteSync(recursive: true);
  exit(code);
}

/// Pub's package config for [packageDir] plus `frustrate_i64_list`'s dart2js
/// variant, written under [scratch]; returns its path.
String _withJsI64List(String packageDir, String scratch) {
  final pub = File('$packageDir/.dart_tool/package_config.json');
  if (!pub.existsSync()) {
    stderr.writeln('js_arm_web_test: run `dart pub get` in $packageDir first.');
    exit(2);
  }
  final config = jsonDecode(pub.readAsStringSync()) as Map<String, Object?>;
  final base = pub.parent.uri;
  final packages = [
    for (final p in (config['packages'] as List).cast<Map<String, Object?>>())
      if (p['name'] != 'frustrate_i64_list')
        {...p, 'rootUri': base.resolve(p['rootUri'] as String).toString()},
    {
      'name': 'frustrate_i64_list',
      'rootUri': Directory('$packageDir/i64_list/js').uri.toString(),
      'packageUri': 'lib/',
      'languageVersion': '3.13',
    },
  ];
  final path = '$scratch/package_config.json';
  File(path).writeAsStringSync(jsonEncode({...config, 'packages': packages}));
  return path;
}

/// The pub `test` runner's entry point, from [packageConfig].
String _testRunner(String packageConfig) {
  final config = jsonDecode(
    File(packageConfig).readAsStringSync(),
  ) as Map<String, Object?>;
  final test = (config['packages'] as List).cast<Map>().firstWhere(
    (p) => p['name'] == 'test',
  );
  final root = test['rootUri'] as String;
  return Uri.parse(root.endsWith('/') ? root : '$root/')
      .resolve('bin/test.dart')
      .toFilePath();
}

String _bytes(int v) {
  final d = ByteData(8)..setInt64(0, v, Endian.little);
  final hex = [
    for (final b in d.buffer.asUint8List())
      '0x${b.toRadixString(16).toUpperCase().padLeft(2, '0')}',
  ];
  return '[${hex.join(', ')}]';
}

void _writeGoldens(String path) {
  final b = StringBuffer()
    ..writeln(
      '/// Wire goldens for the JS-number codec arm: values, and the '
      'bytes a real',
    )
    ..writeln('/// 64-bit `setInt64` produces for them.')
    ..writeln('///')
    ..writeln(
      '/// GENERATED by `bazel run //runtime/dart:js_arm_web_test -- '
      '--goldens`.',
    )
    ..writeln(
      '/// Do not hand-edit: `js_numbers_test` re-derives every row '
      'from `setInt64`',
    )
    ..writeln(
      '/// on the VM, so an edited byte fails there rather than '
      'silently weakening',
    )
    ..writeln('/// the browser half that consumes it.')
    ..writeln('///')
    ..writeln(
      '/// The two halves exist because neither backend can do the job '
      'alone: dart2js',
    )
    ..writeln(
      '/// has no `setInt64` to compare against, and the VM cannot '
      'reproduce the arm\'s',
    )
    ..writeln(
      '/// one double-specific hazard — reconstructing 2^53 + 1 rounds '
      'it down to',
    )
    ..writeln(
      '/// 2^53, so a guard on the reconstructed value passes there '
      'and fails here.',
    )
    ..writeln('library;')
    ..writeln()
    ..writeln('/// (value, little-endian i64 bytes).')
    ..writeln('const List<(int, List<int>)> i64Goldens = [');
  for (final v in _values) {
    b.writeln('  ($v, ${_bytes(v)}),');
  }
  b
    ..writeln('];')
    ..writeln()
    ..writeln(
      '/// Byte patterns outside the exact range. Every one must throw '
      'on read.',
    )
    ..writeln('const List<List<int>> outOfRangeBytes = [');
  for (final v in _outOfRange) {
    b.writeln('  ${_bytes(v)},');
  }
  b.writeln('];');
  File(path).writeAsStringSync(b.toString());
  stdout.writeln(
    'wrote $path (${_values.length} rows, '
    '${_outOfRange.length} reject patterns)',
  );
}
