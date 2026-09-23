/// Platform-shared contract for actor handles. Not part of the public package
/// surface; the public [ActorHandle] is the per-platform declaration
/// (conditional export in frustrate.dart), which differs only in whether it
/// carries `dart:ffi`'s `Finalizable` marker.
library;

/// The shared members of the user-facing handle to a thread- or worker-hosted
/// instance. [dispose] is the one an owning container needs, which is all
/// `ActorPool` (package:frustrate/workers.dart) requires.
abstract interface class ActorHandleContract {
  /// Idempotent drain-then-stop: in-flight calls complete, the Rust object
  /// drops on its executor, the executor is released. Calls after dispose
  /// throw StateError.
  ///
  /// **Prefer calling it.** There is a GC backstop — an actor collected
  /// without dispose() has its executor reaped and its object dropped, and any
  /// channel it still held ends as *leaked*, naming the type
  /// (`ActorHost.attachReaper`) — but it is a backstop, not a schedule.
  /// Nothing is reclaimed until a collection actually runs, and an isolate
  /// already idled by a leak never allocates, so never collects. On web it is
  /// weaker still: `dart:core`'s [Finalizer] promises only that a callback
  /// *may* run.
  Future<void> dispose();
}
