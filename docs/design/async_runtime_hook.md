# Async-runtime hook

`frustrate::runtime::register` gives frustrate's executor the context of a
runtime the app supplies, such as tokio. The context is entered around every
poll, so a bridged `async fn` can `.await` a tokio leaf directly. It is
native-only. How to use it, and what the registrant must uphold, is documented
on `register` in `runtime/rust/src/runtime.rs`.

## Enter the runtime; do not hand it the future

frustrate keeps driving the task and enters the runtime's context only for the
length of each poll. The leaf registers with tokio's reactor. When the reactor
fires, it calls the leaf's waker, which is frustrate's waker, and the task goes
back on frustrate's run queue. tokio decides when the leaf is ready; frustrate
drives the task.

The alternative was a pluggable `spawn` that gives each future to the runtime.
That creates a second drive path. On it, cancellation, panic attribution and
task lifetime would follow tokio's rules instead of frustrate's, and there
would have to be a rule for which path a given `async fn` takes. Entering the
context keeps one drive path. Cancelling still means dropping frustrate's task,
no pool thread is parked, and a panic is still attributed to its call.

## A closure, not a `Handle`

Taking a `tokio::runtime::Handle` would make tokio a dependency of the runtime
crate, and so of every bridge, including bridges that never use tokio. It
cannot be an off-by-default feature under `crate.from_cargo`. A trait that
returns a guard does not type-check either: tokio's `EnterGuard<'_>` borrows the
`Handle`, so it cannot be returned as a `'static` box, and the orphan rule stops
an app from implementing frustrate's trait for tokio's type. A closure that
receives the poll avoids both problems: the guard's borrow ends when the
closure returns, and the runtime crate names no async runtime.

## Web has no equivalent

tokio's reactor is background OS threads plus epoll or kqueue, and
`wasm32-unknown-unknown` has neither, so there is nothing to enter.
Much of tokio compiles for wasm, so a link error would not stop the hook being
used there. That is why `register` is left out of wasm builds with `cfg`.
Async work on web that needs an outside wake has a waker that schedules a
re-poll from JS. It needs no runtime and no hook.
