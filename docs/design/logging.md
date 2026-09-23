# Rust `log` → Dart

`frustrate::logging::install` forwards `log` records to a Dart stream. The
module docs in `runtime/rust/src/logging.rs` cover usage, the `log` build
setting, re-entrancy and dropped-record counting. This page covers what a
portable app has to know about web actors, and the designs that were rejected.

## Web actors log separately

`log`'s logger is a `static`. Natively there is one per process, so one install
covers every thread, actor threads included. On web an actor is a separate wasm
instance with its own memory and its own logger, so an actor whose code logs
needs its own install, through a method on the actor.

The same source therefore behaves differently on the two platforms:

- **Installing from an actor.** Natively this replaces the process logger and
  closes the previously installed stream. On web the two installs are
  independent and both streams stay open.
- **Disposing that actor.** On web the sink dies with the worker's memory, so
  the actor's logging stream fails with an attributable `StateError`. Natively
  the process-global logger keeps the sink, and the stream stays open until
  `uninstall()`.

A portable app feeds every logging stream into one handler, treats `onDone` as
benign, and cancels an actor's subscription before disposing it. Hiding the
difference would mean refusing the per-actor install natively, which breaks
that recipe, or fanning out to a set of sinks, which is a second registration
model for a case one line of Dart handles.

## Rejected designs

**A default logger installed at web init.** It would take `log`'s one global
slot without being asked, which this module refuses to do natively. And the
only place a default could write is `eprintln!`, which reaches the console only
under a facility std, so it would not even be loud.

**Refusing, at codegen, an actor that logs.** Undecidable: a `log!` can sit
anywhere in the crate graph.

**A `From<&log::Record>` impl instead of a mapping closure.** The orphan rule
stops an app writing that impl. A closure is also the honest description: it
shapes the record, it is not a conversion the type system owes anyone.

**Generated logging.** Codegen never expands macros, so a macro that emits
bridge items is out. Generating the source with a declared build action was
possible but rejected: the action would have to invent a record type, a
function that opens the stream, and a Dart surface, all to save the user a
six-line struct describing what they want to see.

**A `tracing` bridge.** `tracing` has spans and structured fields that a
record-shaped sink flattens badly, and `tracing-log` already converts in both
directions.

**Replacing `println!`.** The stdio facility still serves what this cannot:
output from before any Dart code has run, and from a panic path, where there is
no stream to post on.
