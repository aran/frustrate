/// Unit tests for the std source patcher (patch.dart).
///
/// These run on the Dart VM against a synthetic rust-src tree — no toolchain,
/// no std build, no browser. That is the point: the patcher is where std
/// source drift gets caught, so its failure modes need a test loop measured in
/// milliseconds rather than in std rebuilds.
///
///     dart test toolchain/custom_std/tool/patch_test.dart
library;

import 'dart:io';

import 'package:test/test.dart';

import 'patch.dart';

/// A synthetic `library/` tree carrying just the anchors the real facilities
/// edit, in the same shape std spells them.
Directory _fakeRustSrc() {
  final root = Directory.systemTemp.createTempSync('rustsrc');
  void write(String rel, String body) {
    final f = File('${root.path}/$rel');
    f.parent.createSync(recursive: true);
    f.writeAsStringSync(body);
  }

  // Mirrors `std/src/sys/time/mod.rs`: every arm binds `imp`, and the file
  // re-exports through it. The clock facility inserts a wasm arm ahead of the
  // `_` fallback, so the fixture carries the two arms either side of the seam.
  write('std/src/sys/time/mod.rs', '''
cfg_select! {
    target_os = "xous" => {
        mod xous;
        use xous as imp;
    }
    _ => {
        mod unsupported;
        use unsupported as imp;
    }
}

pub use imp::{Instant, SystemTime, UNIX_EPOCH};
''');
  write('std/src/sys/random/mod.rs', '''
cfg_select! {
    any(
        all(target_family = "wasm", target_os = "unknown"),
        target_os = "xous",
        target_os = "vexos",
    ) => {
        mod unsupported;
        pub use unsupported::{fill_bytes, hashmap_random_keys};
    }
    _ => {}
}
''');
  write('std/src/sys/stdio/mod.rs', '''
cfg_select! {
    target_os = "windows" => {
        mod windows;
        pub use windows::*;
    }
    _ => {
        mod unsupported;
        pub use unsupported::*;
    }
}
''');
  write('std/src/sys/thread/mod.rs', '''
cfg_select! {
    all(target_family = "wasm", target_feature = "atomics") => {
        mod wasm;
        pub use wasm::available_parallelism;
    }
    _ => {
        mod unsupported;
        pub use unsupported::*;
    }
}
''');
  return root;
}

