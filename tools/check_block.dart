/// Gate: no `#[bridge(no_block)]` body may reach `memory.atomic.wait`.
///
///     dart run tools/check_block.dart [--no-build]
///
/// This is the cargo driver for `bazel/wasm_block_check`. It builds one
/// throwaway wasm module in the check configuration and hands it to that
/// binary, which owns the predicate — the decoder, its self-test, the
/// fatal-path exemption, and the limits of what green means. Bazel runs the
/// *same* binary as a validation action, so there is one predicate and two
/// drivers rather than two implementations that can drift.
///
/// Under `--cfg frustrate_block_check` codegen emits one `#[no_mangle]` root
/// per artifact-settled claim and gates every other `#[no_mangle]` — 4 generated, 12 in
/// the runtime — out of the build, so lld's GC leaves only what the claims can
/// reach. What the artifact can and cannot settle is
/// `bazel/wasm_block_check/main.rs`.
///
/// # Not every claim wants an artifact
///
/// A claim is settled by *placement* when the calling thread never executes a
/// wait belonging to the member — an actor member's body runs on the actor's
/// own executor on every platform, and a dispatched member's is handed to the
/// pool or the cooperative executor, which puts it off the caller wherever the
/// wait instruction exists at all. Neither wants a root. Which is why this
/// reads the **claim census** codegen writes ([_census]) before deciding to
/// build anything: a claim set with nothing to prove is green with no wasm
/// build at all, and a claim set whose roots are *missing* is a build error
/// rather than a quiet pass. The scanner owns both verdicts; this driver only
/// decides whether there is a module to hand it.
///
/// The census is refreshed before it is read, by a cheap host `cargo check`
/// that reruns `tests/test_api/build.rs`. Without that the hazard is
/// one-directional and quiet: a census that predates a newly added sync claim
/// would say "nothing to prove" and skip the build. The opposite staleness is
/// already loud — a census claiming a root the artifact lacks is exactly the
/// error the scanner names.
///
/// # Why the build configuration is exactly this
///
/// Every flag below was measured, and each of them changes the answer:
///
/// * **debug, never release.** `--release` inlines the wait intrinsic into
///   about ten callers, so there is no one named function left to attribute.
/// * **`+atomics`,** which is what makes std select its futex backends and
///   makes `memory.atomic.wait32` exist at all. Without it the gate is vacuous.
/// * **no `--features wasm-threads`.** With the feature on, `pool::spawn_one`
///   takes the address of `|| QUEUE.run_worker()`, which keeps `run_worker` —
///   and the `memory.atomic.wait32` a pool worker legitimately parks on — alive
///   by relocation with no direct edge from any root. That is the scanner's
///   "cannot decide", and it would be the answer for every claim in the module.
///   This pairing (+atomics, feature off) exists nowhere else: `pool.rs` has a
///   `compile_error!` refusing it in any build that is not this one, because it
///   is also the one configuration in which a dispatched member's placement
///   argument would not hold.
/// * **no `--shared-memory` / `--import-memory` / `--max-memory`.** Dropping
///   the link args removes lld's `__wasm_init_memory` start function, whose
///   passive-data-init barrier is a legitimate wait. The scanner has a
///   structural exemption for it — Bazel gets those args from the atomics
///   toolchain and cannot subtract them — but here the predicate is a literal
///   zero.
/// * **`-Zbuild-std=std,panic_abort`,** because `+atomics` needs a std compiled
///   for it; the shipped `rust-std-wasm32-unknown-unknown` is not.
/// * **`-C linker=bazel/wasm_block_check/link_export_filter.py`,** `rust-lld`
///   with every `--export` the cfg could not suppress cut off the link line, so
///   the GC still sees "exports == roots" on a graph whose dependency rlibs
///   carry `#[wasm_bindgen]` exports. A no-op on this workspace, and set anyway
///   so both drivers link the same way and the filter's fail-closed check runs
///   here too.
/// * **`-A dead_code`.** Gating the exports out is what makes the code behind
///   them unreferenced: 6 dead-code warnings that are the *intended*
///   consequence of the configuration, allowed in this build alone.
/// * **`--remap-path-prefix`,** so the module does not depend on where the
///   checkout lives. See below.
///
/// # The checkout's path is not in the artifact
///
/// Cargo hands rustc *relative* source paths, but a `#[track_caller]` or
/// generic body from `frustrate` monomorphized inside `test_api` gets its span
/// from `frustrate`'s crate metadata, which records file names against an
/// **absolute** `working_dir`. This repo settles claims by diffing artifacts,
/// and an embedded checkout path turns "does this change contribute bytes?"
/// into a question that can only be answered by building every copy at a path
/// of the same *length*.
///
/// A fixed stem rather than an empty one, so a panic still reads `panicked at
/// /frustrate/runtime/rust/src/spin.rs:42:5`. `-Zlocation-detail=none` would
/// also remove the path, by deleting it — and on web that `file:line` is the
/// *only* attribution a panic hook ships (`frustrate_web_init` in
/// runtime/rust/src/lib.rs). Cargo's `trim-paths` profile key would do the same
/// job declaratively, but it is unstable as of cargo 1.91 and the stock wasm
/// build is a *stable*-toolchain build. Compiler diagnostics are untouched: the
/// remap's prefix is absolute and workspace sources reach rustc relative, so a
/// warning still points at `tests/test_api/src/api.rs:42`.
///
/// Two consequences. `target/` is shared with a hand-run `cargo build --target
/// wasm32-unknown-unknown`, which has no remap, so alternating between the two
/// rebuilds the wasm tree. And the remap cannot be displaced from outside this
/// process: cargo reads extra rustc flags from exactly one of
/// `CARGO_ENCODED_RUSTFLAGS`, `RUSTFLAGS`, `target.*.rustflags` or
/// `build.rustflags` — the first it finds, never merged — and this script sets
/// the first. Measured, with a control: a deliberately invalid `RUSTFLAGS` in
/// the environment fails the build when it is the only source and is ignored
/// entirely when `CARGO_ENCODED_RUSTFLAGS` is also set.
///
/// The **encoded** spelling matters because the flag list is not space-free:
/// plain `RUSTFLAGS` is split on whitespace, so a checkout under
/// `~/My Projects/` would have its remap torn into two arguments. The same
/// hazard is one edit away for `--sysroot <path>`, the shape
/// `build_web_fixture.dart` uses for `--facilities`, and *that* one fails
/// silently: the build succeeds against the wrong std.
///
/// # Its own target directory
///
/// These rustflags differ from every other build's, and the module is staged
/// where nothing else looks: a check artifact quietly standing in for the
/// fixture the browser suite runs would be a much worse failure than a slow
/// rebuild.
library;

