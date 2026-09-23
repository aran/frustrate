/// In-flight async bridge calls for one channel — the transport itself or one
/// actor host.
///
/// Every async call is the same three steps: allocate an id, register a
/// `Completer` under it, then hand the request to the platform (an FFI call, a
/// wasm export, a worker `postMessage`). Only the third step is
/// platform-specific, and only the third step can fail before Rust has the
/// call — so the registration has to be rolled back exactly there. Open-coding
/// that per transport is where each of them got it slightly wrong, so this is
/// the one implementation they share.
///
/// The contract it exists to hold, uniformly on every platform:
///
/// - **A `Future`-returning bridge API never throws synchronously.** Failures
///   arrive through the returned future, so `unawaited(…)`, `Future.wait([…])`,
///   `.catchError(…)` and store-then-await-later all behave. (A synchronous
///   throw is caught by `try { await f(); }` — it is those other forms it
///   breaks, which is exactly where a transport error is least expected.)
/// - **A failed issue leaves no trace.** No pending entry, and so — via
///   [onChanged] — no keep-alive pin. On native that second half is the
///   difference between a clean error and a process that never exits.
///
/// Not part of the public package surface, with one exception:
/// [FrustrateCancelToken], which generated bindings hand back to application
/// code. It lives here because cancellation *is* a pending call's lifecycle.
library;

import 'dart:async';
import 'dart:typed_data';

import 'binary_codec.dart';
import 'envelope.dart';
import 'exceptions.dart';

/// Cancels one or more in-flight bridged `async fn` calls.
///
/// Created by application code and passed to a generated `async` member:
///
/// ```dart
/// final token = FrustrateCancelToken();
/// final f = api.slowThing(x: x, cancel: token);
/// token.cancel();
/// // f rejects with CancelledCallException — unless it had already answered.
/// ```
///
/// **What cancelling does on the Rust side is drop the future**, at whatever
/// `.await` it was suspended on: the call's task is claimed out of the
/// cooperative executor's registry and its future is dropped on the next drain
/// (executor.rs). There is no second lifecycle to reason about — no abort
/// handle, no cooperative flag the body has to poll — so a body's cleanup is
/// exactly its ordinary `Drop`.
///
/// **A cancel that arrives too late does nothing.** The runtime answers whether
/// the cancel *claimed* the call; only a claimed call is failed here. A call
/// that already completed (or whose response is in flight) keeps its real
/// answer, so a cancel racing a completion is a legal race rather than a
/// dropped result.
///
/// **Reusable, and one-way.** One token may be given to several calls;
/// [cancel] cancels all of them at once. It also *latches*: a call given an
/// already-cancelled token is refused before it encodes anything, so a token
/// works as a scope ("everything this screen asked for") and not only as a
/// handle on one call.
///
/// Cancelling a batch rejects every bound future at once, so **attach the
/// handlers before you cancel** — `Future.wait`, or a `.then(onError:)` per
/// call. Awaiting them one at a time afterwards leaves the rest rejected with
/// nobody listening for a turn, which Dart reports as an unhandled async
/// error. That is ordinary `Future` behaviour rather than anything this adds,
/// but a batch cancel is where it is easiest to meet.
///
/// Only members that can honour it take one, and there are exactly two: a Rust
/// `async fn`, and a `Deferred<T>` actor method. Everything else — a sync
/// member, a plain `#[bridge] fn` dispatched to a pool worker, a *plain* actor
/// method — has no droppable future and so no parameter to pass this to.
///
/// `dispose()` still cancels every outstanding deferred completion of an actor
/// at once; the token is the finer lever beside it, for abandoning one slow
/// call while the actor keeps serving. A deferred call is claimable from the
/// moment it is issued, including while it waits its turn behind a slow method
/// on the same actor. On web its future lives in another Worker's wasm
/// instance, so the claim is a `postMessage` round trip and the
/// future rejects a turn later than `cancel()` returns — the same
/// worker-instance physics that makes `cancelStream` advisory on an actor.
final class FrustrateCancelToken {
  bool _cancelled = false;

