# Seams

A seam is a place where user code plugs into the bridge for a concern that
belongs to no single bridged member: faking the library in tests, intercepting
calls, hearing panics, forwarding `log` records, lending the executor a tokio
context. Each seam is named for its concern and documented where it lives.

## Reporting an uncaught failure

Wiring a crash reporter takes two registrations, because failures divide by
what they carry.

- **A Rust panic** goes to `frustrate::panic::register`. Its location, its
  backtrace, and whether it killed a thread instead of answering a call never
  cross the wire, so only a Rust-side listener can report them.
- **Anything Dart did not catch** reaches whatever the app already installed.
  An unhandled failure goes to the zone that issued the call, which is what
  `runZonedGuarded` observes and what Flutter routes to
  `PlatformDispatcher.onError`. frustrate adds no catch-all of its own; it
  would be a second place to register the same reporter.

Generated async bindings rethrow a failure with their own stack, so a report
names the member. To get the app's call site too, install a
`DelegatingRuntime` that captures the stack when the call is issued; the
binding keeps that stack rather than replacing it.

**A returned `Err` is not an incident.** Typed, string or `ContentionError`,
it is a declared outcome: the signature said the call could fail, and it did.
Sending those to an incident tracker is noise, and a noisy integration is one
somebody switches off. The untyped tier is where an author may reasonably
disagree, since `anyhow`'s `bail!` is how Rust says "something unexpected
happened". That call is the author's, and Dart is where they make it: an `Err`
nobody catches is an uncaught failure and is reported as one.

## What is refused

**No user code injected into generated output.** No startup hook running a
Rust function nobody called, no Dart spliced into a generated class, no file
preambles. What a generated member does must be a property of the source that
produced it, the same rule that keeps configuration at the declaration site
([configuration.md](configuration.md)). To run something at startup, call it.

**No single handler object.** Naming each concern separately lets each state
its own contract and limits. One trait broad enough to cover them all could not
be kept stable.

**No Rust-side error listener.** A returned `Err` is a declared outcome, so it
is the wrong set to report. And it crosses to Dart intact, so a Rust listener
would hear nothing that Dart does not already hold.

**The async executor is not replaceable.** Lending it a runtime context is
supported; substituting another executor is not. The cooperative executor is
what makes Rust `async fn` work on single-threaded stable wasm
([async_fn.md](async_fn.md)), and a seam that replaced it would let an embedder
retract the portability the charter commits to.

**Rust-owned UI state is not a codegen feature.** Generating a state container
and the widget that rebuilds on its changes would put `package:flutter` in the
runtime's dependency graph, which the pure-Dart test harnesses rely on it
staying out of. A stream and a `ValueListenable` express it without codegen.