import 'dart:convert';
import 'dart:io';

/// The members whose claims this gate exists to keep honest.
///
/// The predicate is not the fragile part — the scanner self-tests it on every
/// run, and its export whitelist catches "you scanned the production module"
/// and "the cfg never reached rustc" as named errors. The fragile part is the
/// *fixture*: roots come from whatever `tests/test_api` happens to claim, so
/// deleting a claim does not fail this gate, it silently shrinks what the gate
/// covers while the run still says "clean".
///
/// Three settlements need three guards, and they are [_requiredRoots] (a root
/// reaching a member's whole body), [_requiredResidue] (a root reaching only
/// what a dispatched member leaves on the caller) and [_requiredDispatch] /
/// [_requiredPlacement] (no root at all). Each is a different claim about the
/// world, so losing one is a different loss.
const _requiredRoots = {
  'add_i32':
      'the sync claim — the simplest shape a red result can produce a '
      'full root-to-body witness for, and one of the two the gate\'s sabotage '
      'test uses',
  'emit_two':
      'the stream claim — the only root that links `StreamSink`, so '
      'the sink\'s own code (`add`, and drop-retire\'s terminal) sits inside '
      'what the gate can see. Without it every claimed member was scalar-in, '
      'scalar-out, and a parking lock on the stream path — there was one, in '
      '`frustrate::testing`\'s capture branch — went unnoticed while this '
      'still said "clean"',
  'LiveProbe::new':
      'the handed-out handle claim: a claimed member returning '
      'an opaque with a real `Drop`. `dispose()` runs that `Drop` inline on '
      'the calling isolate and the un-disposed path runs it from a Dart '
      '`Finalizer` on the isolate that attached it — the main thread, for a '
      'handle the main isolate holds — so the claim covers a body that is not '
      'the member\'s own. Without it no root links any drop glue and a '
      'blocking `Drop` was invisible here, which is how this class of hazard '
      'stayed unscanned',
};

