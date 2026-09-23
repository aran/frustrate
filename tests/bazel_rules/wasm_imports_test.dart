/// Every import a built wasm module declares is one the host actually supplies.
///
/// **Why this exists.** A wasm module's import list is not something anyone
/// writes down — rustc emits an import for each std path the crate's code
/// happens to reach, so it is a property of the *whole dependency graph*. A
/// dependency bump can add one without a single line of app code changing. The
/// failure that follows is late and remote: on the wasi platform frustrate's JS
/// host is a `Proxy` whose unimplemented calls throw, so a newly reached
/// preview1 call instantiates fine and traps at some later moment inside a
/// browser, attributed to whatever called it.
///
/// This turns that into a red build_test at the moment the import appears.
///
/// **The allowlist is not written here.** It is *read from the shim* —
/// `runtime/dart/lib/src/js/frustrate.js`, the same file the runtime serves —
/// so the two cannot drift. Adding an implementation to the shim widens what
/// modules may import, and deleting one narrows it, with no second list to
/// update. A hand-copied list of eight names would have been wrong the day it
/// was written: this fixture reaches seven preview1 calls and frustrate's own
/// `test_api` crate reaches eight.
///
/// **Both directions matter.** The default (`wasm32-unknown-unknown`) fixture
/// is asserted to import *no* wasi at all. Without that half, a change that
/// silently pointed the default platform at wasip1 — or a crate that pulled in
/// a wasi shim of its own — would pass unnoticed.
///
/// Scope: the two fixtures in the `bazel test //...` wildcard. The threaded and
/// wasm-bindgen paths are not covered here — the threaded fixture is `manual`
/// (it needs a locally built +atomics std) and taking it as `data` would drag
/// it into the wildcard, which is exactly what its tag prevents. The
/// wasm-bindgen path lives in `e2e/iroh_demo`, a separate Bazel module.
@TestOn('vm')
library;

import 'dart:io';
import 'dart:typed_data';

import 'package:runfiles/runfiles.dart';
import 'package:test/test.dart';

/// frustrate's own ABI — the complete set, from the four
/// `#[link(wasm_import_module = "frustrate")]` blocks in `runtime/rust/src`.
/// `spawn_worker` appears only under the threaded platform.
///
/// This is the *bridge* ABI. A module built against a custom std imports more
/// from the same namespace — see [_stdFacilities], which is read out of the
/// glue rather than restated here for the same reason [_shimImplements] is.
const Set<String> _frustrateAbi = {
  'panic',
  'post',
  'schedule_drain',
  'spawn_worker',
};

/// The host imports every facility in `toolchain/custom_std/tool/patch.dart`
/// declares, with the `frustrate.` namespace prefix stripped.
///
/// The facilities are the authority on what a std built from them will ask for
/// — it is the same list that lands in `dist/<key>/manifest.json` as
/// `requires_imports`. Reading it here is what makes "the glue serves what the
/// std asks for" checkable without building either.
Set<String> _facilityImports(String dart) =>
    RegExp(r"'frustrate\.([a-z_0-9]+)'")
        .allMatches(dart)
        .map((m) => m.group(1)!)
        .toSet();

/// The std-facility imports the glue serves, parsed out of the
/// `stdFacilityImports` factory in `frustrate.js`.
///
/// Read rather than restated, exactly like [_shimImplements]: these names are a
/// contract between a std built by `toolchain/custom_std` and the host, and a
/// list maintained by hand here would drift the first time a facility is added
/// — silently, because a drifted allowlist only ever makes this test *more*
/// permissive.
Set<String> _stdFacilities(String js) {
  final open = js.indexOf('const stdFacilityImports = (getMemory) => {');
  expect(
    open,
    isNot(-1),
    reason:
        'frustrate.js no longer has a `stdFacilityImports` factory; this '
        'test reads its members as the custom-std allowlist',
  );
  final close = js.indexOf('\n    };', open);
  expect(close, isNot(-1), reason: 'unterminated `stdFacilityImports` factory');
  final body = js.substring(open, close);
  final names = RegExp(
    r'^      ([a-z_0-9]+)\s*[(:]',
    multiLine: true,
  ).allMatches(body).map((m) => m.group(1)!).toSet();
  expect(
    names,
    isNotEmpty,
    reason: 'parsed no members out of `stdFacilityImports`',
  );
  return names;
}

