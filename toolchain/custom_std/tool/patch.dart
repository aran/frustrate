/// The std source patcher: what each browser-backed facility replaces, and
/// where.
///
/// A facility is one std subsystem whose `wasm32-unknown-unknown`
/// implementation is a stub — `panic!`, a discard, or a weak fallback — and for
/// which a browser has a real answer. Turning one on means two edits to a
/// *copy* of rust-src: drop in the replacement module, and retarget the one
/// `cfg_select!` arm or `#[path]` that selects the stub.
///
/// **Anchors are exact strings, and a miss is a hard error.** std's internals
/// carry no stability promise, so the shape these match will eventually move.
/// When it does, the build must stop and say which facility lost its footing —
/// never patch approximately, and never silently produce a std that still has
/// the stub in it. That is the whole reason this file is separately testable
/// (patch_test.dart): the drift path is the one that has to stay loud.
library;

import 'dart:io';

class PatchError implements Exception {
  PatchError(this.facility, this.file, this.detail);
  final String facility;
  final String file;
  final String detail;

  @override
  String toString() =>
      'custom_std: facility `$facility` could not patch $file: $detail\n'
      '  This means std\'s source no longer has the shape the patch expects.\n'
      '  Re-read the file against toolchain/custom_std/pal/ and update the\n'
      '  anchor, or drop the facility. Do not loosen the match.';
}

/// One exact-string edit to a file inside the rust-src `library/` tree.
class Anchor {
  const Anchor({required this.file, required this.find, required this.replace});

  /// Path relative to the `library/` directory.
  final String file;
  final String find;
  final String replace;
}

/// One std subsystem, swapped for a browser-backed implementation.
class Facility {
  const Facility({
    required this.name,
    required this.summary,
    required this.palSource,
    required this.installAs,
    required this.anchors,
    this.imports = const [],
  });

  /// Selector name, as passed to `build.dart --facilities=`.
  final String name;

  /// One line, shown by `--list` and written into the dist manifest.
  final String summary;

  /// File under `toolchain/custom_std/pal/`.
  final String palSource;

  /// Where it lands, relative to the rust-src `library/` directory.
  final String installAs;

  final List<Anchor> anchors;

  /// The wasm imports the resulting std will require of the host, so the
  /// manifest can record what a module built against it needs served.
  final List<String> imports;
}

/// The clock: `Instant` and `SystemTime`.
///
/// std routes both to `sys/time/unsupported.rs`, whose `now()` is
/// `panic!("time not implemented on this platform")`. Inserting a wasm arm
/// ahead of the fallback confines the change to wasm32-unknown-unknown —
/// `unsupported` still serves every other target that lands in `_`.
///
/// **This selector binds an alias, where stdio's re-exports.** `sys/time/mod.rs`
/// ends with `pub use imp::{Instant, SystemTime, UNIX_EPOCH};`, so every arm
/// must bind `imp` and the arm below says `use frustrate as imp;` rather than
/// stdio's `pub use frustrate::*;`. Copying the other facility's shape here
/// compiles to a missing `imp` and a confusing error far from this file.
const _clock = Facility(
  name: 'clock',
  summary: 'Instant + SystemTime via performance.now() / Date.now()',
  palSource: 'time.rs',
  installAs: 'std/src/sys/time/frustrate.rs',
  imports: ['frustrate.now_monotonic_ns', 'frustrate.now_wall_ns'],
  anchors: [
    Anchor(
      file: 'std/src/sys/time/mod.rs',
      find: '''    _ => {
        mod unsupported;
        use unsupported as imp;
    }''',
      replace: '''    all(target_family = "wasm", target_os = "unknown") => {
        mod frustrate;
        use frustrate as imp;
    }
    _ => {
        mod unsupported;
        use unsupported as imp;
    }''',
    ),
  ],
);

/// Entropy: `fill_bytes` and `hashmap_random_keys`.
///
/// The stub panics on `fill_bytes`, and — quieter, and the reason this one is
/// worth doing — seeds `HashMap` from allocation addresses, which std itself
/// annotates "isn't particularly secure, but there isn't really an
/// alternative". In a deterministic wasm module those are predictable. In a
/// browser there *is* an alternative.
///
/// The stub's arm covers wasm **and** xous, so this inserts a wasm-only arm
/// ahead of it rather than editing it: `cfg_select!` takes the first match, so
/// xous keeps the stub.
const _random = Facility(
  name: 'random',
  summary: 'fill_bytes + HashMap seeding via crypto.getRandomValues()',
  palSource: 'random.rs',
  installAs: 'std/src/sys/random/frustrate.rs',
  imports: ['frustrate.fill_random'],
  anchors: [
    Anchor(
      // Anchored on the head of the arm rather than the whole `any(...)`
      // list: the arm is shared with xous, vexos and whatever is added next,
      // and that list changes between toolchains while its first two lines do
      // not. Still exactly one match — verified by the patcher, which refuses
      // any other count.
      file: 'std/src/sys/random/mod.rs',
      find: '''    any(
        all(target_family = "wasm", target_os = "unknown"),
''',
      replace: '''    all(target_family = "wasm", target_os = "unknown") => {
        mod frustrate;
        pub use frustrate::{fill_bytes, hashmap_random_keys};
    }
    any(
        all(target_family = "wasm", target_os = "unknown"),
''',
    ),
  ],
);

