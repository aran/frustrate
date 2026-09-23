/// What a build with asserts **off** carries for handle-bridge safety, and
/// what it refuses instead.
///
///   bazel run //tests/dart_integration:handle_bridge_release_check
///
/// A handle has to know which bridge minted it, because a raw is a value one
/// transport issued and the wire that carries it to another is a bare `u64`
/// with nothing to check. Knowing it costs a field on every handle and a
/// comparison on every use, so it is paid only where asserts are on, and a
/// build without them is kept safe at the other end:
/// `Frustrate.activate` refuses a bridge-identity change there, so no handle
/// can ever meet a bridge that did not mint it.
///
/// Both halves of that are claims about a build no test runner can produce —
/// `dart test` is asserts-on by definition, and the browser vehicle hardcodes
/// `--enable-asserts` — so they are measured here instead, each against a
/// positive control that proves the measurement could have failed.
///
/// ## Part 1: the release program carries none of it
///
/// **What this reasons over.** dart2js's `--dump-info`: the compiler's own
/// record of what survived into the output program, element by element. Not
/// the emitted JavaScript and not a size delta — a retained-set listing
/// answers "is this in the program" directly.
///
/// **One probe, compiled twice**, differing only by `--enable-asserts`, so the
/// control is exact rather than merely similar. It mints an opaque handle,
/// calls a method on it (the one door every handle value takes to the wire)
/// and disposes it, which is every site the bookkeeping lives at.
///
/// Two anchors, and each is chosen because it is what dump-info can actually
/// see at `-O2`. Top-level functions are *not* usable: at `-O2` dart2js inlines
/// them and they stop being elements at all, in both compiles.
///
///   * **`Expando`** — the type of the side table the debug stamp writes to,
///     and nothing else in the runtime uses one. Absent from the asserts-off
///     program, present in the asserts-on one. The asserts-on run is the
///     control: it proves the class is visible to dump-info when something
///     reaches it. Verified against the failure it exists for — with the
///     `assert(...)` wrapper stripped off the check in `handleValue`, so that
///     the same call runs unconditionally, the asserts-off program retains
///     `Expando` and this reports FAIL.
///   * **The fields of `OpaqueHandleBase`** — no field naming a bridge, in
///     either program. `_raw` must be listed as the control, because a class
///     whose fields dump-info does not list would pass this vacuously.
///     Verified the same way: with `final Object? _bridge` put back on the
///     class, the asserts-off program lists it.
///
/// ## Part 2: the release program refuses the swap
///
/// The other half, and the reason part 1 is not a hole: a build that cannot
/// detect a stranded handle must not be able to strand one. The gateway probe
/// activates one fake, mints a handle under it, activates a *second* fake, and
/// then uses the handle — reporting by exit code which of the two doors it got
/// through. Run twice, as one process each:
///
///   * **asserts off** — refused at `activate`, before a second bridge is ever
///     active. That is the release contract.
///   * **asserts on** (`--enable-asserts`) — *not* refused there, and refused
///     at the handle use instead, with the stale-handle message. That is the
///     control: it proves the probe really reaches both doors, so "refused at
///     the first one" means something.
///
/// ## Where it cannot decide
///
/// If a compile fails, if the info JSON carries no `elements.class` map, if a
/// probe process dies in a way neither arm expects, or if a positive control
/// comes up short, it exits non-zero saying UNDECIDED rather than reporting a
/// pass.
///
/// **What it does not measure.** Two things, both worth naming.
///
/// dart2js is the only Dart backend with a retained-set dump; AOT and
/// dart2wasm have no equivalent. The argument that carries part 1 to them is
/// the same one `tool/fake_tree_shake_check.dart` makes: reachability is a
/// whole-program property every backend computes the same way, from what the
/// program can allocate and call, so one backend answering it is evidence
/// about the analysis rather than about JavaScript. Part 2 needs no such
/// argument — it runs a real asserts-off program on the VM.
///
/// And part 1 covers the **opaque** handle path only. The actor receiver read
/// carries the same check in the same construct — an `assert` around a
/// comparison against `ActorHost.bridgeIdentity` — but it touches no side
/// table, so there is no anchor here that its presence would move. What pins
/// that half is the codegen test on the emitted getter
/// (`emit_dart.rs`, `actor_class_shape`), which fails if the comparison is
/// emitted outside the assert or the refusal is dropped from inside it.
library;

import 'dart:convert';
import 'dart:io';

// -------------------------------------------------------------- part 1 --