/// Non-wasi, non-frustrate imports a module may legitimately declare.
///
/// `env.__stack_chk_fail` is emitted by the C half of a crate graph built with
/// stack protectors on; `runtime_web.dart` supplies it.
const Set<String> _envAllowed = {'__stack_chk_fail'};

String _rlocation(String path) =>
    Platform.environment.containsKey('TEST_SRCDIR')
    ? Runfiles.create().rlocation('_main/$path')
    : path;

/// (module, name) for every entry in the module's import section.
///
/// A hand-rolled parser rather than a package: the import section is the second
/// section of the binary and needs only LEB128 plus four import-kind shapes, so
/// this is cheaper than a dependency and cannot go stale against a format that
/// has been frozen since MVP.
List<({String module, String name})> _imports(Uint8List d) {
  expect(d.sublist(0, 4), [
    0x00,
    0x61,
    0x73,
    0x6d,
  ], reason: 'not a wasm module');
  var i = 8; // magic + version
  final out = <({String module, String name})>[];

  int uleb() {
    var result = 0, shift = 0;
    while (true) {
      final b = d[i++];
      result |= (b & 0x7f) << shift;
      shift += 7;
      if (b & 0x80 == 0) return result;
    }
  }

  String str() {
    final len = uleb();
    final s = String.fromCharCodes(d.sublist(i, i + len));
    i += len;
    return s;
  }

  while (i < d.length) {
    final sectionId = d[i++];
    final size = uleb();
    final end = i + size;
    if (sectionId == 2) {
      for (var n = uleb(); n > 0; n--) {
        final module = str();
        final name = str();
        switch (d[i++]) {
          case 0: // func: typeidx
            uleb();
          case 1: // table: reftype + limits
            i++;
            final hasMax = d[i++] != 0;
            uleb();
            if (hasMax) uleb();
          case 2: // memory: limits (bit 0 = max, bit 1 = shared)
            final flags = d[i++];
            uleb();
            if (flags & 0x01 != 0) uleb();
          case 3: // global: valtype + mutability
            i += 2;
          default:
            fail('unknown import kind in $module.$name');
        }
        out.add((module: module, name: name));
      }
    }
    i = end;
  }
  return out;
}

/// The preview1 calls the shim supplies, read out of its `const impl = {…}`
/// object. Members are written two ways — `name(args) {` for a real
/// implementation and `name: unsupported(…)` for one that throws — and both
/// count as supplied: a module may *import* `proc_exit` (std's abort path links
/// it) and be correct, because the throw only happens if it is called.
Set<String> _shimImplements(String js) {
  final open = js.indexOf('const impl = {');
  expect(
    open,
    isNot(-1),
    reason:
        'the wasip1 host in frustrate.js no longer has a `const impl` '
        'object; this test reads its members as the allowlist',
  );
  final close = js.indexOf('\n    };', open);
  expect(close, isNot(-1), reason: 'unterminated `const impl` object');
  final body = js.substring(open, close);
  final names = RegExp(
    r'^      ([a-z_0-9]+)\s*[(:]',
    multiLine: true,
  ).allMatches(body).map((m) => m.group(1)!).toSet();
  expect(names, isNotEmpty, reason: 'parsed no members out of `const impl`');
  return names;
}

