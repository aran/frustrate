/// A dependency's `log` records reaching Dart, in an app module that is not
/// frustrate.
///
/// This is the assertion `frustrate::logging` exists for and the one no
/// in-repo test can make. `tests/dart_integration/test/logging_test.dart` over
/// there proves the mechanism against a fixture crate that logs on purpose;
/// what it cannot prove is the wiring, because frustrate's own module has one
/// crate hub and therefore one `log` no matter what anyone does. Here there are
/// two hubs. Nothing in `//bridge:api.rs` raises the records this asserts on —
/// iroh, `netwatch`, `quinn` and `rustls` do, through `tracing` with no
/// subscriber installed, against `@iroh_crates//:log`. They arrive only because
/// `//:.bazelrc` points frustrate's `log` build setting at that same alias.
///
/// A plain `main` rather than `package:test`: the app's pub set has no test
/// package, and adding one to a Flutter app's `pubspec.lock` to run three
/// assertions would be the tail wagging the dog. A throw fails the target.
library;

import 'dart:async';
import 'dart:io';

import 'package:frustrate/frustrate.dart';
import 'package:iroh_bridge/iroh_rust.frustrate.dart';
import 'package:runfiles/runfiles.dart';

/// Crates whose records prove the point: none of them has heard of frustrate,
/// and none of them is this app.
///
/// Prefix-matched against `LogRecord.target`, which is the emitting module
/// path. Deliberately a set of crate names and not messages — iroh's log text
/// is not an interface and would rot.
const Set<String> _dependencies = {
  'iroh',
  'netwatch',
  'quinn',
  'quinn_proto',
  'quinn_udp',
  'rustls',
  'n0_watcher',
  'iroh_relay',
  'iroh_base',
};

bool _isDependency(String target) {
  final crate = target.split('::').first;
  return _dependencies.contains(crate);
}

Future<void> main() async {
  FrustrateNative.init(
      Runfiles.create().rlocation('_main/bridge/libiroh_rust.dylib'));
  checkFrustrateSchema();

  final records = <LogRecord>[];
  final logs = StreamController<LogRecord>();
  final logSubscription = logs.stream.listen(records.add);
  // `debug` and not `info`: iroh's bind path is instrumented at debug, and an
  // `info`-only run would assert nothing while looking like it passed.
  installLogging(maxLevel: 'debug', sink: logs);

  // Bind an endpoint, which is the loudest thing this app can do without a
  // peer. `Preset.minimal` with no relay is the honestly-offline cell of
  // `Node.open`'s matrix: no relays, no DNS, no pkarr, so nothing here reaches
  // the network and the target needs no `external` tag.
  final events = StreamController<PeerEvent>();
  final eventSubscription = events.stream.listen((_) {});
  final node = await Node.open(
      nickname: 'logprobe',
      preset: Preset.minimal,
      relay: null,
      events: events);

  // Delivery is a port message, so records land on later turns of the event
  // loop. Bounded, and the bound is what fails the test.
  final deadline = DateTime.now().add(const Duration(seconds: 10));
  while (!records.any((r) => _isDependency(r.target)) &&
      DateTime.now().isBefore(deadline)) {
    await Future<void>.delayed(const Duration(milliseconds: 20));
  }

  final fromDependencies = records.where((r) => _isDependency(r.target));
  if (fromDependencies.isEmpty) {
    final seen = records.map((r) => r.target).toSet().toList()..sort();
    throw StateError(
        'no record from a dependency reached Dart in 10s. ${records.length} '
        'record(s) arrived, from: $seen.\n'
        'If that list is empty, frustrate\'s runtime and this crate are '
        'holding two different `log` rlibs — check that //:.bazelrc still '
        'sets --@frustrate//runtime/rust:log=@iroh_crates//:log.\n'
        'If only this crate\'s targets are listed, something now installs a '
        '`tracing` subscriber, which stops tracing events becoming `log` '
        'records at all.');
  }

  // What arrived has to be a record and not a placeholder: a level `log` can
  // name, a target that is a module path, and something in the message.
  for (final record in fromDependencies) {
    if (!const {'ERROR', 'WARN', 'INFO', 'DEBUG', 'TRACE'}
        .contains(record.level)) {
      throw StateError('unexpected level ${record.level} on ${record.target}');
    }
    if (record.message.isEmpty) {
      throw StateError('empty message from ${record.target}');
    }
  }

  final example = fromDependencies.first;
  stdout.writeln('${fromDependencies.length} of ${records.length} records came '
      'from a dependency; first: [${example.level}] ${example.target}: '
      '${example.message}');

  // Release in the order the runtime documents: the logger outlives the node
  // that provoked it, so uninstalling is what ends the stream.
  node.dispose();
  uninstallLogging();
  await eventSubscription.cancel();
  await logSubscription.cancel();
  await events.close();
  if (!logs.isClosed) await logs.close();
}
