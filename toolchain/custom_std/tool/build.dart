/// Builds a customized wasm32 std and stages it where the Bazel toolchain
/// expects it — dist/, which is gitignored: the automation is checked in, the
/// artifacts are always built locally.
///
/// A *facility* is one std subsystem this builder can change. Two kinds:
///
///   * `atomics` — not a source change at all, but the +atomics RUSTFLAGS
///     (rustflags.txt) that make std's threaded backends available. This is
///     what threaded wasm is.
///   * `clock`, `random`, `stdio`, `thread` — source facilities, each
///     replacing a `wasm32-unknown-unknown` stub with an implementation that
///     asks the host. See tool/patch.dart for what each one edits, and pal/
///     for the code it installs.
///
/// Usage:
///
///     bazel run //toolchain/custom_std:build                             # atomics (the default)
///     bazel run //toolchain/custom_std:build -- --facilities=clock,random
///     bazel run //toolchain/custom_std:build -- --facilities=atomics,clock,random,stdio,thread
///     bazel run //toolchain/custom_std:build -- --list
///
/// `dart toolchain/custom_std/tool/build.dart` from the checkout works too.
/// Either way it needs `rustup` on the PATH.
///
/// What it does:
///   1. installs the pinned nightly (nightly-pin.txt) with rust-src,
///   2. for source facilities, copies rust-src into work/src and patches the
///      copy — never the toolchain's own tree,
///   3. builds an empty stub crate with `-Zbuild-std=std,panic_abort`,
///   4. stages the resulting std rlibs into
///      dist/lib/rustlib/wasm32-unknown-unknown/lib/ (the sysroot layout
///      rustc resolves std from),
///   5. writes dist/manifest.json last — the file bazel/threads.bzl watches,
///      so the next `bazel build` picks the artifacts up with no manual
///      refetch. The manifest records the facilities and the host imports they
///      introduce, so what a module needs served is discoverable from the
///      artifact rather than from this file.
library;

import 'dart:convert';
import 'dart:io';

import 'patch.dart';

/// What the checkout's path is rewritten to in the staged rlibs. Kept the same
/// string as `tools/check_block.dart` and `build_web_fixture.dart` use, so a
/// module and the std it links name the checkout the same way or not at all.
const _remapTo = '/frustrate';