  /// The calls this token is currently bound to, and who owns each. Entries
  /// are removed when the call settles, so a long-lived token does not
  /// accumulate ids for the life of the isolate.
  final Map<int, PendingCalls> _bound = {};

  /// Whether [cancel] has been called. Latched: a call issued with a cancelled
  /// token is refused rather than dispatched.
  bool get isCancelled => _cancelled;

  /// Cancel every call bound to this token, and latch so later ones are
  /// refused.
  ///
  /// Idempotent, and safe at any point in a call's life — before it is issued,
  /// while it is suspended, after it answered.
  void cancel() {
    _cancelled = true;
    // Drained before cancelling, not during: `PendingCalls.cancelCall` settles
    // the future, and a listener that runs synchronously off that could issue
    // another call with this same token. Draining first means such a call is
    // refused by the latch above rather than mutating the map being iterated.
    final bound = List.of(_bound.entries);
    _bound.clear();
    for (final e in bound) {
      e.value.cancelCall(e.key);
    }
  }

  /// Bind [callId] on [owner]. Called by [PendingCalls.issue]; not application
  /// API.
  void bind(int callId, PendingCalls owner) {
    _bound[callId] = owner;
  }

  /// Release [callId] — the call settled on its own. Called by [PendingCalls];
  /// not application API.
  void unbind(int callId) {
    _bound.remove(callId);
  }
}

/// How many async calls are in flight across every [PendingCalls] that shares
/// one of these — a transport's own, plus each actor host it spawned.
///
/// Shared rather than summed on demand because hosts are not enumerable:
/// neither transport keeps a list of the hosts it spawned, and a host reaped
/// by the GC never reports its own death ([ActorHost.attachReaper]), so a
/// registry would either leak entries or undercount. A counter every
/// participant keeps current is exact and costs nothing to read.
///
/// Backs [FrustrateRuntime.inFlightCallCount], which
/// [Frustrate.activate]/[Frustrate.reset] read as half of "quiescent".
final class InFlightCalls {
  int _count = 0;

  /// Calls registered right now across every sharer.
  int get count => _count;
}

final class PendingCalls {
  /// Allocates from the owning channel's id sequence — isolate-tagged on
  /// native, the page-wide sequence on web, where call ids share a space with
  /// the router's stream and callback ids (see `StreamRouter`).
  final int Function() _nextId;

  /// Called after every mutation, so a transport can recompute state derived
  /// from "is a call in flight". Native passes `_updateKeepAlive`: an isolate
  /// stays alive exactly while it owes someone a completion. Web has nothing
  /// to recompute and passes nothing.
  final void Function()? _onChanged;

  final Map<int, Completer<BinaryReader>> _calls = {};

  /// Per-call decoder for a `statusTypedError` payload, for the calls that
  /// have one. Kept beside the completer rather than passed to [complete],
  /// because the transport that receives the response knows only a call id —
  /// the *binding* knows the error's type, and that knowledge has to survive
  /// from issue to completion.
  final Map<int, Object Function(BinaryReader)> _typedErrors = {};

  /// The cancel token watching each cancellable call, so a call that settles
  /// on its own releases its binding. Keyed like [_typedErrors] and drained
  /// with it — a token that outlived every call it was given would otherwise
  /// hold their ids forever.
  final Map<int, FrustrateCancelToken> _tokens = {};

  /// Claim [callId] out of the Rust cooperative executor
  /// (`frustrate_call_cancel`), returning whether the cancel claimed it **and
  /// this frame owns failing the future**.
  ///
  /// Two answer shapes, because the executors are in two places:
  ///
  ///   * **Synchronous** (the native transport, and native actor hosts through
  ///     it): the FFI call returns the claim under the executor's own registry
  ///     lock, so `true` here means "claimed, fail it now".
  ///   * **Asynchronous** (a web actor host): the future is parked on a Worker's
  ///     own wasm instance, so the claim is a `postMessage` round trip. Such a
  ///     transport returns `false` — nothing to fail in *this* frame — and fails
  ///     the call itself when the worker answers.
  ///
  /// `false` therefore means only "not claimed here", never "the call
  /// survived". [cancelCall] treats it as "leave the call alone", which is
  /// right for both.
  ///
  /// Absent on a channel that carries no cancellable member.
  final bool Function(int callId)? _cancelCall;