/// Mints a handle, reaches the wire through it, and drops it: the three sites
/// the bookkeeping would live at if it were still there.
///
/// It never runs. `Frustrate.instance` would throw on the first line — what is
/// being read out of this is what the compiler *kept*, and reachability is a
/// property of the code, not of an execution.
const _retainedProbe = '''
import 'package:frustrate_integration/test_api.frustrate.dart';

void main() {
  final d = TextDoc.new_();
  print(d.lenChars());
  d.dispose();
}
''';

/// The class whose presence would mean the debug side table survived. Named,
/// not prefix-matched: a prefix would also catch an unrelated class and fail
/// for a reason that is not this one.
const String _sideTableClass = 'Expando';

/// The class whose field list is the per-handle storage claim, and the field
/// that must be in it for the claim to mean anything.
const String _handleClassId =
    'package:frustrate/src/opaque_handle_base.dart::OpaqueHandleBase';
const String _handleControlField = '_raw';

/// Fields the handle is allowed to carry. Anything else on this class is
/// per-handle storage that a release build should not be paying for.
const Set<String> _allowedHandleFields = {'_raw', '_drop'};

// -------------------------------------------------------------- part 2 --

/// Exit codes the gateway probe reports itself with, so the two arms are told
/// apart by what happened and not by whether a throw occurred.
const int _refusedAtActivate = 10;
const int _refusedAtUse = 11;
const int _refusedNowhere = 12;

const _gatewayProbe =
    '''
import 'dart:io';

import 'package:frustrate/frustrate.dart';

final class Bridge implements FrustrateRuntime {
  final String name;
  final Drop drop = Drop();
  Bridge(this.name);
  @override
  Object get bridgeIdentity => this;
  @override
  HandleDrop handleDrop(String symbol) => drop;
  @override
  int get inFlightCallCount => 0;
  @override
  int get openChannelCount => 0;
  @override
  List<String> get openChannelLabels => const [];
  @override
  String toString() => 'bridge \$name';
  @override
  dynamic noSuchMethod(Invocation i) =>
      throw StateError('\$this: unexpected \${i.memberName}');
}

final class Drop implements HandleDrop {
  @override
  void attach(Object owner, int raw) {}
  @override
  void detach(Object owner) {}
  @override
  void drop(int raw) {}
}

final class Doc extends OpaqueHandle {
  Doc(super.raw, super.drop);
}

void main() {
  var on = false;
  assert(on = true);
  stderr.writeln('probe: asserts \${on ? 'on' : 'off'}');

  final first = Bridge('first');
  final second = Bridge('second');

  Frustrate.activate(first);
  final h = Doc(0x1234, Frustrate.instance.handleDrop('frustrate_drop_Doc'));

  try {
    Frustrate.reset();
    Frustrate.activate(second);
  } on StateError catch (e) {
    stderr.writeln('probe: refused at activate: \${e.message}');
    exit($_refusedAtActivate);
  }

  try {
    stderr.writeln('probe: handle value \${h.handleValue}');
  } on StateError catch (e) {
    stderr.writeln('probe: refused at use: \${e.message}');
    exit($_refusedAtUse);
  }
  stderr.writeln('probe: a handle minted by \$first reached \$second intact');
  exit($_refusedNowhere);
}
''';

// ----------------------------------------------------------------- main --

/// Marks a process this tool spawned, so one that turns out to be *this tool*
/// stops at the first generation instead of forking exponentially.
///
/// The failure this exists for is not hypothetical. `Platform.resolvedExecutable`
/// is the obvious handle for "run a Dart program" and the wrong one here: it is
/// the Dart VM under `dart run` but this tool's own AOT binary under
/// `bazel run`, where spawning it re-runs the check itself, twice per
/// generation, forever. Part 2 names the SDK instead — but a guard that only
/// lives in the argument being right is one nobody notices going wrong, so this
/// one is structural: a child that reaches [main] refuses.
const String _childMarker = 'FRUSTRATE_HANDLE_BRIDGE_CHECK_CHILD';

/// The check reports UNDECIDED and exits 2 rather than passing. Thrown rather
/// than exited so that a probe written into the source tree is still deleted
/// on the way out.
class _Undecided implements Exception {
  _Undecided(this.message);
  final String message;
}