/// The claims settled by a **residue** root: a dispatched member whose body is
/// placed, but which leaves something of itself on the calling thread.
///
/// Kept apart from [_requiredRoots] because the loss is different. A member
/// falling out of that map stops having its body scanned; a member falling out
/// of this one stops having a root *at all* — it becomes `placement-dispatch`,
/// which is green, silently, and correct only if the residue really did go
/// away.
const _requiredResidue = {
  'find_snapshot':
      'the drop-edge residue one level in: `Option<Snapshot>` '
      'from a dispatched member, so the edge comes from the return '
      'type-graph walk rather than a top-level match, over a Frozen `Arc` '
      'rather than a Confined `Box`. Two shapes, because a walk that stopped '
      'at the top level would still pass on `LiveProbe::new` alone — and a '
      'dispatched shape, because that root has no body in it to hide behind',
  'merge_plans':
      'the decode residue, and the only one: `Vec<FakePlanMsg>` '
      'through the pool, whose `BytesCodec::from_bytes` the glue runs on the '
      'calling thread before anything reaches a worker. It is the one piece '
      'of user Rust a dispatched member leaves on the main thread, it is '
      'reached through a collection rather than at the top level, and nothing '
      'else in the fixture links a user codec from a check root',
};

/// The claims settled by **dispatch** placement: handed to the pool or the
/// cooperative executor, with nothing left behind.
///
/// Separate from [_requiredPlacement] because the two arguments are different
/// and only one of them holds on every configuration. Losing this pin would
/// un-test the settlement that this whole check exists to permit — a claimed
/// body that genuinely reaches a wait.
const _requiredDispatch = {
  'once_setting':
      'the claim that must not need an artifact: a pool member '
      'whose body reads a `static OnceLock`, which on +atomics is a real '
      'reachable `memory.atomic.wait32`. Scanning it would be red. It is '
      'green because the body is placed, and this is the fixture that says '
      'so — the shape `e2e/iroh_demo` hit on its first fallible call',
  'withdraw_awaiting':
      'the same settlement for an `async fn`, which is the '
      'harder half: the body suspends, so its later polls and the drop of a '
      'cancelled future happen on some drain rather than in the call. Both go '
      'to the pool on threaded web (`arrange_drain`, `Executor::cancel`), and '
      'this is the only claimed `async fn` either driver reports',
};

/// The claims this gate covers *without* an artifact, kept honest for the same
/// reason as [_requiredRoots] and by the same mechanism.
///
/// A placement claim leaves no trace in any module, so nothing downstream can
/// notice it going away — the run would simply stop saying "settled by
/// placement" and still be green. Losing this one would un-test the mixed
/// report, which is the only shape either driver reports both halves in.
const _requiredPlacement = {
  'Miner::sleep_on_executor':
      'the placement claim, and deliberately a member '
      'that really does wait: `thread::sleep` on the actor\'s own executor. A '
      'claim that is true only because of where the body runs is the one worth '
      'pinning, since a claim on a body that never waits would still pass if '
      'placement stopped being what settled it',
};

/// The census codegen writes beside the generated glue: every
/// `#[bridge(no_block)]` claim and how it is settled. Written by
/// `tests/test_api/build.rs`.
const _census = 'tests/test_api/src/frustrate_claims.txt';

