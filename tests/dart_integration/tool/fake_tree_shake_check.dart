/// Does a production build carry the generated fake harness?
///
///   bazel run //tests/dart_integration:fake_tree_shake_check
///
/// The harness is emitted **into** the binding surface, so every build of every
/// app that imports the bindings compiles it. The claim is that a build which
/// never constructs one links none of it, and that claim needs measuring
/// rather than asserting.
///
/// **What this reasons over.** dart2js's own `--dump-info`: the compiler's
/// record of what survived into the output program, element by element. Not the
/// emitted JavaScript, and not a size comparison — a retained-set listing from
/// the compiler answers "is this class in the program" directly, where a size
/// delta only suggests it.
///
/// **Two programs, because one proves nothing.** The subject imports the
/// bindings and calls a free function. The control does the same *and*
/// constructs `FakeTestApiBridge`. If the control does not retain the harness,
/// this check cannot see the harness at all and says so instead of passing —
/// which is the failure mode a one-program check has and cannot detect.
///
/// **Why dart2js and not dart2wasm or AOT.** `--dump-info` is dart2js's; the
/// other two backends have no equivalent retained-set dump. The question is
/// about *reachability*, which is a whole-program property every Dart backend
/// computes the same way — from the set of classes something allocates — so one
/// backend answering it is evidence about the analysis, not about JavaScript.
///
/// **Where it cannot decide**: if `dart compile js` fails, or the info JSON has
/// no `elements.class` map, it exits non-zero saying so rather than reporting a
/// pass.
library;

import 'dart:convert';
import 'dart:io';

const _subject = '''
import 'package:frustrate_integration/test_api.frustrate.dart';

void main() {
  print(addI32(1, 2));
}
''';

const _control = '''
import 'package:frustrate/frustrate.dart';
import 'package:frustrate/testing.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';

class Api extends FakeTestApi {
  @override
  FakeTextDoc textDocNew() => FakeTextDoc();
}

void main() {
  print(addI32(1, 2));
  Frustrate.activate(FakeRuntime(FakeTestApiBridge(Api())));
}
''';

Future<void> main(List<String> args) async {
  final workspace = Platform.environment['BUILD_WORKSPACE_DIRECTORY'];
  final packageDir = workspace != null
      ? '$workspace/tests/dart_integration'
      : Directory.current.path;
  final tmp = await Directory.systemTemp.createTemp('frustrate_shake');
  try {
    // The probes live under the package's own `tool/`, because dart2js resolves
    // `package:` through the package config rooted there.
    final subjectRetained = await _retainedClasses(
      packageDir,
      tmp,
      'subject',
      _subject,
    );
    final controlRetained = await _retainedClasses(
      packageDir,
      tmp,
      'control',
      _control,
    );

    final subjectFakes = _anchors.intersection(subjectRetained).toList()
      ..sort();
    final controlFakes = _anchors.intersection(controlRetained).toList()
      ..sort();

    stdout.writeln(
      'classes retained, subject (never builds a fake): '
      '${subjectRetained.length}',
    );
    stdout.writeln(
      'classes retained, control (builds one):          '
      '${controlRetained.length}',
    );
    stdout.writeln(
      'harness classes in the control: '
      '${controlFakes.length}/${_anchors.length} $controlFakes',
    );
    stdout.writeln(
      'harness classes in the subject: '
      '${subjectFakes.length}/${_anchors.length} $subjectFakes',
    );

    if (controlFakes.length != _anchors.length) {
      stderr.writeln(
        'UNDECIDED: the control builds a FakeTestApiBridge and '
        'dump-info still does not list '
        '${_anchors.difference(controlRetained).toList()..sort()}. Either a '
        'name moved, or this check cannot see the harness — and a clean '
        'subject would then mean nothing.',
      );
      exit(2);
    }
    if (subjectFakes.isNotEmpty) {
      stderr.writeln(
        'FAIL: a build that never constructs a fake still carries '
        '${subjectFakes.length} harness class(es).',
      );
      exit(1);
    }
    stdout.writeln(
      'OK: the harness is absent from a build that never '
      'constructs one.',
    );
  } finally {
    await tmp.delete(recursive: true);
  }
}

/// The classes whose presence answers the question, named rather than matched
/// on a prefix: one generated per-type fake, the generated dispatch, the
/// generated crate fake, the two contract classes a harness always references,
/// and the fake transport itself. A prefix match would also catch an unrelated
/// `Fake*` from some other package and fail for a reason that is not this one.
///
/// The control must retain **every** one of them; if it does not, a name has
/// moved and this check is measuring nothing.
const Set<String> _anchors = {
  'FakeTestApiBridge',
  'FakeTestApi',
  'FakeTextDoc',
  'FakeStreamSink',
  'FakeRequest',
  'FakeRuntime',
};

Future<Set<String>> _retainedClasses(
  String packageDir,
  Directory tmp,
  String label,
  String source,
) async {
  final probe = File('$packageDir/tool/_shake_probe_$label.dart');
  await probe.writeAsString(source);
  try {
    final out = '${tmp.path}/$label.js';
    final result = await Process.run('dart', [
      'compile',
      'js',
      '-O2',
      '--dump-info=json',
      '-o',
      out,
      probe.path,
    ], workingDirectory: packageDir);
    if (result.exitCode != 0) {
      stderr.writeln(result.stdout);
      stderr.writeln(result.stderr);
      throw StateError(
        'UNDECIDED: dart compile js failed for the $label probe',
      );
    }
    final info = jsonDecode(await File('$out.info.json').readAsString()) as Map;
    final elements = info['elements'];
    if (elements is! Map || elements['class'] is! Map) {
      throw StateError(
        'UNDECIDED: the $label probe produced no elements.class map, so '
        'nothing here can read a retained set out of it',
      );
    }
    return {
      for (final e in (elements['class'] as Map).values)
        (e as Map)['name'] as String,
    };
  } finally {
    await probe.delete();
  }
}