Future<void> main(List<String> args) async {
  if (Platform.environment.containsKey(_childMarker)) {
    stderr.writeln(
      'handle_bridge_release_check: this process was spawned by '
      'the check itself, which means the probe runner resolved to this tool '
      'rather than to the Dart SDK. Refusing so it cannot fork itself.',
    );
    exit(3);
  }
  final workspace = Platform.environment['BUILD_WORKSPACE_DIRECTORY'];
  final packageDir = workspace != null
      ? '$workspace/tests/dart_integration'
      : Directory.current.path;
  final tmp = await Directory.systemTemp.createTemp('frustrate_handle_bridge');
  var failures = 0;
  try {
    failures += await _checkRetained(packageDir, tmp);
    stdout.writeln('');
    failures += await _checkGateway(packageDir);
  } on _Undecided catch (e) {
    stderr.writeln('UNDECIDED: ${e.message}');
    exit(2);
  } finally {
    await tmp.delete(recursive: true);
  }
  if (failures > 0) {
    stderr.writeln('\n$failures check(s) failed.');
    exit(1);
  }
  stdout.writeln(
    '\nOK: a build without asserts carries no per-handle bridge '
    'stamp and no per-use lookup, and refuses the swap that would need one.',
  );
}

Future<int> _checkRetained(String packageDir, Directory tmp) async {
  stdout.writeln('part 1 — what an asserts-off program retains');
  final off = await _compile(
    packageDir,
    tmp,
    'release',
    _retainedProbe,
    asserts: false,
  );
  final on = await _compile(
    packageDir,
    tmp,
    'debug',
    _retainedProbe,
    asserts: true,
  );

  // Control first: if the asserts-on program does not carry the side table,
  // the anchor is not measuring anything and a clean release program means
  // nothing.
  if (!on.classes.contains(_sideTableClass)) {
    throw _Undecided(
      'the asserts-on program does not retain '
      '$_sideTableClass either, so this check cannot see the debug stamp at '
      'all — a name moved, or the stamp stopped using one.',
    );
  }
  if (!on.fieldsOf(_handleClassId).contains(_handleControlField) ||
      !off.fieldsOf(_handleClassId).contains(_handleControlField)) {
    throw _Undecided(
      'dump-info lists no $_handleControlField field '
      'on $_handleClassId (asserts-on: ${on.fieldsOf(_handleClassId)}, '
      'asserts-off: ${off.fieldsOf(_handleClassId)}), so a handle carrying '
      'an extra field would be just as invisible.',
    );
  }

  var failures = 0;
  stdout.writeln('  classes retained, asserts off: ${off.classes.length}');
  stdout.writeln('  classes retained, asserts on:  ${on.classes.length}');
  stdout.writeln(
    '  $_sideTableClass present, asserts on:  '
    '${on.classes.contains(_sideTableClass)}   (the control)',
  );
  stdout.writeln(
    '  $_sideTableClass present, asserts off: '
    '${off.classes.contains(_sideTableClass)}',
  );
  if (off.classes.contains(_sideTableClass)) {
    stderr.writeln(
      'FAIL: a build without asserts still carries '
      '$_sideTableClass, so the per-use bridge lookup reached the release '
      'program.',
    );
    failures++;
  }

  final offFields = off.fieldsOf(_handleClassId)..sort();
  final onFields = on.fieldsOf(_handleClassId)..sort();
  stdout.writeln('  OpaqueHandleBase fields, asserts off: $offFields');
  stdout.writeln('  OpaqueHandleBase fields, asserts on:  $onFields');
  final extra = {...offFields, ...onFields}.difference(_allowedHandleFields);
  if (extra.isNotEmpty) {
    stderr.writeln(
      'FAIL: OpaqueHandleBase carries ${extra.toList()..sort()} '
      'beyond ${_allowedHandleFields.toList()..sort()} — that is a word on '
      'every handle in every build.',
    );
    failures++;
  }
  return failures;
}

