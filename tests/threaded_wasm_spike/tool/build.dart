// Builds the threaded wasm spike module: nightly -Zbuild-std with +atomics, and
// copies the result to module.wasm next to the JS glue.
//
//   dart tool/build.dart
import 'dart:convert';
import 'dart:io';

/// What the checkout's path is rewritten to, so the module does not depend on
/// where the checkout lives.
///
/// This is prophylactic rather than a fix: the spike is its own cargo
/// workspace whose only source is its own `src/`, so every path cargo hands
/// rustc is already relative and the remap currently matches nothing —
/// measured, two checkouts 29 characters apart already produce byte-identical
/// modules. It is here because the failure it prevents is silent. The day this
/// crate grows a `path = "../../runtime/rust"` dependency, `frustrate` bodies
/// monomorphized here come back absolute — reconstructed from the `working_dir`
/// in that crate's metadata — and the module quietly becomes a different file
/// in every checkout. `tools/check_block.dart` carries the full argument for
/// the same flag.
///
/// The `~/.rustup` and `~/.cargo/registry` paths that `-Zbuild-std` leaves in
/// the module are deliberately untouched: they are machine-specific but
/// directory-invariant, so they cannot make two checkouts of one commit
/// disagree. This module is the demonstration — it carries five of them and is
/// byte-identical across checkouts anyway.
const _remapTo = '/frustrate';

Future<void> main() async {
  // Symlink-resolved, because the prefix `--remap-path-prefix` matches against
  // is the working directory the kernel reports (`getcwd`), never the logical
  // path a `Platform.script` URI carries. A checkout under /tmp on macOS, whose
  // real name is /private/tmp, is the case that catches an unresolved one.
  final spikeDir = File.fromUri(Platform.script).parent.parent
      .resolveSymbolicLinksSync();

  // The CHECKOUT root, not `spikeDir`, and that is the whole point of the
  // flag. An out-of-workspace path dependency does not leak its own directory
  // — it leaks the reconstructed absolute path of the dependency's sources,
  // which for `../../runtime/rust` starts at the checkout root and never
  // passes through `tests/threaded_wasm_spike`. A remap written for `spikeDir`
  // would sit there matching nothing on the exact day it was needed, and the
  // guard below would agree with it. `spikeDir` is under this, so the spike's
  // own sources stay covered.
  final checkout = Directory('$spikeDir/../..').resolveSymbolicLinksSync();

  final rustflags = [
    '--remap-path-prefix', '$checkout=$_remapTo',
    '-C', 'target-feature=+atomics,+bulk-memory,+mutable-globals',
    '-C', 'link-arg=--import-memory',
    '-C', 'link-arg=--shared-memory',
    // 4096 pages = 256 MiB; the JS glue must create the shared memory with
    // maximum <= this.
    '-C', 'link-arg=--max-memory=268435456',
    '-C', 'link-arg=--export=__stack_pointer',
    '-C', 'link-arg=--export=__wasm_init_tls',
    '-C', 'link-arg=--export=__tls_size',
    '-C', 'link-arg=--export=__tls_align',
  ];

  final build = await Process.start(
    'cargo',
    [
      '+nightly',
      'build',
      '--release',
      '--target',
      'wasm32-unknown-unknown',
      '-Zbuild-std=std,panic_abort',
    ],
    workingDirectory: spikeDir,
    // `CARGO_ENCODED_RUSTFLAGS`, not `RUSTFLAGS`: cargo splits the latter on
    // whitespace, so its meaning depends on no flag value ever containing a
    // space — and the remap above carries the checkout's path, which a
    // directory named `My Projects` would tear in half. `\x1f`-separated and
    // never split, so the argument vector is what it says it is.
    environment: {'CARGO_ENCODED_RUSTFLAGS': rustflags.join('\x1f')},
    mode: ProcessStartMode.inheritStdio,
  );
  final code = await build.exitCode;
  if (code != 0) {
    exit(code);
  }

  final built = File(
    '$spikeDir/target/wasm32-unknown-unknown/release/'
    'threaded_wasm_spike.wasm',
  );
  _requireRemapped(built, checkout);
  built.copySync('$spikeDir/module.wasm');
  stdout.writeln(
    'Wrote ${spikeDir}/module.wasm '
    '(${built.lengthSync()} bytes)',
  );
}

/// Control: the module does not name the directory it was built in.
///
/// `--remap-path-prefix` does nothing at all when its prefix is not a prefix
/// of the path rustc sees, and says nothing when it does nothing, so a build
/// that succeeds is no evidence the path is gone. That is exactly the state
/// this spike is in today — the remap matches nothing because nothing absolute
/// reaches the module — which is why the property is asserted rather than
/// assumed: an inert guard and a working one look identical until the day a
/// path does leak.
void _requireRemapped(File wasm, String checkout) {
  final text = String.fromCharCodes(wasm.readAsBytesSync());
  if (!text.contains(String.fromCharCodes(utf8.encode(checkout)))) return;
  stderr.writeln(
    'build.dart: the module still embeds this checkout\'s '
    'path.\n\n  $checkout\n\n'
    'It was asked to remap that to $_remapTo, and `--remap-path-prefix` is a '
    'silent no-op\nwhen its prefix is not a prefix of the path rustc sees — '
    'a symlinked checkout, or\n/tmp against its real name /private/tmp. '
    'Left in, it makes the module a different\nfile in every checkout.',
  );
  exit(2);
}