/// The pinned nightly, read rather than written down twice — the same read
/// `tests/dart_integration/tool/build_web_fixture.dart` and `analyze.dart` do.
/// `-Zbuild-std` and `+atomics` both need nightly, and they need *this* one:
/// a gate on whatever `+nightly` resolves to is a gate that flakes.
String get _pin =>
    File('toolchain/custom_std/nightly-pin.txt').readAsStringSync().trim();

const _targetDir = 'target/block-check';
const _artifact = '$_targetDir/wasm32-unknown-unknown/debug/test_api.wasm';
const _label = 'tests/test_api (cargo)';

const _rustflags = [
  // std's futex backends, and therefore the instruction being looked for.
  '-C', 'target-feature=+atomics,+bulk-memory,+mutable-globals',
  // Emit the check roots; suppress every other `#[no_mangle]`.
  '--cfg', 'frustrate_block_check',
  // The 6 warnings that suppression causes. See the header.
  '-A', 'dead_code',
];

/// What the checkout's path is rewritten to. See the header.
const _remapTo = '/frustrate';

/// The linker the check build uses. See the header.
///
/// Absolute, because rustc runs it from whatever directory cargo picks, and
/// built from the **symlink-resolved** [root] for the same reason [_remapFlags]
/// is — `/tmp` against `/private/tmp` names a path that resolves here and not
/// there.
List<String> _linkerFlags(String root) => [
  '-C',
  'linker=$root/bazel/wasm_block_check/link_export_filter.py',
];

/// `--remap-path-prefix` for a checkout rooted at [root].
///
/// [root] must be the **symlink-resolved** directory, because the prefix rustc
/// matches against is the working directory the kernel reports (`getcwd`),
/// never the logical path a shell or a `Platform.script` URI carries. A `/tmp`
/// checkout on macOS is the case that catches this: cargo's rustc sees
/// `/private/tmp/…`, so a remap written for `/tmp/…` matches nothing and the
/// artifact still names the checkout.
List<String> _remapFlags(String root) => [
  '--remap-path-prefix',
  '$root=$_remapTo',
];