Future<int> _checkGateway(String packageDir) async {
  stdout.writeln('part 2 — what an asserts-off program refuses');
  final probe = File('$packageDir/tool/_handle_bridge_gateway_probe.dart');
  await probe.writeAsString(_gatewayProbe);
  try {
    // `dart` from PATH, the same thing part 1 compiles with — NOT
    // `Platform.resolvedExecutable`, which is this tool's own AOT binary
    // whenever the tool is a `dart_binary` rather than a script. See
    // [_childMarker].
    final release = await _runProbe(packageDir, probe.path, asserts: false);
    final debug = await _runProbe(packageDir, probe.path, asserts: true);

    // Control first, again: the asserts-on arm has to walk past the gateway
    // and be stopped at the handle. If it is not, the probe never reaches the
    // second door and "the release arm stopped at the first" says nothing.
    if (debug.exitCode != _refusedAtUse) {
      throw _Undecided(
        'with asserts on the probe should have been '
        'refused at the handle use ($_refusedAtUse) and exited '
        '${debug.exitCode} instead. Its stderr:\n${debug.stderr}',
      );
    }
    if (!'${debug.stderr}'.contains('different bridge')) {
      throw _Undecided(
        'the asserts-on refusal did not name a '
        'different bridge, so it is not the refusal this is about:\n'
        '${debug.stderr}',
      );
    }
    stdout.writeln(
      '  asserts on:  exit ${debug.exitCode} — past the gateway, '
      'refused at the handle   (the control)',
    );

    var failures = 0;
    if (release.exitCode == _refusedAtActivate &&
        '${release.stderr}'.contains('asserts off')) {
      stdout.writeln(
        '  asserts off: exit ${release.exitCode} — refused at '
        'the gateway, before a second bridge was ever active',
      );
    } else {
      failures++;
      final what = switch (release.exitCode) {
        _refusedAtUse =>
          'let the swap through and caught the handle instead — '
              'a build without asserts cannot do that, so it did not',
        _refusedNowhere =>
          'let a raw from one bridge reach another with nothing said',
        _ => 'died unexpectedly',
      };
      stderr.writeln(
        'FAIL: with asserts off the probe $what '
        '(exit ${release.exitCode}). Its stderr:\n${release.stderr}',
      );
    }
    return failures;
  } finally {
    await probe.delete();
  }
}

/// Run the gateway probe as its own process, once per assert setting.
///
/// The child says which setting it got on its first stderr line, and that is
/// checked rather than assumed: without it, "the release arm refused at the
/// gateway" would also be what a mis-flagged pair of asserts-on runs looked
/// like, and the two arms would stop being two arms.
Future<ProcessResult> _runProbe(
  String packageDir,
  String probePath, {
  required bool asserts,
}) async {
  final r = await Process.run(
    'dart',
    ['run', if (asserts) '--enable-asserts', probePath],
    workingDirectory: packageDir,
    environment: {_childMarker: '1'},
  );
  final want = 'probe: asserts ${asserts ? 'on' : 'off'}';
  if (!'${r.stderr}'.contains(want)) {
    throw _Undecided(
      'the ${asserts ? 'asserts-on' : 'asserts-off'} probe '
      'never reported "$want", so it is not the program this arm is about '
      '(exit ${r.exitCode}). Its stderr:\n${r.stderr}',
    );
  }
  return r;
}

/// One compiled program's retained set, as dart2js reports it.
class _Retained {
  _Retained(this.classes, this._fieldsByParent);

  final Set<String> classes;
  final Map<String, List<String>> _fieldsByParent;

  List<String> fieldsOf(String classId) => [
    ...?_fieldsByParent['class/$classId'],
  ];
}

Future<_Retained> _compile(
  String packageDir,
  Directory tmp,
  String label,
  String source, {
  required bool asserts,
}) async {
  // The probe lives under the package's own `tool/`, because dart2js resolves
  // `package:` through the package config rooted there.
  final probe = File('$packageDir/tool/_handle_bridge_probe_$label.dart');
  await probe.writeAsString(source);
  try {
    final out = '${tmp.path}/$label.js';
    final result = await Process.run('dart', [
      'compile',
      'js',
      '-O2',
      if (asserts) '--enable-asserts',
      '--dump-info=json',
      '-o',
      out,
      probe.path,
    ], workingDirectory: packageDir);
    if (result.exitCode != 0) {
      stderr.writeln(result.stdout);
      stderr.writeln(result.stderr);
      throw _Undecided('dart compile js failed for the $label probe');
    }
    final info = jsonDecode(await File('$out.info.json').readAsString()) as Map;
    final elements = info['elements'];
    if (elements is! Map ||
        elements['class'] is! Map ||
        elements['field'] is! Map) {
      throw _Undecided(
        'the $label probe produced no elements.class '
        'or elements.field map, so nothing here can read a retained set out '
        'of it',
      );
    }
    final classes = {
      for (final e in (elements['class'] as Map).values)
        (e as Map)['name'] as String,
    };
    final fields = <String, List<String>>{};
    for (final e in (elements['field'] as Map).values) {
      final parent = (e as Map)['parent'];
      if (parent is String) {
        (fields[parent] ??= []).add(e['name'] as String);
      }
    }
    return _Retained(classes, fields);
  } finally {
    await probe.delete();
  }
}