  /// The transport-wide in-flight tally this channel contributes to, if the
  /// transport keeps one.
  final InFlightCalls? _tally;

  /// This channel's last-reported contribution to [_tally]. Kept so the tally
  /// can be maintained as a *difference from the truth* rather than by paired
  /// increments and decrements: every mutation path already ends at [_changed],
  /// and recomputing from `_calls.length` there means a path that forgets to
  /// decrement cannot exist.
  int _tallied = 0;

  PendingCalls(
    this._nextId, {
    void Function()? onChanged,
    bool Function(int callId)? cancelCall,
    InFlightCalls? tally,
  }) : _onChanged = onChanged,
       _cancelCall = cancelCall,
       _tally = tally;

  /// Republish this channel's contribution to the shared tally, then run the
  /// transport's own recomputation. Called after every mutation of [_calls].
  void _changed() {
    final tally = _tally;
    if (tally != null) {
      final n = _calls.length;
      tally._count += n - _tallied;
      _tallied = n;
    }
    _onChanged?.call();
  }

  /// Whether any call is in flight. Native's keep-alive reads this.
  bool get isEmpty => _calls.isEmpty;

  /// The ids in flight. The web actor host's shutdown assert reads this to
  /// say something stronger than [isEmpty]: everything still pending must be
  /// a deferred call (the one kind whose completion is detached from the
  /// dispatch turn, and the one kind dispose may legitimately interrupt).
  Iterable<int> get ids => _calls.keys;

  /// Decoders still held. **Test-only** — no `@visibleForTesting`, because
  /// `package:meta` is not a dependency of this package under Bazel and one
  /// annotation does not justify making it one.
  ///
  /// Deliberately not folded into [isEmpty]: that drives native's keep-alive
  /// pin, so a cleanup bug would turn a bounded memory leak into an isolate
  /// that never exits. The invariant — a decoder never outlives its call — is
  /// worth checking and is not worth hanging a process over, so it is
  /// asserted by tests rather than enforced at run time.
  int get retainedDecoders => _typedErrors.length;

  /// Register a call and run [send] with its freshly allocated id.
  ///
  /// [send] is the platform hand-off, and the only part that can throw before
  /// Rust owns the call: a missing export (the lazy `lookupFunction`s are
  /// resolved on first use, *inside* here), a failed request-buffer
  /// allocation, a wasm trap, a rejected `postMessage`. Whatever it throws,
  /// the registration is retired and the error is delivered through the
  /// returned future — never synchronously.
  ///
  /// [cancel], when the member takes a `FrustrateCancelToken`, binds the call
  /// to it for as long as the call is in flight. An **already-cancelled** token
  /// refuses the call here, before [send] runs — so nothing is encoded and, in
  /// particular, no stream or callback registration is opened. That ordering is
  /// what keeps the latch from needing a rollback: registration happens during
  /// encoding, which happens inside [send].
  Future<BinaryReader> issue(
    void Function(int callId) send, {
    Object Function(BinaryReader)? typedError,
    FrustrateCancelToken? cancel,
  }) {
    assert(
      cancel == null || _cancelCall != null,
      'frustrate: a cancel token reached a channel that cannot cancel '
      '(this is a bridge bug)',
    );
    if (cancel != null && cancel.isCancelled) {
      return Future.error(
        CancelledCallException(
          'frustrate: the call was not issued — its FrustrateCancelToken '
          'had already been cancelled',
        ),
        StackTrace.current,
      );
    }
    final callId = _nextId();
    final completer = Completer<BinaryReader>();
    _calls[callId] = completer;
    if (typedError != null) _typedErrors[callId] = typedError;
    if (cancel != null) {
      _tokens[callId] = cancel;
      cancel.bind(callId, this);
    }
    _changed();
    try {
      send(callId);
    } catch (e, st) {
      if (_calls.remove(callId) != null) {
        _typedErrors.remove(callId);
        _releaseToken(callId);
        _changed();
        completer.completeError(e, st);
      } else {
        // Our entry is already gone, so the completion landed *during* [send]
        // and this future is settled: single-threaded web runs the whole
        // async body inline during `frustrate_call_async`, so `post` can fire
        // before a later trap unwinds out of the same export. Completing
        // twice would be a "Future already completed" state error, and
        // swallowing a trap is against the loud-contract stance — report it
        // out of band instead.
        Zone.current.handleUncaughtError(e, st);
      }
    }
    return completer.future;
  }