void main() {
  late Set<String> implemented;
  late Set<String> facilities;
  setUpAll(() {
    final js = File(_rlocation('runtime/dart/lib/src/js/frustrate.js'))
        .readAsStringSync();
    implemented = _shimImplements(js);
    facilities = _stdFacilities(js);
  });

  List<({String module, String name})> importsOf(String target) =>
      _imports(File(_rlocation('tests/bazel_rules/$target')).readAsBytesSync());

  group('the default platform (wasm32-unknown-unknown)', () {
    late List<({String module, String name})> imports;
    setUpAll(() => imports = importsOf('fixture.wasm'));

    test('imports nothing but frustrate\'s own ABI', () {
      expect(imports.map((e) => e.module).toSet(), {'frustrate'});
      expect(imports.map((e) => e.name).toSet(), isNotEmpty);
      expect(
        imports.map((e) => e.name).toSet().difference(_frustrateAbi),
        isEmpty,
        reason:
            'an import in the `frustrate` namespace that is not one of '
            'the four #[link(wasm_import_module = "frustrate")] functions',
      );
    });

    test('imports no wasi', () {
      // The negative half. On this platform std links sys/pal/unsupported/*,
      // so a wasi import here means the platform moved under us — or a
      // dependency brought its own preview1 shim, which the runtime would not
      // be feeding.
      expect(
        imports.where((e) => e.module == 'wasi_snapshot_preview1'),
        isEmpty,
        reason:
            'wasm32-unknown-unknown must not reach wasi; if this crate '
            'now needs a working std, it belongs on //bazel:wasm32_wasi',
      );
    });
  });

  // The custom-std fixture itself cannot be tested here: it needs a locally
  // built std that is never checked in, so taking it as `data` would drag a
  // manual target into the wildcard — the same reason the threaded fixture is
  // excluded (see this file's scope note). What *is* checkable without any
  // artifact is the contract the module depends on: every import a facility
  // declares must be one the glue serves. That is the drift this test can
  // catch cheaply, and the one that would otherwise surface as a LinkError in
  // a browser.
  group('the custom-std facilities', () {
    test('every import a facility declares is served by the glue', () {
      final declared = _facilityImports(
        File(_rlocation('toolchain/custom_std/tool/patch.dart'))
            .readAsStringSync(),
      );
      expect(
        declared,
        isNotEmpty,
        reason: 'parsed no `imports:` entries out of patch.dart',
      );
      expect(
        declared.difference(facilities),
        isEmpty,
        reason:
            'a facility in toolchain/custom_std declares a host import '
            'that frustrate.js does not serve. A module built against that '
            'std fails to instantiate — LinkError, in the browser, naming '
            'the import. Add it to stdFacilityImports.',
      );
    });

    test('the glue serves nothing no facility asks for', () {
      final declared = _facilityImports(
        File(_rlocation('toolchain/custom_std/tool/patch.dart'))
            .readAsStringSync(),
      );
      expect(
        facilities.difference(declared),
        isEmpty,
        reason:
            'frustrate.js serves a std facility import that no facility '
            'declares. Either a facility lost its `imports:` entry — in '
            'which case the manifest now under-reports what a module needs '
            '— or the glue has a member nothing will ever call.',
      );
    });

    test('facility imports do not collide with the bridge ABI', () {
      expect(
        facilities.intersection(_frustrateAbi),
        isEmpty,
        reason:
            'both sets live in the `frustrate` import namespace, so a '
            'shared name would have one silently shadow the other',
      );
    });
  });

  group('the wasip1 platform', () {
    late List<({String module, String name})> imports;
    setUpAll(() => imports = importsOf('fixture_wasi.wasm'));

    test('imports only namespaces the runtime supplies', () {
      expect(
        imports.map((e) => e.module).toSet(),
        everyElement(
          isIn(const ['frustrate', 'wasi_snapshot_preview1', 'env']),
        ),
      );
      expect(
        imports
            .where((e) => e.module == 'env')
            .map((e) => e.name)
            .toSet()
            .difference(_envAllowed),
        isEmpty,
      );
      expect(
        imports
            .where((e) => e.module == 'frustrate')
            .map((e) => e.name)
            .toSet()
            .difference(_frustrateAbi),
        isEmpty,
      );
    });

    test('every preview1 call it needs is one the host implements', () {
      final needed = imports
          .where((e) => e.module == 'wasi_snapshot_preview1')
          .map((e) => e.name)
          .toSet();
      expect(
        needed,
        isNotEmpty,
        reason:
            'the wasi fixture reaches no preview1 call at all, so this '
            'platform is no longer being exercised',
      );
      expect(
        needed.difference(implemented),
        isEmpty,
        reason:
            'this module imports a wasi_snapshot_preview1 function that '
            'runtime/dart/lib/src/js/frustrate.js does not supply. The '
            'runtime would instantiate it and throw when it ran. Either '
            'implement it in the shim (and in the glue_source.dart mirror), '
            'or stop reaching the std path that needs it. '
            'Implemented: ${(implemented.toList()..sort()).join(', ')}',
      );
    });
  });
}