Future<void> main(List<String> args) async {
  Directory.current = File.fromUri(Platform.script).parent.parent;
  final root = Directory.current.resolveSymbolicLinksSync();
  final noBuild = args.contains('--no-build');
  final pin = _pin;
  _requireToolchain(pin);

  // The census decides whether there is anything to build, so it is refreshed
  // first — a host `cargo check` reruns build.rs and nothing else. Skipped
  // under --no-build, which means "trust what is on disk" for the census as
  // much as for the artifact.
  if (!noBuild) {
    final gen = await Process.run('cargo', ['check', '-q', '-p', 'test_api']);
    if (gen.exitCode != 0) {
      stderr.write(gen.stdout);
      stderr.write(gen.stderr);
      stderr.writeln(
        '\ncheck_block: `cargo check -p test_api` failed, so the '
        'claim census could not be refreshed and nothing was scanned.',
      );
      exit(2);
    }
  }
  final claims = _readCensus();
  _requirePlacement(claims);
  _requireDispatch(claims);
  _requireRoots(claims);
  _requireResidue(claims);
  if (!claims.any(
    (c) => c.startsWith('artifact ') || c.startsWith('artifact-residue '),
  )) {
    // Nothing to prove. Green without a wasm build at all — the scanner writes
    // the verdict so that this and Bazel word it the same way.
    stdout.writeln('==> no claim needs an artifact; not building');
    final only = await Process.run('cargo', [
      'run', '-q', '-p', 'wasm-block-check', //
      '--', '--claims-only', _census, _label,
    ]);
    stdout.write(only.stdout);
    if (only.exitCode != 0) {
      stderr.write(only.stderr);
      exit(1);
    }
    return;
  }

  if (noBuild) {
    stdout.writeln('==> NOT building (--no-build)');
    if (!File(_artifact).existsSync()) {
      stderr.writeln(
        'check_block: --no-build, but $_artifact does not exist. '
        'There is nothing to scan.',
      );
      exit(2);
    }
  } else {
    stdout.writeln(
      '==> building the block-check artifact ($pin, debug, '
      '+atomics, no wasm-threads)',
    );
    final b = await Process.run(
      'cargo',
      [
        '+$pin',
        'build',
        '-p',
        'test_api',
        '--target',
        'wasm32-unknown-unknown',
        '-Zbuild-std=std,panic_abort',
      ],
      environment: {
        'CARGO_TARGET_DIR': _targetDir,
        'CARGO_ENCODED_RUSTFLAGS': [
          ..._rustflags,
          ..._remapFlags(root),
          ..._linkerFlags(root),
        ].join('\x1f'),
      },
    );
    if (b.exitCode != 0) {
      stderr.write(b.stdout);
      stderr.write(b.stderr);
      stderr.writeln(
        '\ncheck_block: the check artifact did not build, so '
        'nothing was scanned.',
      );
      exit(2);
    }
    if (!File(_artifact).existsSync()) {
      stderr.writeln(
        'check_block: cargo reported success but $_artifact does '
        'not exist. Is CARGO_TARGET_DIR or build.target-dir redirecting the '
        'output?',
      );
      exit(2);
    }
  }

  // The one control over the artifact that is this driver's alone: that it
  // does not name the directory it was built in. Root presence is the
  // scanner's, and exact — it matches the census both ways.
  _requireRemapped(
    String.fromCharCodes(File(_artifact).readAsBytesSync()),
    root,
  );

  // The predicate lives in the scanner, and so does every control over the
  // module itself. `cargo run` rather than a prebuilt path so this and the
  // Bazel validation action run the same source.
  final scan = await Process.run('cargo', [
    'run', '-q', '-p', 'wasm-block-check', //
    '--', '--claims', _census, _artifact, _label,
  ]);
  stdout.write(scan.stdout);
  if (scan.exitCode != 0) {
    stderr.write(scan.stderr);
    exit(1);
  }
}

/// The census rows, or a named failure if it is not there to read.
List<String> _readCensus() {
  final f = File(_census);
  if (!f.existsSync()) {
    stderr.writeln(
      'check_block: $_census does not exist, so what this gate '
      'covers is unknown.\nIt is written by tests/test_api/build.rs; run '
      '`cargo check -p test_api`.',
    );
    exit(2);
  }
  return f
      .readAsLinesSync()
      .map((l) => l.trim())
      .where((l) => l.isNotEmpty && !l.startsWith('#'))
      .toList();
}

/// Control: the fixture still claims what this gate was built to cover.
///
/// The scanner now matches roots against the census in both directions, so
/// "is the root there" is no longer this driver's job. What is still its job is
/// the layer underneath: roots come from whatever `tests/test_api` happens to
/// claim, so *deleting a claim* does not fail the gate — it silently shrinks
/// what the gate covers while the run still says "clean". Read against the
/// census rather than the artifact, because the census is the only place a
/// claim exists before a module does.
void _requireRoots(List<String> census) => _requireClaims(
  census,
  'a full artifact',
  _requiredRoots,
  (m) => RegExp('^artifact \\d+ (\\w+::)?$m\$'),
);

/// The same control for the residue roots. See [_requiredResidue].
void _requireResidue(List<String> census) => _requireClaims(
  census,
  'a residue artifact',
  _requiredResidue,
  (m) => RegExp('^artifact-residue \\d+ (\\w+::)?$m\$'),
);

/// The same control for the half no artifact can carry. See [_requiredPlacement].
void _requirePlacement(List<String> census) => _requireClaims(
  census,
  'an actor placement',
  _requiredPlacement,
  (m) => RegExp('^placement \\d+ ${RegExp.escape(m)}\$'),
);

/// The same control for dispatch placement. See [_requiredDispatch].
void _requireDispatch(List<String> census) => _requireClaims(
  census,
  'a dispatch placement',
  _requiredDispatch,
  (m) => RegExp('^placement-dispatch \\d+ ${RegExp.escape(m)}\$'),
);

