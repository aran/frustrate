/// Builds the bridge crate for wasm32 and stages the module where the
/// browser test server can serve it (build/test_api.wasm).
///
/// Both builds stage to the same path; the suite adapts at runtime
/// (FrustrateRuntime.asyncIsParallel):
///
/// - default: single-threaded web — stable Rust, single-threaded instance.
/// - `--threaded`: threaded wasm — nightly -Zbuild-std with +atomics and an
///   imported shared memory. Running this build
///   under `dart test -p chrome` needs SharedArrayBuffer enabled without
///   cross-origin isolation; dart_test.yaml passes the Chrome flag.
///
/// `--release` builds the bridge module with stock `cargo --release` instead of
/// the default debug profile, and stages from `target/wasm32-unknown-unknown/
/// release/`. It is the only way to get an optimized bridge module out of
/// this script — the debug default is `-Copt-level=0`.
///
/// It changes only the **Rust half**. The Dart half of a `dart test -p chrome
/// -c dart2wasm` run stays unoptimized no matter what: the pub `test` package
/// hardcodes `-O0` and `--enable-asserts` for dart2wasm
/// (test_core/lib/src/runner/wasm_compiler_pool.dart) with no arg plumbing to
/// override it. So `--release` here is for measuring *module size* and for
/// running the suite against optimized Rust — never quote a web timing from
/// this harness as a release number.
///
/// Codegen enforces capabilities structurally: native-only members
/// (`on_contention = "block"`) are absent from the web Dart surface and their
/// dispatch arms are cfg-gated out of this wasm artifact, under both web
/// builds.
///
/// Run from the package directory: `dart run tool/build_web_fixture.dart`,
/// then `dart test -p chrome -c dart2wasm`.
library;

import 'dart:io';

/// The staged flavour this script builds against for `--facilities`. Must be
/// the directory `toolchain/custom_std/tool/build.dart` writes for that
/// facility set -- the key is the names sorted and joined by `-`.
const _facilityFlavour = 'clock-random-stdio-thread';

/// What the checkout's path is rewritten to in the staged module.
///
/// Cargo hands rustc relative source paths, so most of the module's
/// `panic::Location` strings are already `runtime/rust/src/…`; the ones that
/// are not come from `frustrate` bodies monomorphized inside `test_api`, whose
/// spans are reconstructed against the absolute `working_dir` recorded in
/// `frustrate`'s crate metadata. Without the remap the staged fixture is a
/// different file in every checkout, which costs this repo the ability to
/// answer "did this change contribute bytes?" by diffing artifacts.
///
/// A fixed stem rather than an empty one, because a web panic's only
/// attribution is the `file:line` the `frustrate_web_init` hook ships:
/// `panicked at /frustrate/runtime/rust/src/spin.rs:42:5` names the file as
/// precisely as the absolute path did. `tools/check_block.dart` carries the
/// full argument, and a control that fails when the remap stops applying.
const _remapTo = '/frustrate';

/// Link flags for threaded wasm. --max-memory must stay in sync with the
/// runtime's _sharedMemoryMaxPages (16384 pages = 1GiB).
///
/// The same list as `toolchain/custom_std/rustflags.txt`, which is the Bazel
/// flow's, and kept the same on purpose: "works under cargo, fails under Bazel"
/// must never be a flag skew. `__tls_base` is only ever read by wasm-bindgen's
/// thread transform, which this fixture's graph never invokes — it is here
/// because a divergence between the two lists is the thing worth preventing,
/// not because this build needs it.
const _threadedLinkFlags = [
  '-C',
  'target-feature=+atomics,+bulk-memory,+mutable-globals',
  '-C',
  'link-arg=--import-memory',
  '-C',
  'link-arg=--shared-memory',
  '-C',
  'link-arg=--max-memory=1073741824',
  '-C',
  'link-arg=--export=__stack_pointer',
  '-C',
  'link-arg=--export=__wasm_init_tls',
  '-C',
  'link-arg=--export=__tls_size',
  '-C',
  'link-arg=--export=__tls_align',
  '-C',
  'link-arg=--export=__tls_base',
];

