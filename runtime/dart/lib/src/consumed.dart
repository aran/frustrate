/// Dart's move: giving a Rust object up to the call that consumes it.
///
/// A Rust `fn finish(self)` — or a parameter written `d: Doc` — says the call
/// takes the object. Dart has no move syntax, so the opt-in is a *type*: the
/// handle's generated `take()` mints a [Consumed] token, and the consuming
/// member is reachable only through one. `session.attach(doc: doc)` does not
/// compile; `session.attach(doc: doc.take())` does, and reads as what it is.
library;

/// What a [Consumed] token does to the handle it holds. Implemented by every
/// generated handle class — opaque handles through `OpaqueHandleBase`, actors
/// through their own generated body, which has no common base with them.
///
/// **Generated code only.** Nothing an application writes calls these; the two
/// operations exist so that reading a raw for the wire and giving the object
/// up can happen at different moments (see [Consumed]).
abstract interface class Consumable {
  /// The raw handle value, for encoding — *without* giving the object up.
  ///
  /// Throws the same `StateError` a disposed or stranded handle throws
  /// anywhere else, because it goes through the same read. [member] names the
  /// consuming member, for the message a later use of this handle gets.
  int peekHandle(String member);

  /// Give the object up to [member]: the handle reads as disposed from here
  /// on, and its finalizer is detached so nothing on this side will free what
  /// Rust is about to take.
  void spendHandle(String member);
}

/// One handle marked for consumption by the call it is passed to.
///
/// Minted by a handle's generated `take()` and by nothing else. Holding one
/// costs nothing and commits to nothing: the handle is spent by the call that
/// receives the token, so an unused token leaves its handle exactly as it was.
///
/// **Spending happens at issue, not at the answer, and that is the contract.**
/// The generated encoder writes every consumed raw first and spends every
/// token last, after the whole request has encoded — so a parameter whose
/// encode throws leaves every handle intact — and then the call is issued. It
/// cannot wait for the reply: a call made in between would otherwise reach an
/// object the consuming call had already taken. So a spent token's object is
/// gone whether the call succeeded or not. Where Rust could not take it (see
/// below) it is released rather than handed back, because handing a handle
/// back would make the failure path a resource-management obligation.
///
/// **When a consuming call can fail.** On a `confined` type nothing can be in
/// flight (one isolate, synchronous calls) and on an `actor` the call simply
/// queues behind the ones already sent, so on those two it cannot. On a
/// `frozen` or `locked` type the object is shared, and taking it needs every
/// call on that handle to have finished: pass the token only after awaiting
/// (or never starting) them. A violation is detected — it is not a race — and
/// the call throws `ContentionException` naming the type and the member.
///
/// The one thing that could hide such a call is a handle reachable from
/// another isolate, and none is: a handle cannot be sent (it is `Finalizable`,
/// and the actor variant is bound to its host), which is the same reason
/// `dispose()` is sound.
final class Consumed<T extends Consumable> {
  /// The handle this token will spend. Generated code reads it to write an
  /// impl tag or to compare acquisitions; an application has no use for it.
  final T handle;

  bool _spent = false;

  Consumed(this.handle);

  /// Whether the object has been given up. False after an encode that threw,
  /// which is what lets a generated actor consume distinguish "the call went
  /// out, release the executor" from "nothing happened".
  bool get isSpent => _spent;

  /// The raw for the wire, with the object still ours. **Generated code only.**
  int peekFor(String member) => handle.peekHandle(member);

  /// Give the object up. **Generated code only.** Idempotent, so a token that
  /// somehow reaches two spends does not double-detach.
  void spend(String member) {
    if (_spent) return;
    _spent = true;
    handle.spendHandle(member);
  }
}