  /// Settle [callId] from a response envelope: a payload completes the future,
  /// a non-ok status completes it with the envelope's exception.
  ///
  /// Returns false if no such call is in flight. That is a bridge bug wherever
  /// a real completion arrives (ids are never reused), but the callers differ
  /// in how they say so, so the loudness is theirs.
  bool complete(int callId, Uint8List response) {
    final completer = _calls.remove(callId);
    final typedError = _typedErrors.remove(callId);
    _releaseToken(callId);
    _changed();
    if (completer == null) return false;
    completeWithEnvelope(completer, response, typedError: typedError);
    return true;
  }

  /// Settle [callId] with an error — a trap attributed to one call, or a host
  /// torn down under it. Returns false if no such call is in flight, which
  /// lets a caller distinguish "attributed" from "nothing to attribute it to"
  /// (the web executor's mid-poll trap handling depends on that).
  bool fail(int callId, Object error, StackTrace st) {
    final completer = _calls.remove(callId);
    _typedErrors.remove(callId);
    _releaseToken(callId);
    _changed();
    if (completer == null) return false;
    completer.completeError(error, st);
    return true;
  }

  /// Fail every call in flight — the terminated-worker path, where nothing
  /// will ever complete them. Turns a silent forever-hang into an
  /// attributable error.
  void failAll(Object error, StackTrace st) {
    final completers = _calls.values.toList();
    _calls.clear();
    // Cleared with `_calls`, not alongside individual completions: these two
    // maps are keyed the same and every drain of one must drain the other, or
    // a decoder outlives the call that owned it and the map grows for the
    // life of the transport.
    _typedErrors.clear();
    for (final e in _tokens.entries) {
      e.value.unbind(e.key);
    }
    _tokens.clear();
    _changed();
    for (final completer in completers) {
      if (!completer.isCompleted) completer.completeError(error, st);
    }
  }

  /// Cancel [callId] on behalf of its [FrustrateCancelToken].
  ///
  /// The claim protocol, identical to the one `dispose()` runs for a deferred
  /// actor completion (runtime_native.dart): the runtime export answers whether
  /// this cancel took the call out of the executor's registry. **Claimed** means
  /// its completion will never be posted and failing the future is now ours;
  /// **not claimed** means the call already answered — or is answering right now,
  /// race-free under the executor's registry lock — and must be left to complete
  /// normally. Getting this backwards would either drop a real answer or double-
  /// settle a `Completer`.
  ///
  /// A transport whose executor is out of reach answers the claim later; see
  /// [_cancelCall]. It returns false here and calls [failCancelled] itself.
  void cancelCall(int callId) {
    final claim = _cancelCall;
    // Nothing in flight under this id: it settled between the token releasing
    // it and this call, which is the ordinary cancel/completion race.
    if (claim == null || !_calls.containsKey(callId)) return;
    if (claim(callId)) failCancelled(callId);
  }

  /// Settle [callId] as cancelled. The one place the cancellation exception is
  /// built, so a claim answered from another thread of control (a web actor
  /// host's worker reply) says byte-identically what a synchronous claim says.
  ///
  /// Returns [fail]'s answer: false when nothing was in flight under this id,
  /// which a late claim must tolerate rather than assert on.
  bool failCancelled(int callId) => fail(
    callId,
    CancelledCallException(
      'frustrate: the call was cancelled through its '
      'FrustrateCancelToken; the Rust future was dropped where it was '
      'suspended',
    ),
    StackTrace.current,
  );

  void _releaseToken(int callId) {
    _tokens.remove(callId)?.unbind(callId);
  }
}