void main(List<String> args) {
  // Symlink-resolved, because the remap below is matched against the working
  // directory the kernel reports (`getcwd`), never against the logical path a
  // `Platform.script` URI carries. Under `bazel run` the script is in a
  // runfiles tree, and the checkout is BUILD_WORKSPACE_DIRECTORY.
  final workspace = Platform.environment['BUILD_WORKSPACE_DIRECTORY'];
  final dir = workspace != null
      ? Directory('$workspace/toolchain/custom_std').resolveSymbolicLinksSync()
      : Directory(File.fromUri(Platform.script).parent.parent.path)
            .resolveSymbolicLinksSync();
  final root = Directory('$dir/../..').resolveSymbolicLinksSync();

  if (args.contains('--list')) {
    stdout.writeln('facilities:');
    stdout.writeln(
      '  atomics      +atomics/shared-memory build (rustflags.txt)',
    );
    for (final f in allFacilities) {
      stdout.writeln('  ${f.name.padRight(12)} ${f.summary}');
    }
    return;
  }

  final selected = _parseFacilities(args);
  final atomics = selected.remove('atomics');
  final source = selected.map(facilityByName).toList();

  // Everything below is keyed by flavour, including the cargo target dir.
  // Sharing one target dir across flavours is not a cache win, it is a
  // correctness bug: `deps/` accumulates rlibs from every flavour ever built
  // there and the staging glob cannot tell them apart, so an atomics build
  // silently stages the previous non-atomics std alongside its own. Measured:
  // 48 rlibs where 24 were expected.
  final facilities = [if (atomics) 'atomics', ...source.map((f) => f.name)];
  final key = flavourKey(facilities);
  final work = '$dir/stub/work/$key';

  final pin = File('$dir/nightly-pin.txt').readAsStringSync().trim();
  // One argv token per element. rustflags.txt is written one *flag* per line
  // ('-C target-feature=...' is two tokens), and the manifest is consumed as
  // a rust_toolchain's extra_rustc_flags, which are not shell-split: an
  // untokenized '-C target-feature=+atomics' arrives as a single argument and
  // rustc reads it as `-C` with the value ` target-feature=+atomics`, failing
  // with "unknown codegen option". Splitting here keeps the env var below
  // (which joins them again) and the manifest both correct.
  final rustflags = atomics
      ? File('$dir/rustflags.txt')
            .readAsLinesSync()
            .map((l) => l.trim())
            .where((l) => l.isNotEmpty && !l.startsWith('#'))
            .expand((l) => l.split(' '))
            .toList()
      : <String>[];

  // What rustc is actually invoked with. `CARGO_ENCODED_RUSTFLAGS`, not
  // `RUSTFLAGS`: `--remap-path-prefix` carries the checkout's absolute path,
  // and cargo word-splits `RUSTFLAGS`, so a checkout under `~/My Projects/`
  // would tear it in half. `\x1f`-separated and never split. See
  // tools/check_block.dart for the longer version.
  //
  // The remap is what keeps the *staged std* out of the reproducibility
  // problem, and it is not optional here even though the app-side builds have
  // their own. A source facility compiles std from a patched copy that lives
  // under this checkout (`stub/work/<flavour>/src/library`), so without it
  // every `panic::Location` in the staged rlibs — and therefore in every Bazel
  // module that links them — names this directory. Measured before the remap:
  // 808 hits in the `clock-random-stdio-thread` rlibs.
  //
  // Deliberately *not* part of `rustflags`: that list is written into
  // manifest.json and becomes the toolchain's `extra_rustc_flags` for app
  // crates under Bazel, where sources are already relative to the execroot.
  // Replaying a machine-absolute path through Bazel would put back exactly the
  // kind of dependence this removes.
  final encoded = [
    ...rustflags,
    '--remap-path-prefix',
    '$root=$_remapTo',
  ].join('\x1f');

  _requireFreshWorkDir(work, pin, encoded);

  stdout.writeln(
    'facilities: '
    '${[if (atomics) 'atomics', ...source.map((f) => f.name)].join(', ')}',
  );

  _run('rustup', [
    'toolchain',
    'install',
    pin,
    '--profile',
    'minimal',
    '--component',
    'rust-src',
  ]);

  // Source facilities need a writable copy of rust-src. Cargo takes the
  // location from an internal env var; it is unstable and can be removed
  // without notice, which is why the failure it produces has to stay loud and
  // why this line says so out loud rather than hiding in a flag list.
  String? srcRoot;
  if (source.isNotEmpty) {
    final sysroot = _capture('rustup', [
      'run',
      pin,
      'rustc',
      '--print',
      'sysroot',
    ]);
    final upstream = Directory('$sysroot/lib/rustlib/src/rust/library');
    if (!upstream.existsSync()) {
      stderr.writeln('custom_std: rust-src is missing at ${upstream.path}');
      exit(1);
    }
    final srcDir = Directory('$work/src/library');
    if (srcDir.parent.existsSync()) srcDir.parent.deleteSync(recursive: true);
    srcDir.parent.createSync(recursive: true);
    stdout.writeln('copying rust-src -> ${srcDir.path}');
    _run('cp', ['-R', upstream.path, srcDir.path]);

    stdout.writeln(
      'patching ${source.length} '
      'facilit${source.length == 1 ? 'y' : 'ies'} into the copy',
    );
    try {
      applyFacilities(srcDir, Directory('$dir/pal'), source);
    } on PatchError catch (e) {
      stderr.writeln(e);
      exit(1);
    }
    srcRoot = srcDir.path;
    stdout.writeln(
      'NOTE: patched-std builds set __CARGO_TESTS_ONLY_SRC_ROOT, '
      'an internal cargo\n      variable. If a toolchain bump removes it, '
      'this build fails loudly here\n      rather than producing a std with '
      'the stubs still in it.',
    );
  }

  // panic_abort matches the runtime's trap-based panic story; release because
  // a debug std is not what anything measures against.
  _run(
    'cargo',
    [
      '+$pin',
      'build',
      '--release',
      '--target',
      'wasm32-unknown-unknown',
      '-Zbuild-std=std,panic_abort',
    ],
    workingDirectory: '$dir/stub',
    environment: {
      'CARGO_ENCODED_RUSTFLAGS': encoded,
      'CARGO_TARGET_DIR': work,
      if (srcRoot != null) '__CARGO_TESTS_ONLY_SRC_ROOT': srcRoot,
    },
  );

  // Harvest by *searching* the target dir, not by naming a subdirectory of it.
  //
  // cargo has moved this: `-Zbuild-std` output used to land in
  // `<target>/release/deps/` and now lands in
  // `<target>/release/build/<crate>/<hash>/out/`. Naming the old path did not
  // fail when that changed — the previous build's `deps/` was still sitting
  // there, so the builder harvested a whole std from the *previous compiler*,
  // staged it, and wrote a manifest naming the new pin. Nothing downstream
  // could see the mismatch; rustc eventually refused the rlibs with E0514,
  // in an unrelated crate, with no path back to this function.
  //
  // A recursive search has no layout to go stale. `_requireOneOfEachCrate`
  // below is what keeps it honest: if a future cargo writes a crate to two
  // places, or a stale tree survives a wipe, that check fails rather than
  // letting this one pick arbitrarily.
  final targetDir = Directory('$work/wasm32-unknown-unknown/release');
  if (!targetDir.existsSync()) {
    stderr.writeln('cargo produced no ${targetDir.path}; nothing was built');
    exit(1);
  }
  final rlibs =
      targetDir
          .listSync(recursive: true, followLinks: false)
          .whereType<File>()
          .where((f) => f.path.endsWith('.rlib'))
          .where(
            (f) => !f.uri.pathSegments.last.startsWith('libcustom_std_stub'),
          )
          .toList()
        ..sort((a, b) => a.path.compareTo(b.path));
  if (rlibs.isEmpty) {
    stderr.writeln('no std rlibs found anywhere under ${targetDir.path}');
    exit(1);
  }
  _requireOneOfEachCrate(rlibs);
  // Both controls run before the staging directory is touched, so a build that
  // fails one of them leaves the previously staged flavour intact rather than
  // half-replaced.
  _requireRemapped(rlibs, root);

  // One directory per facility set, named by `flavourKey`. Only this flavour
  // is wiped: another flavour staged earlier stays valid, so a repo with both
  // a threaded and a browser-facility platform does not need the builder rerun
  // every time the other one is built.
  final flavour = Directory('$dir/dist/$key');
  final lib = Directory(
    '${flavour.path}/lib/rustlib/wasm32-unknown-unknown/lib',
  );
  if (flavour.existsSync()) flavour.deleteSync(recursive: true);
  lib.createSync(recursive: true);
  for (final rlib in rlibs) {
    rlib.copySync('${lib.path}/${rlib.uri.pathSegments.last}');
  }

  final imports = [for (final f in source) ...f.imports]..sort();

  // Written last: bazel/custom_std.bzl watches this file, and its appearance
  // (or content change) is what tells Bazel the staging is complete.
  // The compiler that actually ran, read back from it rather than assumed.
  // `nightly` below is the *pin* — what the build was asked for — and on its
  // own it cannot witness what compiled these rlibs: a stale target dir
  // produced a manifest claiming a toolchain that had touched nothing. The two
  // fields disagreeing is the symptom to look for, and `rustc` is the one of
  // them that is evidence.
  final rustcVv = Process.runSync('rustc', ['+$pin', '-vV']).stdout as String;
  final rustcVersion = rustcVv
      .split('\n')
      .firstWhere((l) => l.startsWith('release: '), orElse: () => 'release: ?')
      .substring('release: '.length)
      .trim();

  File('${flavour.path}/manifest.json').writeAsStringSync(
    const JsonEncoder.withIndent('  ').convert({
      'nightly': pin,
      'rustc': rustcVersion,
      // The flags the *app* crates must be compiled with to match this std. The
      // toolchain reads them from here rather than from rustflags.txt, so the
      // std's flags and its consumers' flags have one source: a std staged
      // without atomics can no longer be handed to a toolchain that adds them.
      'rustflags': rustflags,
      'facilities': facilities..sort(),
      // What a module built against this std will import from the host. The
      // glue has to serve every one of these or instantiation fails.
      'requires_imports': imports,
      'files': [for (final f in rlibs) f.uri.pathSegments.last],
    }),
  );

  stdout.writeln('staged ${rlibs.length} rlibs into ${lib.path}');
  stdout.writeln('flavour: $key');
  if (imports.isNotEmpty) {
    stdout.writeln('the host must serve: ${imports.join(', ')}');
  }
  final others =
      Directory('$dir/dist')
          .listSync()
          .whereType<Directory>()
          .map((d) => d.uri.pathSegments[d.uri.pathSegments.length - 2])
          .where((n) => n != key)
          .toList()
        ..sort();
  if (others.isNotEmpty) {
    stdout.writeln('also staged: ${others.join(', ')}');
  }
}

