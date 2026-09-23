//! Rust handles to Dart objects — the dual of opaque handles.
//!
//! An opaque handle lets Dart hold a Rust object and call its methods. These
//! are the mirror image: a Rust handle to a *Dart* object whose methods Rust
//! can call. The two together are the whole of frustrate's object interop.
//!
//! Each type here mirrors a real Dart type, and the module path mirrors the
//! Dart library it comes from — `dart::core::Sink` is `dart:core`'s `Sink`,
//! `dart::r#async::EventSink` is `dart:async`'s. (`r#async` because `async`
//! is a Rust keyword; the escape is the price of naming the library
//! honestly.) The Dart side passes an ordinary object implementing that
//! interface — a `StreamController`, a class of the user's own — and nothing
//! frustrate-specific.
//!
//! **A handle is data.** It carries its Dart-minted id inline, on the wire
//! where the value sits, so it composes exactly like any other type: nested
//! in a struct, in a `Vec`, in an `Option`, several per function, alongside a
//! real return value. That is the whole point of modelling endpoints this
//! way — composition falls out of the ordinary recursive codecs instead of
//! being special-cased.
//!
//! **Direction.** Handles are argument-only (checker rule FR0031): Rust
//! cannot mint a Dart object, so a handle can only travel Dart → Rust.
//!
//! **Lifetime.** The Dart object is kept alive by the router's map entry,
//! rooted by the long-lived transport. The last clone of the Rust handle
//! dropping posts one terminal, which retires that entry. No persistent
//! handles and no GC finalizers on either side — see `stream.rs`.
//!
//! # Which mirror to take
//!
//! | Rust type | Dart argument | Rust can | Dart cancel | Backpressure |
//! |---|---|---|---|---|
//! | [`core::Sink<T>`] | any `Sink<T>` | `add` | no | no |
//! | [`r#async::EventSink<T>`] | any `EventSink<T>` | `add`, `add_error` | no | no |
//! | [`r#async::StreamController<T>`] | a `StreamController<T>` | `add`, `add_error`, `send().await` | **yes** | **yes** |
//!
//! Cancellation is why [`r#async::StreamController`] exists as a separate
//! mirror rather than everything taking the tightest write-end interface:
//! `Sink` and `EventSink` are write-end *only*, so handed one, frustrate has
//! no way to learn the consumer stopped caring — or paused. A
//! `StreamController` also exposes `onCancel`/`onPause`/`onResume`, which the
//! generated Dart binds — so the user's cancel handle is the ordinary
//! `StreamSubscription` they already hold from `controller.stream.listen(...)`,
//! and `send().await` parks on their pause. There is no frustrate type to
//! learn.
//!
//! Take the tightest one that does the job; take `StreamController` when the
//! producer should stop early if the consumer walks away.

use crate::codec::ByteWriter;
use crate::stream::{selector, Inner};
use std::sync::Arc;

/// The shared core of every mirror: the id, the cancel flag, the
/// terminal-once accounting (all in [`Inner`]), plus how to encode `T`.
///
/// Every mirror is a thin wrapper over this. They differ only in which
/// methods they expose — which is exactly the difference between the Dart
/// interfaces they mirror, and the reason they are distinct Rust types
/// rather than one type with a superset of methods.
struct Chan<T> {
    inner: Arc<Inner>,
    /// By value: an item that reaches an opaque handle is minted here, and
    /// minting takes the object. See `crate::stream::StreamSink`.
    encode: fn(T, &mut ByteWriter),
}

impl<T> Clone for Chan<T> {
    fn clone(&self) -> Self {
        Chan {
            inner: Arc::clone(&self.inner),
            encode: self.encode,
        }
    }
}

impl<T> Chan<T> {
    fn new(id: u64, encode: fn(T, &mut ByteWriter)) -> Self {
        Chan {
            inner: crate::stream::end(id),
            encode,
        }
    }

    fn add(&self, item: T) -> bool {
        // Liveness before the encode, for `StreamSink::add`'s reason: a closed
        // channel must not mint what it will not send, and dropping `item`
        // whole runs the object's own `Drop`.
        if !self.inner.live() {
            return false;
        }
        let mut w = crate::stream::item_writer(selector::ADD);
        (self.encode)(item, &mut w);
        crate::stream::post_method(&self.inner, w)
    }

