/// Cross-isolate completion routing (VM-only).
///
/// Two Dart isolates each `FrustrateNative.init` the SAME bridge library and
/// issue concurrent async calls. Their per-isolate call-id sequences both
/// start at 1, so without isolate-tagged routing the two isolates' call ids
/// collide and the native transport's completions cross-deliver — the first
/// isolate's futures either hang or receive the second isolate's results
/// (silent data corruption). This test pins that they do not: every call's
/// result is correct in both isolates under concurrency.
///
/// Native prerequisite: `cargo build -p test_api`. Run with `dart test`.
@TestOn('vm')
library;

import 'dart:async';
import 'dart:io';
import 'dart:isolate';

import 'package:frustrate/frustrate.dart';
import 'package:frustrate_integration/test_api.frustrate.dart';
import 'package:runfiles/runfiles.dart';
import 'package:test/test.dart';

/// How many concurrent calls each isolate issues per round, and how many
/// rounds. Enough overlap that the two isolates have in-flight calls sharing
/// low call-id bits at the same time (the exact collision the fix routes
/// around).
const _batch = 64;
const _rounds = 4;

int _expectedSum(int n) => n * (n + 1) * (2 * n + 1) ~/ 6;

/// One isolate's workload: many concurrent async calls of two *different*
/// return types issued together. Mixing an `int` return (sumSquares) with a
/// `String` return (concatStrings) means a cross-delivered completion is
/// decoded as the wrong type and throws — so misrouting cannot pass silently.
Future<(List<int>, List<String>)> _runBatch() async {
  final sums = <int>[];
  final cats = <String>[];
  for (var round = 0; round < _rounds; round++) {
    final sumF = Future.wait(
      List.generate(_batch, (i) => sumSquares(n: i + 1)),
    );
    final catF = Future.wait(
      List.generate(
        _batch,
        (i) => concatStrings(parts: ['a$i', 'b$i'], sep: '-'),
      ),
    );
    sums.addAll(await sumF);
    cats.addAll(await catF);
  }
  return (sums, cats);
}

/// Entry point for the second isolate: init the same bridge, run the batch,
/// and ship the results (or the failure) back to the main isolate.
void _workerEntry((SendPort, String) args) async {
  final (toMain, libPath) = args;
  try {
    FrustrateNative.init(libPath);
    toMain.send(await _runBatch());
  } catch (e, st) {
    toMain.send('worker error: $e\n$st');
  }
}

void main() {
  final libPath = _bridgeLibraryPath();

  setUpAll(() {
    // The main isolate is the first to init (isolate tag 1); the worker inits
    // second (tag 2). Under the old single global callback slot, this second
    // init would steal the main isolate's completions.
    FrustrateNative.init(libPath);
  });

  test(
    'concurrent async calls from two isolates never cross-deliver',
    () async {
      final fromWorker = ReceivePort();
      final worker = await Isolate.spawn(_workerEntry, (
        fromWorker.sendPort,
        libPath,
      ));

      // Run the main isolate's identical batch concurrently with the worker's,
      // then collect the worker's results.
      final mainBatch = _runBatch();
      final workerMsg = await fromWorker.first;
      fromWorker.close();
      worker.kill();

      expect(
        workerMsg,
        isA<(List<int>, List<String>)>(),
        reason: 'worker did not return results: $workerMsg',
      );
      final (workerSums, workerCats) = workerMsg as (List<int>, List<String>);
      final (mainSums, mainCats) = await mainBatch;

      for (var round = 0; round < _rounds; round++) {
        for (var i = 0; i < _batch; i++) {
          final idx = round * _batch + i;
          expect(
            mainSums[idx],
            _expectedSum(i + 1),
            reason: 'main isolate sumSquares mismatch at round $round, i $i',
          );
          expect(
            workerSums[idx],
            _expectedSum(i + 1),
            reason: 'worker isolate sumSquares mismatch at round $round, i $i',
          );
          expect(
            mainCats[idx],
            'a$i-b$i',
            reason: 'main isolate concatStrings mismatch at round $round, i $i',
          );
          expect(
            workerCats[idx],
            'a$i-b$i',
            reason:
                'worker isolate concatStrings mismatch at round $round, i $i',
          );
        }
      }
    },
    timeout: const Timeout(Duration(seconds: 30)),
  );
}

/// Locate the bridge dylib. Mirrors init_native.dart: Bazel supplies it as a
/// runfile (TEST_SRCDIR set), the cargo loop uses the cargo target dir.
String _bridgeLibraryPath() {
  if (Platform.environment.containsKey('TEST_SRCDIR')) {
    return Runfiles.create().rlocation(
      '_main/tests/test_api/libtest_api_shared.$_libExt',
    );
  }
  final lib = File('../../target/debug/libtest_api.$_libExt').absolute.path;
  if (!File(lib).existsSync()) {
    fail('libtest_api.$_libExt not found; run `cargo build -p test_api` first');
  }
  return lib;
}

/// The shared-library extension of the host the bridge was built for.
final String _libExt = Platform.isMacOS
    ? 'dylib'
    : Platform.isWindows
    ? 'dll'
    : 'so';