/// Wipe [work] unless it was built with exactly [encoded] as its rustflags.
///
/// The target dir is keyed by *flavour*, which the comment at the top of `main`
/// explains: sharing one across flavours stages two stds at once, because
/// `deps/` keeps every rlib ever built there and the staging glob cannot tell
/// them apart. Flags are the same hazard by a different route. Changing them
/// changes every crate's `-Cmetadata` hash, so the new rlibs land *beside* the
/// old ones under new names instead of replacing them, and the glob stages
/// both. Measured when `--remap-path-prefix` was introduced: 48 rlibs where 24
/// were expected, two of every crate, with the pre-remap copy of each still
/// naming the checkout.
///
/// So the flags are part of the cache's identity and are recorded as such.
/// Wiping is a full std rebuild, which is minutes — paid only when the flags
/// actually change, and the alternative is a std that is quietly half stale.
/// Wipe the cargo target dir unless it was built with *exactly* this
/// toolchain and these flags.
///
/// **The toolchain half is load-bearing and was missing.** cargo does not
/// reliably invalidate `-Zbuild-std` units when rustc changes underneath them:
/// with a bumped pin and unchanged flags it reports `Finished` in
/// milliseconds and leaves every std rlib from the previous compiler in place.
/// The builder then stages those, writes a manifest naming the *new* pin, and
/// nothing downstream can tell — until rustc refuses the mismatched rlibs with
/// E0514, arbitrarily far away, in whichever crate happens to compile first.
///
/// So the stamp carries the pin as well as the flags, and a bump wipes the
/// tree. Verified by doing it: bumping the pin alone left a
/// 1.93.0-built `libcore` staged under a manifest that said the build was
/// 1.99.0.
void _requireFreshWorkDir(String work, String pin, String encoded) {
  final stamp = File('$work/.stamp');
  final want = '$pin\n$encoded';
  if (stamp.existsSync() && stamp.readAsStringSync() == want) return;
  final d = Directory(work);
  if (d.existsSync()) {
    stdout.writeln(
      'toolchain or rustflags changed since this flavour was '
      'last built; rebuilding it from scratch',
    );
    d.deleteSync(recursive: true);
  }
  d.createSync(recursive: true);
  stamp.writeAsStringSync(want);
}