void main() {
  late Directory src;
  late Directory pal;

  setUp(() {
    src = _fakeRustSrc();
    pal = Directory.systemTemp.createTempSync('pal');
    File('${pal.path}/time.rs').writeAsStringSync('// clock impl\n');
    File('${pal.path}/random.rs').writeAsStringSync('// entropy impl\n');
    File('${pal.path}/stdio.rs').writeAsStringSync('// stdio impl\n');
    File('${pal.path}/thread.rs').writeAsStringSync('// thread impl\n');
  });

  tearDown(() {
    src.deleteSync(recursive: true);
    pal.deleteSync(recursive: true);
  });

  test('a facility drops its source in and retargets the anchor', () {
    applyFacilities(src, pal, [facilityByName('clock')]);

    final placed = File('${src.path}/std/src/sys/time/frustrate.rs');
    expect(
      placed.existsSync(),
      isTrue,
      reason: 'the pal source must land beside the module that names it',
    );
    expect(placed.readAsStringSync(), contains('clock impl'));

    final mod = File('${src.path}/std/src/sys/time/mod.rs').readAsStringSync();
    expect(mod, contains('mod frustrate;'));
    expect(
      mod,
      contains('use frustrate as imp;'),
      reason:
          'every arm of this selector must bind `imp`, because the file '
          're-exports through it — a `pub use frustrate::*;` arm copied from '
          'the stdio facility would leave `imp` unbound',
    );
    expect(
      mod.indexOf('mod frustrate;'),
      lessThan(mod.indexOf('mod unsupported;')),
      reason:
          'the wasm arm must precede the `_` fallback, or cfg_select '
          'takes the fallback first and the stub still ships',
    );
    expect(
      mod,
      contains('use xous as imp;'),
      reason: 'unrelated arms must be untouched',
    );
  });

  test('an unselected facility changes nothing', () {
    applyFacilities(src, pal, [facilityByName('clock')]);
    final random = File('${src.path}/std/src/sys/random/mod.rs')
        .readAsStringSync();
    expect(random, contains('mod unsupported;'));
    expect(random, isNot(contains('frustrate')));
  });

  test('a missing anchor fails loudly, naming the facility and the file', () {
    // Simulates std source drift: the shape the patcher expects is gone.
    final f = File('${src.path}/std/src/sys/time/mod.rs');
    f.writeAsStringSync('// upstream restructured this file\n');

    expect(
      () => applyFacilities(src, pal, [facilityByName('clock')]),
      throwsA(
        isA<PatchError>()
            .having((e) => e.toString(), 'message', contains('clock'))
            .having((e) => e.toString(), 'message', contains('time/mod.rs')),
      ),
    );
  });

  // Every facility, not just one. These anchors *preserve* what they match —
  // each inserts an arm ahead of the arm it found — so after a first pass the
  // anchor still matches exactly once and a count-based guard cannot tell a
  // clean tree from a patched one. The guard is that the replacement is
  // already present, and it has to hold for all four or the odd one out
  // silently grows a second arm.
  for (final name in ['clock', 'random', 'stdio', 'thread']) {
    test('applying $name twice fails rather than silently double-patching', () {
      applyFacilities(src, pal, [facilityByName(name)]);
      expect(
        () => applyFacilities(src, pal, [facilityByName(name)]),
        throwsA(isA<PatchError>()),
        reason:
            'a second pass over an already-patched tree must fail, not '
            'insert the arm again',
      );
    });
  }

  test('facilities compose, each editing its own file', () {
    applyFacilities(src, pal, [
      facilityByName('clock'),
      facilityByName('random'),
    ]);

    expect(
      File('${src.path}/std/src/sys/time/frustrate.rs').existsSync(),
      isTrue,
    );
    expect(
      File('${src.path}/std/src/sys/random/frustrate.rs').existsSync(),
      isTrue,
    );

    final random = File('${src.path}/std/src/sys/random/mod.rs')
        .readAsStringSync();
    expect(
      random,
      contains('pub use frustrate::{fill_bytes, hashmap_random_keys};'),
    );
    // The combined wasm+xous arm must survive for xous; only wasm is diverted,
    // by an arm inserted ahead of it (cfg_select! matches in order).
    expect(random, contains('target_os = "xous"'));
    expect(
      random.indexOf('mod frustrate;'),
      lessThan(random.indexOf('mod unsupported;')),
      reason: 'the wasm arm must precede the combined arm to win the match',
    );
  });

  test(
    'a facility whose pal source is missing fails before it edits anything',
    () {
      File('${pal.path}/time.rs').deleteSync();
      expect(
        () => applyFacilities(src, pal, [facilityByName('clock')]),
        throwsA(
          isA<PatchError>().having(
            (e) => e.toString(),
            'message',
            contains('time.rs'),
          ),
        ),
      );
      // and the tree is untouched, so a failed run leaves nothing half-applied
      final mod = File('${src.path}/std/src/sys/time/mod.rs')
          .readAsStringSync();
      expect(mod, contains('use unsupported as imp;'));
    },
  );

  test('an unknown facility name is refused with the known set', () {
    expect(
      () => facilityByName('sockets'),
      throwsA(
        isA<ArgumentError>().having(
          (e) => e.toString(),
          'message',
          contains('clock'),
        ),
      ),
    );
  });

  group('flavourKey', () {
    test('is the sorted names joined by a dash', () {
      expect(flavourKey(['atomics']), 'atomics');
      expect(flavourKey(['clock', 'random']), 'clock-random');
    });

    test('does not depend on the order the facilities were given in', () {
      expect(
        flavourKey(['random', 'clock', 'atomics']),
        flavourKey(['atomics', 'clock', 'random']),
      );
    });

    test(
      'collapses duplicates, so a repeated --facilities entry is harmless',
      () {
        expect(flavourKey(['clock', 'clock', 'random']), 'clock-random');
      },
    );

    test('refuses an empty set rather than naming a directory ""', () {
      expect(() => flavourKey([]), throwsA(isA<ArgumentError>()));
    });

    /// The exact strings bazel/custom_std.bzl computes for the flavours it
    /// declares. Pinned here because the Starlark side reimplements this rule
    /// and the two must agree byte for byte — a silent disagreement is a
    /// toolchain pointed at a directory that will never exist.
    test('pins the spellings the Starlark side must reproduce', () {
      expect(flavourKey(['atomics']), 'atomics');
      expect(
        flavourKey(['clock', 'random', 'stdio', 'thread']),
        'clock-random-stdio-thread',
      );
      expect(
        flavourKey(['atomics', 'clock', 'random', 'stdio', 'thread']),
        'atomics-clock-random-stdio-thread',
      );
    });
  });

  test('every declared facility is uniquely named and self-consistent', () {
    final seen = <String>{};
    for (final f in allFacilities) {
      expect(seen.add(f.name), isTrue, reason: 'duplicate facility ${f.name}');
      expect(f.anchors, isNotEmpty, reason: '${f.name} declares no anchor');
      expect(
        f.imports,
        isNotEmpty,
        reason:
            '${f.name} must declare the host imports it introduces, so '
            'the manifest can record what a module needs served',
      );
    }
  });
}