    fn add_error(&self, msg: impl std::fmt::Display) -> bool {
        let mut w = crate::stream::item_writer(selector::ADD_ERROR);
        w.write_string(&msg.to_string());
        crate::stream::post_method(&self.inner, w)
    }
}

/// Mirrors of `dart:core` types.
pub mod core {
    use super::Chan;
    use crate::codec::ByteWriter;

    /// `dart:core`'s [`Sink<T>`]: the tightest write end — `add` and the
    /// implicit close. Any Dart object implementing `Sink<T>` works, so a
    /// caller can pass `controller.sink` or a class of their own.
    ///
    /// No cancellation: a `Sink` is write-end only, so nothing here can learn
    /// that the consumer stopped caring. Take
    /// [`super::r#async::StreamController`] when that matters.
    ///
    /// [`Sink<T>`]: https://api.dart.dev/stable/dart-core/Sink-class.html
    pub struct Sink<T>(Chan<T>);

    impl<T> Clone for Sink<T> {
        fn clone(&self) -> Self {
            Sink(self.0.clone())
        }
    }

    impl<T> Sink<T> {
        /// `sink.add(item)`. Returns false — without sending — once the
        /// channel is closed, so a stale handle is inert, never a panic.
        pub fn add(&self, item: T) -> bool {
            self.0.add(item)
        }

        /// Close the Dart sink now, even with clones alive. Equivalent to
        /// dropping the last clone; posting twice is impossible (the
        /// terminal is taken once — see `Inner::take_terminal`).
        pub fn close(self) {
            self.0.inner.close_now();
        }
    }

    /// Construct the handle for a decoded id. Called by generated glue,
    /// which supplies the item encoder.
    pub fn sink<T>(id: u64, encode: fn(T, &mut ByteWriter)) -> Sink<T> {
        Sink(Chan::new(id, encode))
    }
}

/// Mirrors of `dart:async` types.
pub mod r#async {
    use super::Chan;
    use crate::codec::ByteWriter;

    /// `dart:async`'s [`EventSink<T>`]: [`super::core::Sink`] plus
    /// `addError`. Still write-end only — no cancellation.
    ///
    /// [`EventSink<T>`]: https://api.dart.dev/stable/dart-async/EventSink-class.html
    pub struct EventSink<T>(Chan<T>);

    impl<T> Clone for EventSink<T> {
        fn clone(&self) -> Self {
            EventSink(self.0.clone())
        }
    }

    impl<T> EventSink<T> {
        /// `sink.add(item)`.
        pub fn add(&self, item: T) -> bool {
            self.0.add(item)
        }

        /// `sink.addError(error)` — **non-terminal**. Dart's contract is that
        /// a stream may carry errors and continue, so the channel stays open
        /// and `add` keeps working. The terminal form is
        /// [`crate::StreamSink::error`].
        pub fn add_error(&self, msg: impl std::fmt::Display) -> bool {
            self.0.add_error(msg)
        }

        /// `sink.close()`. See [`super::core::Sink::close`].
        pub fn close(self) {
            self.0.inner.close_now();
        }
    }

    /// Construct the handle for a decoded id (generated glue).
    pub fn event_sink<T>(id: u64, encode: fn(T, &mut ByteWriter)) -> EventSink<T> {
        EventSink(Chan::new(id, encode))
    }

    /// `dart:async`'s [`StreamController<T>`]: the full producer end —
    /// `add`, `addError`, `close`, **and** the `onCancel` back-channel.
    ///
    /// The generated Dart binds the passed controller's `onCancel` (throwing
    /// if the caller already set one — frustrate owns it for a controller
    /// handed across), so when the consumer cancels their subscription
    /// [`is_cancelled`](Self::is_cancelled) flips and every send goes inert.
    /// That is the whole reason to prefer this over the write-end mirrors.
    ///
    /// This is the mirror the legacy `StreamSink<T>` names, and the one the
    /// generated `Stream<T>`-returning members are built on.
    ///
    /// [`StreamController<T>`]: https://api.dart.dev/stable/dart-async/StreamController-class.html
    pub type StreamController<T> = crate::stream::StreamSink<T>;

    /// Construct the handle for a decoded id (generated glue).
    pub fn stream_controller<T>(
        id: u64,
        encode: fn(T, &mut ByteWriter),
    ) -> StreamController<T> {
        crate::stream::sink(id, encode)
    }
}