/// Control: one rlib per crate, so the staged sysroot is one std.
///
/// The glob that fills [rlibs] takes whatever is in `deps/`, and rustc resolves
/// a crate from a sysroot by name: two `libstd-*.rlib` there is not a sysroot
/// with a spare, it is a sysroot whose `std` depends on which one rustc picks.
/// `_requireFreshWorkDir` is what should keep this from happening; this is the
/// check that it did, because the symptom otherwise shows up as a link error in
/// a downstream Bazel build with nothing pointing back to here.
void _requireOneOfEachCrate(List<File> rlibs) {
  final byCrate = <String, List<String>>{};
  for (final rlib in rlibs) {
    final file = rlib.uri.pathSegments.last;
    // `lib<crate>-<16 hex>.rlib`; the hash is the part that differs.
    final crate = file.replaceFirst(RegExp(r'-[0-9a-f]+\.rlib$'), '');
    byCrate.putIfAbsent(crate, () => []).add(file);
  }
  final dupes = byCrate.entries.where((e) => e.value.length > 1).toList();
  if (dupes.isEmpty) return;
  stderr.writeln(
    'custom_std: ${dupes.length} crate'
    '${dupes.length == 1 ? ' has' : 's have'} more than one rlib in the '
    'build directory, so the\nstaged sysroot would not be one std:\n',
  );
  for (final e in dupes.take(5)) {
    stderr.writeln('  ${e.key}: ${e.value.join(', ')}');
  }
  stderr.writeln('\nThis is a stale build directory. Remove it and rerun.');
  exit(1);
}

