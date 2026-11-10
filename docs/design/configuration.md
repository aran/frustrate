# Choosing defaults

The rule for every place frustrate picks a mapping, representation or
behaviour:

1. **The default is the most convenient, idiomatic Dart mapping — unless it can
   be wrong.** A default may never lose data, leak, or be undefined behaviour
   without an error.
2. **If there is more than one reasonable choice, make it configurable**, with
   the safe, convenient option as the default. Do not pick one and hide the
   other.
3. **If the convenient option can be wrong, it is not the default.** Make the
   safe representation the default and offer the convenient one as an opt-in,
   or refuse with a diagnostic and make the author choose.

The test: *if the author does nothing, can they get a wrong result, a leak, or
UB without an error?* If yes, the default is wrong.

## Where a choice is written

A choice is written where the construct is declared, in the `#[bridge(...)]`
attribute, so the generated surface is a pure function of the source.

**A type-mapping choice is written as the type in the signature.** The glue
calls the author's own `fn`, so the type it rebuilds must be the one in the
signature; an attribute could only repeat or contradict it. An attribute is
also per-item, and cannot reach two parameters that want different mappings, or
a struct field, which cannot carry `#[bridge(...)]` at all. Codegen's job is to
refuse when the spelling is ambiguous. Error handling follows this: the `E` in
`Result<T, E>` decides whether a failure crosses as a message or as a typed
value, with no marker to forget.

## Examples

- **Data-class equality.** Structural `==`/`hashCode` is the default, because
  identity equality breaks map keys, `Set` and `expect()` without an error.
  `no_eq` opts out.
- **`Option<Option<T>>`.** `Option<T>` maps to `T?`, but nested options would
  collapse `Some(None)` and `None` into one `null`, so they get a wrapper class.
- **`u64` is always `BigInt`.** Reinterpreting as a signed `int` would turn
  values above 2^63 into negative numbers; a runtime range check would fail
  only for certain data, on values the app author cannot fix without changing
  the Rust type. So there is no `int` option: an author who wants `int` writes
  `i64`.
- **Names that collide with the wire protocol or handle surface** (a parameter
  named `call_id`, a method named `dispose`) have no safe default, so they are
  refused and the fix is a rename.
- **Time types.** The Rust peer (`std::time`, `chrono`, `time`) is selectable
  by writing its type, because projects already use one; the Dart side and the
  wire are the same for all three. Precision is fixed at microseconds, which is
  what Dart's `DateTime` and `Duration` hold. Time zones are not configurable:
  the wire carries an instant, and local time is converted for display on the
  Dart side, where every platform has a real time-zone database.

## Refused

What frustrate declines to offer, so that it is not re-proposed without a new
reason:

- **User code injected into generated output**: no startup hook, no Dart spliced
  into a generated class, no file preambles. What a generated member does is a
  property of its source. To run something at startup, call it.
- **One handler object for every seam.** Each seam is named for its concern and
  states its own contract.
- **A Rust-side listener for returned `Err`s.** They are declared outcomes and
  reach Dart intact.
- **A replaceable async executor.** Lending it a runtime context is supported;
  replacing it would let an embedder retract portability to single-threaded
  stable wasm.
- **Rust-owned UI state as a codegen feature.** It would put `package:flutter`
  in the runtime's dependencies; a stream and a `ValueListenable` express it.
- **A member returning `Stream<T>`, or an `impl Stream` return.** A sink
  parameter is the one shape: the caller owns the controller, and frustrate
  owns no background poller.
- **An owned upcast** of a concrete handle to a long-lived `Box<dyn Trait>`
  handle while the concrete one keeps working.
