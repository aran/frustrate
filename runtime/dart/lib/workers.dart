/// Optional conveniences over the actor model. Nothing here is required:
/// generated code never imports this library, the core runtime does not
/// depend on it, and everything in it is expressible in application code
/// over the public actor surface. It exists so the policy every fan-out
/// needs — who is free, what a panic does to the pool, how teardown drains —
/// is written and tested once.
///
///     import 'package:frustrate/workers.dart';
///
///     final pool = await ActorPool.spawn((i) => Miner.new_(label: 'w$i'));
///     final results = await Future.wait([
///       for (final n in inputs) pool.run((m) => m.nthPrime(n: n)),
///     ]);
///     await pool.dispose();
library;

export 'src/actor_pool.dart' show ActorPool;
