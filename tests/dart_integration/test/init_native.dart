/// Native test bootstrap: load the bridge dylib over FFI.
library;

import 'dart:io';

import 'package:frustrate/frustrate.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:runfiles/runfiles.dart';
import 'package:test/test.dart';

/// On native a Rust panic *unwinds*, so a producer panic drops the sink and
/// drop-retire ends the bound stream cleanly (`onDone`). On web
/// (`panic=abort`) no destructor runs, so the stream is left open by design —
/// the panic is still reported on the call. Tests that assert the stream's
/// terminal after a producer panic branch on this.
const bool panicClosesStreams = true;

/// True on the native VM. The open-channel leak guard (which asserts a test
/// disposed its stored sinks/callbacks) is native-only: only there does an open
/// registration keep the isolate alive and hang process exit. On web the page
/// is always alive and retirement timing differs, so the guard would misfire.
const bool isNativeVm = true;

Future<void> initBridge() async {
  FrustrateNative.init(bridgeLibraryPath());
  // Fail loudly here if the loaded dylib and these bindings disagree on the
  // wire schema, rather than dispatching wrong fn_ids into unsafe derefs.
  checkFrustrateSchema();
}

/// Where the bridge dylib is. Public so `init_contract_test.dart` can name
/// the same library a second time (the repeated-init contract).
///
/// Bazel runs this test with runfiles; the cargo loop (`cargo build -p
/// test_api && dart test`) uses the cargo target dir. TEST_SRCDIR is the
/// canonical Bazel-test marker.
///
/// Under the cargo loop, `FRUSTRATE_PROFILE` selects which cargo profile's
/// dylib to load — `debug` (the default) or `release`. A typo is an error, not
/// a silent default, and a missing artifact is a loud failure naming the exact
/// cargo command, never a fallback to the other profile: a debug dylib
/// standing in for a release one makes every number measured through it a lie.
///
/// This selector does NOT reach the Bazel path. `bazel test` gets the profile
/// from `-c` (see README "Release artifacts"), and note that Bazel's *default*
/// compilation mode, fastbuild, is `-Copt-level=0` under rules_rust — so
/// `bazel test //...` measures unoptimized Rust unless you pass `-c opt`.
String bridgeLibraryPath() {
  if (Platform.environment.containsKey('TEST_SRCDIR')) {
    return Runfiles.create().rlocation(
      '_main/tests/test_api/libtest_api_shared.$_libExt',
    );
  }
  final profile = Platform.environment['FRUSTRATE_PROFILE'] ?? 'debug';
  if (profile != 'debug' && profile != 'release') {
    fail(
      'FRUSTRATE_PROFILE=$profile is not a cargo profile; '
      'expected `debug` or `release`',
    );
  }
  final lib = File('../../target/$profile/libtest_api.$_libExt').absolute.path;
  if (!File(lib).existsSync()) {
    fail(
      'libtest_api.$_libExt not found at $lib.\n'
      'Build it with: cargo build -p test_api'
      '${profile == 'release' ? ' --release' : ''}\n'
      '(FRUSTRATE_PROFILE=$profile selected this path; there is deliberately '
      'no fallback to the other profile.)',
    );
  }
  return lib;
}

/// The shared-library extension of the host the bridge was built for.
final String _libExt = Platform.isMacOS
    ? 'dylib'
    : Platform.isWindows
    ? 'dll'
    : 'so';
