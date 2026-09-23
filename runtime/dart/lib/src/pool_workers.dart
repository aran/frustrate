/// The live set of threaded-wasm pool workers, and what it means when one
/// fails to start.
///
/// A pool worker can die two ways, and they call for opposite responses:
///
/// - **A trap** killed a worker that was running jobs. The pool replenishes —
///   width is an invariant, and a worker that worked once will work again.
///   That is [retire]: terminate and forget, decide nothing, because the
///   caller respawns immediately.
/// - **A spawn failed**: the worker never ran a job and never will. Retrying
///   is a no-progress storm (the same script, the same module, the same
///   memory), so the only question is whether the pool can still make
///   progress without it. That is [initFailed].
///
/// The answer to that question is deliberately *not* "any failure is fatal".
/// While one worker is alive it drains the queue, so the calls in flight will
/// complete — failing them would reject work that was about to succeed. Fatal
/// means **nothing is left to drain the queue**. Because pool workers send no
/// `ready` message, the live set means "spawned and not yet failed", which is
/// what makes that safe: the latch cannot fire while another candidate might
/// still come up, and it fires once the last one has not.
///
/// Both outcomes report exactly once per pool. [onFatal] carries an error that
/// callers deliver to the futures that can no longer complete; [onDegraded]
/// has no future to attach to and is reported out of band by the caller.
///
/// Pure Dart (the JS `terminate` is injected) so the sequencing above is
/// testable without a browser — the same reason `pending_calls.dart` and
/// `stream_router.dart` are shaped this way. Not part of the public surface.
library;

/// [W] is the platform worker handle — `_JSWorker` in the web transport.
final class PoolWorkers<W extends Object> {
  final void Function(W worker) _terminate;

  /// Reported once, when no worker is left to run a job. The caller fails
  /// every call that can no longer complete and refuses new ones.
  final void Function(StateError error) _onFatal;

  /// Reported once, when the pool lost a worker but can still make progress.
  /// Not attributable to any one call — the caller delivers it out of band.
  final void Function(StateError error) _onDegraded;

  final List<W> _live = [];

  /// Latched: set when the pool can no longer run anything. Permanent — the
  /// only respawn path is replenishment after a trap, which needs a live
  /// worker to trap.
  StateError? _failure;

  /// Whether [_onDegraded] has already fired. A four-worker pool that fails
  /// wholesale would otherwise report "still running at 3, at 2, at 1" on the
  /// way to a fatal that falsifies all three.
  bool _degradedReported = false;

  PoolWorkers({
    required void Function(W worker) terminate,
    required void Function(StateError error) onFatal,
    required void Function(StateError error) onDegraded,
  }) : _terminate = terminate,
       _onFatal = onFatal,
       _onDegraded = onDegraded;

  /// Workers spawned and not yet known to have failed. Not "ready" — there is
  /// no such signal — so this counts candidates, which is exactly what the
  /// fatal decision needs.
  int get live => _live.length;

  /// The latched fatal error, or null while the pool can still run jobs.
  /// Checked on the issue path so a call rejects instead of hanging.
  StateError? get failure => _failure;

  void add(W worker) => _live.add(worker);

  /// Terminate and forget [worker] without deciding anything — the trap path,
  /// whose caller replenishes immediately afterwards, so an empty set here is
  /// transient rather than fatal. Idempotent.
  void retire(W worker) {
    if (!_live.remove(worker)) return;
    _terminate(worker);
  }

  /// [worker] died before it could ever run a job. Retires it, then reports:
  /// fatal if the pool is now empty, degraded otherwise. Idempotent per
  /// worker, and each outcome reports at most once per pool.
  void initFailed(W worker, String cause) {
    if (!_live.remove(worker)) return;
    _terminate(worker);
    if (_live.isEmpty) {
      _fatal(cause);
    } else if (!_degradedReported) {
      _degradedReported = true;
      _onDegraded(
        StateError(
          'frustrate: a threaded-wasm pool worker failed to start ($cause); '
          'the pool is running at $live worker(s) and cannot be replenished — '
          'replacing a worker that fails to start is a no-progress loop. '
          'Async throughput is reduced, and a job that blocks a worker now has '
          'fewer workers to block. Check \$frustrateGlueUrl and that the '
          'glue asset is served.',
        ),
      );
    }
  }

  /// Creating the worker threw synchronously, so it never entered the set —
  /// CSP blocking `blob:` workers. Fatal by construction when it happens on
  /// the initial spawn, and harmless to call when other workers are alive
  /// (it degrades like any other failed spawn).
  void spawnThrew(String cause) {
    if (_live.isEmpty) {
      _fatal(cause);
    } else if (!_degradedReported) {
      _degradedReported = true;
      _onDegraded(
        StateError(
          'frustrate: a threaded-wasm pool worker could not be created '
          '($cause); the pool is running at $live worker(s) and cannot be '
          'replenished.',
        ),
      );
    }
  }

  void _fatal(String cause) {
    if (_failure != null) return;
    final failure = _failure = StateError(
      'frustrate: every threaded-wasm pool worker failed to start, so async '
      'bridge calls cannot run ($cause). Sync calls and actor hosts are '
      'unaffected — each actor owns its own worker. Check '
      '\$frustrateGlueUrl and that the glue asset is served.',
    );
    _onFatal(failure);
  }
}