void _requireClaims(
  List<String> census,
  String settled,
  Map<String, String> want,
  RegExp Function(String) row,
) {
  final missing = want.keys.where((m) => !census.any(row(m).hasMatch)).toList();
  if (missing.isEmpty) return;

  stderr.writeln(
    'check_block: $_census no longer carries $settled claim for '
    '${missing.length == 1 ? 'this member' : 'these members'}:\n',
  );
  for (final m in missing) {
    stderr.writeln('  $m — ${want[m]}');
  }
  stderr.writeln(
    '\nThe scan would still pass on whatever claims remain, which '
    'is why this is checked\nhere: losing a claim does not make this gate '
    'red, it makes it cover less while still\nsaying "clean". Restore the '
    '`#[bridge(no_block)]` in tests/test_api/src/api.rs, or —\nif the member '
    'genuinely may block now — pick a replacement of the same shape and '
    'name\nit above, with the reason it earns a permanent place.',
  );
  exit(2);
}

/// Control: the artifact does not name the directory it was built in.
///
/// What this defends against is not a flag being overridden — nothing in the
/// environment displaces `CARGO_ENCODED_RUSTFLAGS` — but a flag that is
/// *present and inert*. `--remap-path-prefix` does nothing at all when its
/// prefix does not match, and says nothing when it does nothing, so a build
/// that succeeds is no evidence that the path is gone.
///
/// [root] is what the remap was written for, so a mismatch between it and the
/// directory cargo actually ran rustc in — the `/tmp` versus `/private/tmp`
/// case `_remapFlags` documents — shows up here as a failure rather than as a
/// quietly path-dependent artifact. It is UTF-8 encoded and then widened the
/// same way [text] was, so the check still fires for a checkout path with a
/// non-ASCII character in it; a guard that cannot fail is worse than no guard,
/// because it reads as evidence.
void _requireRemapped(String text, String root) {
  if (!text.contains(String.fromCharCodes(utf8.encode(root)))) return;
  stderr.writeln(
    'check_block: the artifact still embeds this checkout\'s '
    'path.\n\n  $root\n\n'
    'The build was asked to remap it to $_remapTo, and '
    '`--remap-path-prefix` is a silent\nno-op when its prefix is not a '
    'prefix of the path rustc sees. So the usual cause\nis that the two '
    'spell the same directory differently — a symlinked checkout, or\n'
    '/tmp against its real name /private/tmp. Leaving it embedded makes the '
    'module a\ndifferent file in every checkout, which is exactly what '
    'artifact-level evidence\ncannot afford.',
  );
  exit(2);
}

/// Fail with the exact commands to run, rather than falling back to a floating
/// `+nightly`. `rust-src` is listed because `-Zbuild-std` compiles std from
/// source: without it cargo fails deep inside the build with a message about a
/// missing `library/std/Cargo.toml`, which reads like a toolchain bug.
void _requireToolchain(String pin) {
  final installed = Process.runSync('rustup', [
    'component',
    'list',
    '--toolchain',
    pin,
    '--installed',
  ]);
  if (installed.exitCode != 0) {
    stderr.writeln(
      'check_block: the pinned toolchain $pin is not installed.\n'
      'Run:\n\n  rustup toolchain install $pin\n',
    );
    exit(2);
  }
  final targets = Process.runSync('rustup', [
    'target',
    'list',
    '--toolchain',
    pin,
    '--installed',
  ]).stdout.toString();
  final fixes = <String>[
    if (!installed.stdout.toString().contains('rust-src'))
      'rustup component add --toolchain $pin rust-src',
    if (!targets.contains('wasm32-unknown-unknown'))
      'rustup target add wasm32-unknown-unknown --toolchain $pin',
  ];
  if (fixes.isEmpty) return;
  stderr.writeln(
    'check_block: the pinned toolchain is missing what the check '
    'build needs.\nRun:\n',
  );
  for (final f in fixes) {
    stderr.writeln('  $f');
  }
  exit(2);
}