/// `println!` / `eprintln!`, which the stub accepts and discards.
const _stdio = Facility(
  name: 'stdio',
  summary: 'stdout/stderr to console.log / console.error',
  palSource: 'stdio.rs',
  installAs: 'std/src/sys/stdio/frustrate.rs',
  imports: ['frustrate.write_stdio'],
  anchors: [
    Anchor(
      file: 'std/src/sys/stdio/mod.rs',
      find: '''    _ => {
        mod unsupported;
        pub use unsupported::*;
    }''',
      replace: '''    all(target_family = "wasm", target_os = "unknown") => {
        mod frustrate;
        pub use frustrate::*;
    }
    _ => {
        mod unsupported;
        pub use unsupported::*;
    }''',
    ),
  ],
);

/// `available_parallelism` and `sleep`, which the stubs answer with an error
/// and a panic.
///
/// One facility for two members because they share one `cfg_select!` arm; the
/// reasoning is in `pal/thread.rs`. Both wasm arms in `sys/thread/mod.rs` take
/// these from `unsupported`, so this inserts one wasm arm ahead of both and
/// re-exports the rest of the surface from the same places the arm it precedes
/// would have.
///
/// `sleep` is taken from the host **only without atomics**: the `+atomics`
/// build has std's own futex-backed `sleep`, which is better than anything a
/// host import can do, so the arm keeps it.
const _thread = Facility(
  name: 'thread',
  summary: 'available_parallelism + sleep via the host',
  palSource: 'thread.rs',
  installAs: 'std/src/sys/thread/frustrate.rs',
  imports: ['frustrate.hardware_concurrency', 'frustrate.sleep_ns'],
  anchors: [
    Anchor(
      file: 'std/src/sys/thread/mod.rs',
      find: '''    all(target_family = "wasm", target_feature = "atomics") => {''',
      replace: '''    all(target_family = "wasm", target_os = "unknown") => {
        mod frustrate;
        pub use frustrate::available_parallelism;

        #[cfg(target_feature = "atomics")]
        mod wasm;
        #[cfg(target_feature = "atomics")]
        pub use wasm::sleep;

        #[cfg(not(target_feature = "atomics"))]
        pub use frustrate::sleep;

        #[expect(dead_code)]
        mod unsupported;
        pub use unsupported::{Thread, current_os_id, set_name, yield_now, DEFAULT_MIN_STACK_SIZE};
    }
    all(target_family = "wasm", target_feature = "atomics") => {''',
    ),
  ],
);

/// Every facility this builder knows how to install, in the order a
/// `--facilities` list is applied.
const allFacilities = <Facility>[_clock, _random, _stdio, _thread];

/// The staging directory name for a facility set: the names, sorted, joined by
/// `-`. `atomics` counts like any other.
///
/// **Derived, never chosen.** The builder stages into `dist/<key>/` and the
/// Bazel toolchain looks for `dist/<key>/` computed the same way from the
/// facility set its platform declares — so a toolchain cannot be handed a std
/// built from a different set. The alternative, one `dist/` plus a check, makes
/// a wrong artifact *representable* and relies on catching it; this makes it
/// unrepresentable.
///
/// `bazel/custom_std.bzl` implements this same rule in Starlark, and
/// patch_test.dart pins the spellings both sides depend on. If you change the
/// separator or the ordering, both move together or nothing builds.
String flavourKey(Iterable<String> facilities) {
  final names = facilities.toSet().toList()..sort();
  if (names.isEmpty) {
    throw ArgumentError('a flavour needs at least one facility');
  }
  return names.join('-');
}

Facility facilityByName(String name) {
  for (final f in allFacilities) {
    if (f.name == name) return f;
  }
  throw ArgumentError(
    'unknown facility `$name`; known: ${allFacilities.map((f) => f.name).join(', ')}',
  );
}

/// Apply [facilities] to the rust-src `library/` tree at [libraryRoot], taking
/// replacement modules from [palDir].
///
/// Throws [PatchError] on any anchor that does not match exactly once.
void applyFacilities(
  Directory libraryRoot,
  Directory palDir,
  List<Facility> facilities,
) {
  for (final f in facilities) {
    final source = File('${palDir.path}/${f.palSource}');
    if (!source.existsSync()) {
      throw PatchError(f.name, f.palSource, 'no such file under pal/');
    }

    for (final a in f.anchors) {
      final target = File('${libraryRoot.path}/${a.file}');
      if (!target.existsSync()) {
        throw PatchError(f.name, a.file, 'no such file in rust-src');
      }
      final body = target.readAsStringSync();
      // Already-patched check, and it has to come first. An anchor whose
      // replacement *preserves* it — every `cfg_select!` facility here, which
      // inserts an arm ahead of the one it matched — still matches exactly
      // once on a tree that has already been patched, so the count below
      // cannot tell a clean tree from a patched one and a second pass would
      // insert the arm twice. (Only the old `#[path]` clock anchor was
      // self-consuming, which is why this was invisible until it moved.)
      // Nothing in the normal flow hits this: build.dart patches a fresh copy
      // of rust-src every time. It is here so that if that ever stops being
      // true, the build stops instead of producing a std with two arms.
      if (body.contains(a.replace)) {
        throw PatchError(
          f.name,
          a.file,
          'this file already carries the facility\'s replacement — the tree '
          'has been patched before. Patch a fresh copy of rust-src rather '
          'than re-running over a patched one',
        );
      }
      final hits = a.find.allMatches(body).length;
      if (hits != 1) {
        throw PatchError(
          f.name,
          a.file,
          'expected its anchor exactly once, found $hits',
        );
      }
      target.writeAsStringSync(body.replaceFirst(a.find, a.replace));
    }

    final dest = File('${libraryRoot.path}/${f.installAs}');
    dest.parent.createSync(recursive: true);
    source.copySync(dest.path);
  }
}