void main(List<String> args) {
  final threaded = args.contains('--threaded');
  final facilities = args.contains('--facilities');
  final release = args.contains('--release');
  if (threaded && facilities) {
    // Not a fundamental conflict -- an `atomics,clock,...` flavour is a
    // legitimate thing to build -- but this script stages both flavours to one
    // path, so it can only describe one at a time. Refused rather than
    // silently preferring one, because the suite reads the *value* of the std
    // probes to decide what it is running against.
    stderr.writeln(
      'build_web_fixture: --threaded and --facilities together '
      'are not supported here. Build one flavour at a time.',
    );
    exit(2);
  }
  final profile = release ? 'release' : 'debug';
  final packageDir = File.fromUri(Platform.script).parent.parent.path;
  final workspace = Directory('$packageDir/../..').resolveSymbolicLinksSync();

  // The same pinned nightly the Bazel toolchain uses (bazel/threads.bzl):
  // "works under cargo, fails under Bazel" must never be a version skew.
  // toolchain/custom_std/tool/build.dart installs it.
  final pin = File('$workspace/toolchain/custom_std/nightly-pin.txt')
      .readAsStringSync()
      .trim();

  // The facility flavour links a std that toolchain/custom_std staged, by
  // pointing rustc at it as a sysroot. Not -Zbuild-std: that would rebuild a
  // *stock* std from the toolchain's own rust-src and quietly undo every
  // patch, which is the one failure that would look like success here.
  final flavourDir = '$workspace/toolchain/custom_std/dist/$_facilityFlavour';
  if (facilities && !File('$flavourDir/manifest.json').existsSync()) {
    stderr.writeln(
      'build_web_fixture: the facility std has not been staged.\n'
      'Run: bazel run //toolchain/custom_std:build -- '
      '--facilities=${_facilityFlavour.replaceAll('-', ',')}\n'
      'Looked in: $flavourDir',
    );
    exit(1);
  }

  final build = Process.runSync(
    'cargo',
    [
      if (threaded || facilities) '+$pin',
      'build',
      '-p',
      'test_api',
      if (release) '--release',
      if (threaded) ...['--features', 'wasm-threads'],
      if (facilities) ...['--features', 'wasm-std-facilities'],
      '--target',
      'wasm32-unknown-unknown',
      if (threaded) '-Zbuild-std=std,panic_abort',
    ],
    workingDirectory: workspace,
    // `CARGO_ENCODED_RUSTFLAGS`, not `RUSTFLAGS`: cargo word-splits the latter,
    // and two of the flags below carry a path. A checkout under `~/My
    // Projects/` would have `--sysroot` torn in half and the build would
    // *succeed* against the wrong std — see tools/check_block.dart, which
    // names this file as the reason it uses the encoded form. `\x1f`-separated
    // and never split, so the argument vector is what it says it is.
    environment: {
      'CARGO_ENCODED_RUSTFLAGS': [
        '--remap-path-prefix',
        '$workspace=$_remapTo',
        if (threaded) ..._threadedLinkFlags,
        if (facilities) ...['--sysroot', flavourDir],
      ].join('\x1f'),
    },
  );
  if (build.exitCode != 0) {
    stderr.write(build.stdout);
    stderr.write(build.stderr);
    exit(build.exitCode);
  }

  // Stage from the profile we just asked cargo for. Never fall back to the
  // other profile: a debug module silently standing in for a release one makes
  // every size and timing number downstream a lie.
  final wasm = File(
    '$workspace/target/wasm32-unknown-unknown/$profile/test_api.wasm',
  );
  if (!wasm.existsSync()) {
    stderr.writeln(
      'build_web_fixture: cargo reported success but '
      '${wasm.path} does not exist.\n'
      'Expected it from: cargo build -p test_api '
      '${release ? '--release ' : ''}--target wasm32-unknown-unknown\n'
      'Is CARGO_TARGET_DIR or build.target-dir redirecting the output?',
    );
    exit(1);
  }
  final staged = File('$packageDir/build/test_api.wasm')
    ..parent.createSync(recursive: true);
  wasm.copySync(staged.path);

  // The served-glue asset, for the strict-CSP delivery path
  // (csp_glue_test.dart creates workers from this URL instead of blob:).
  File('$workspace/runtime/dart/lib/src/js/frustrate.js')
      .copySync('$packageDir/build/frustrate.js');

  stdout.writeln(
    'staged ${staged.path} (${wasm.lengthSync()} bytes, $profile, '
    '${threaded ? 'threaded' : 'single-threaded'})',
  );
}
