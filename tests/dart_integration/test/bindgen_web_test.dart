/// An actor that reaches browser APIs from Rust through wasm-bindgen, web-sys
/// and js-sys inside its Worker (//tests/bindgen): JS promises and timers
/// driving its tasks, and the errors that escape it outside any call reaching
/// the zone the actor was built in.
///
/// The module, its wasm-bindgen JS and the bindings are built by Bazel and
/// staged by `bazel run //tests/dart_integration:web_test` (the threaded build
/// under `-- --threaded`); `dart run tool/web_test.dart` leaves this file out
/// by its tag.
@TestOn('browser')
@Tags(['bazel_fixture'])
@Timeout(Duration(minutes: 2))
library;

import 'dart:async';

import 'package:frustrate/frustrate.dart';
import 'package:frustrate/frustrate_web.dart';
import 'package:frustrate_integration/bindgen_api.frustrate.dart';
import 'package:test/test.dart';

/// Run [body] in a fresh guarded zone; answer its result and what the zone
/// hears.
Future<(T, List<Object>)> _guarded<T>(Future<T> Function() body) async {
  final heard = <Object>[];
  final done = Completer<T>();
  runZonedGuarded(
    () => body().then(done.complete, onError: done.completeError),
    (e, _) => heard.add(e),
  );
  return (await done.future, heard);
}

/// Wait, bounded, until [heard] holds something.
Future<void> _until(List<Object> heard) async {
  final deadline = DateTime.now().add(const Duration(seconds: 10));
  while (heard.isEmpty && DateTime.now().isBefore(deadline)) {
    await Future<void>.delayed(const Duration(milliseconds: 5));
  }
}

/// `runtime::spawn_local`, which a `JsFuture` needs because it is not `Send`,
/// exists only on single-threaded wasm. On the threaded build the tests that
/// await a JS promise are skipped; making `spawn_local` exist for actor
/// instances there is a separate decision.
bool get _noSpawnLocal => Frustrate.instance.asyncIsParallel;

const _noSpawnLocalReason =
    'runtime::spawn_local exists only on single-threaded wasm';

void main() {
  setUpAll(() async {
    await FrustrateWeb.initFromUrl(
      '../build/bindgen/bindgen_api.wasm',
      bindgenGlueUrl: '../build/bindgen/bindgen_api_bg.js',
    );
    checkFrustrateSchema();
  });

  test('a spawn_local task awaits a JS promise and answers through Addr '
      'and a stream', () async {
    if (_noSpawnLocal) {
      markTestSkipped(_noSpawnLocalReason);
      return;
    }
    final (b, heard) = await _guarded(Browser.new_);
    final sink = StreamController<int>();
    await b.addFromPromise(x: 5, sink: sink);
    expect(await sink.stream.first, 5);
    expect(await b.total(), 5);
    expect(heard, isEmpty);
    await b.dispose();
  });

  test('a spawn_local task awaits setTimeout from web-sys', () async {
    if (_noSpawnLocal) {
      markTestSkipped(_noSpawnLocalReason);
      return;
    }
    final (b, heard) = await _guarded(Browser.new_);
    final sink = StreamController<int>();
    final clock = Stopwatch()..start();
    await b.addAfterTimeout(ms: 50, x: 3, sink: sink);
    expect(await sink.stream.first, 3);
    expect(
      clock.elapsed,
      greaterThanOrEqualTo(const Duration(milliseconds: 50)),
    );
    expect(heard, isEmpty);
    await b.dispose();
  });

  test('a panic in a wasm-bindgen closure the browser calls reaches the '
      'zone the actor was built in', () async {
    final (b, heard) = await _guarded(Browser.new_);
    await b.panicInCallback(message: 'a timer callback panicked');
    await _until(heard);
    expect(heard, [
      isA<BridgePanicException>().having(
        (e) => e.message,
        'message',
        contains('a timer callback panicked'),
      ),
    ]);
    await b.dispose();
  });

  test('an unhandled promise rejection reaches the zone the actor was built '
      'in, and is not a panic', () async {
    final (b, heard) = await _guarded(Browser.new_);
    await b.rejectUnhandled(reason: 'nobody handled this');
    await _until(heard);
    expect(heard, [
      isA<BridgeException>().having(
        (e) => e.message,
        'message',
        allOf(
          contains('unhandled promise rejection'),
          contains('nobody handled this'),
        ),
      ),
    ]);
    expect(await b.total(), 0, reason: 'the actor still answers');
    await b.dispose();
  });
}