/// Control: the staged std does not name the directory it was built in.
///
/// The remap on the cargo call above is what removes it; this is the check that
/// it applied. `--remap-path-prefix` does nothing when its prefix does not
/// match the path rustc sees, and says nothing when it does nothing — so a
/// staging run that reports success is no evidence at all. Get the prefix
/// wrong (a symlinked checkout; `/tmp` against its real `/private/tmp`) and
/// this flavour is a different set of bytes in every checkout, and so is every
/// Bazel module that links it — which is a much harder thing to notice from
/// the other end.
///
/// Scanned as raw bytes: an rlib is an `ar` archive of object files, and this
/// only needs to know whether the path is in one at all. The needle is UTF-8
/// encoded rather than taken as code units, so the check still fires for a
/// checkout path with a non-ASCII character in it — a guard that cannot fail is
/// worse than no guard, because it reads as evidence.
void _requireRemapped(List<File> rlibs, String root) {
  final needle = utf8.encode(root);
  final guilty = <String>[];
  for (final rlib in rlibs) {
    final bytes = rlib.readAsBytesSync();
    if (_indexOfBytes(bytes, needle) >= 0) {
      guilty.add(rlib.uri.pathSegments.last);
    }
  }
  if (guilty.isEmpty) return;
  stderr.writeln(
    'custom_std: ${guilty.length} staged rlib'
    '${guilty.length == 1 ? '' : 's'} still embed this checkout\'s path.\n\n'
    '  $root\n\n'
    'First: ${guilty.first}\n\n'
    'The build was asked to remap it to $_remapTo, and `--remap-path-prefix` '
    'is a silent\nno-op when its prefix is not a prefix of the path rustc '
    'sees. So the usual cause\nis that the two spell the same directory '
    'differently — a symlinked checkout, or\n/tmp against its real name '
    '/private/tmp.',
  );
  exit(1);
}

/// First index of [needle] in [haystack], or -1. Plain search; the inputs are a
/// few MB and this runs once per staged rlib.
int _indexOfBytes(List<int> haystack, List<int> needle) {
  if (needle.isEmpty || needle.length > haystack.length) return -1;
  final last = haystack.length - needle.length;
  final first = needle[0];
  outer:
  for (var i = 0; i <= last; i++) {
    if (haystack[i] != first) continue;
    for (var j = 1; j < needle.length; j++) {
      if (haystack[i + j] != needle[j]) continue outer;
    }
    return i;
  }
  return -1;
}

/// `--facilities=a,b,c`; defaults to `atomics`, which is what this builder did
/// before it could do anything else.
Set<String> _parseFacilities(List<String> args) {
  const flag = '--facilities=';
  final arg = args.where((a) => a.startsWith(flag)).lastOrNull;
  if (arg == null) return {'atomics'};
  final names = arg
      .substring(flag.length)
      .split(',')
      .map((s) => s.trim())
      .where((s) => s.isNotEmpty)
      .toSet();
  final known = {'atomics', ...allFacilities.map((f) => f.name)};
  for (final n in names) {
    if (!known.contains(n)) {
      stderr.writeln(
        'custom_std: unknown facility `$n`; '
        'known: ${known.join(', ')}',
      );
      exit(2);
    }
  }
  if (names.isEmpty) {
    stderr.writeln(
      'custom_std: --facilities= was empty; '
      'pass at least one of ${known.join(', ')}',
    );
    exit(2);
  }
  return names;
}

String _capture(String cmd, List<String> args) {
  final r = Process.runSync(cmd, args);
  if (r.exitCode != 0) {
    stderr.write(r.stderr);
    exit(r.exitCode);
  }
  return (r.stdout as String).trim();
}

void _run(
  String cmd,
  List<String> args, {
  String? workingDirectory,
  Map<String, String>? environment,
}) {
  stdout.writeln('\$ $cmd ${args.join(' ')}');
  final r = Process.runSync(
    cmd,
    args,
    workingDirectory: workingDirectory,
    environment: environment,
  );
  if (r.exitCode != 0) {
    stderr.write(r.stdout);
    stderr.write(r.stderr);
    exit(r.exitCode);
  }
}
