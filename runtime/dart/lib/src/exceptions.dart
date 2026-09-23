/// Errors surfaced by frustrate bridge calls. One Dart type per envelope
/// status, so callers can catch precisely.
///
/// The `Exception`/`Error` split is the usual Dart one and is load-bearing
/// here: the first three report an outcome from Rust that a caller may
/// reasonably handle, while [LeakedChannelError] reports a bug in the calling
/// Dart code and is an `Error` accordingly.
library;

/// The Rust function returned `Err`; [message] is the error's Display output
/// (`{:#}`, so anyhow chains are included).
class BridgeException implements Exception {
  final String message;
  BridgeException(this.message);
  @override
  String toString() => 'BridgeException: $message';
}

/// The Rust side panicked. Always a bug — in the bridge crate or in frustrate
/// itself; the message is the panic payload.
class BridgePanicException implements Exception {
  final String message;
  BridgePanicException(this.message);
  @override
  String toString() => 'BridgePanicException: $message';
}

/// A sync call declared `on_contention = "error"` found the lock contended.
/// The message names the type and method (the contract is attributable).
class ContentionException implements Exception {
  final String message;
  ContentionException(this.message);
  @override
  String toString() => 'ContentionException: $message';
}

/// The call was cancelled through its `FrustrateCancelToken`, so it will never
/// answer: the Rust future was dropped where it was suspended.
///
/// An `Exception`, and specifically not an `Error`: this is the outcome the
/// caller asked for, and code that cancels normally wants to catch it. It is
/// raised only for a cancel the runtime **claimed** — a cancel that lost the
/// race against a completion leaves the real answer in place, so seeing this
/// is proof the result never existed rather than that it was discarded.
class CancelledCallException implements Exception {
  final String message;
  CancelledCallException(this.message);
  @override
  String toString() => 'CancelledCallException: $message';
}

/// This stream or callback ended because the Rust object feeding it was
/// garbage-collected without `dispose()` — the channel was abandoned, not
/// closed.
///
/// An [Error], not an [Exception], and the only one this file defines: the
/// other three report what *Rust* did and a caller may reasonably handle them,
/// while this one reports a bug in the consuming Dart code with exactly one
/// remedy — dispose the handle. It extends [StateError] to sit with the
/// runtime's other handle-lifecycle mistake, `'<Type> used after dispose()'`
/// (opaque_handle_base.dart).
///
/// The message names the holder's Rust type and, where the binding supplied
/// one, the member that opened the channel. Reaching this is proof rather than
/// inference: `dispose()` detaches the finalizer before dropping, so the
/// finalize path is unreachable for a handle that was disposed.
///
/// Best-effort by construction: Dart never guarantees a finalizer runs, so a
/// leak that is never collected is never reported, and one in an isolate that
/// has gone idle never will be. `dispose()` discipline
/// and `FrustrateRuntime.openChannelCount` remain the primary defenses; this
/// catches what slips through while the program is still running.
class LeakedChannelError extends StateError {
  LeakedChannelError(super.message);
}
