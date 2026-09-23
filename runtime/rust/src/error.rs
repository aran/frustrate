//! Contract-violation errors. Loud, deterministic, attributable.

use std::fmt;

/// Why a call could not have the object it asked for. One type, two
/// contracts — both are "something else was using it", and both reach Dart as
/// `ContentionException` — but the remedy differs and a caller acts on the
/// remedy, so the text must not be shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Contention {
    /// A sync call declared `on_contention = "error"` found the lock held.
    Lock,
    /// A consuming call could not take the object because a call on it had
    /// not finished.
    Take,
}

/// Thrown (as a distinct envelope status) when a call cannot have the object:
/// a contended lock under `on_contention = "error"`, or a consume with a call
/// still in flight.
#[derive(Debug)]
pub struct ContentionError {
    pub type_name: &'static str,
    pub method: &'static str,
    pub kind: Contention,
}

impl fmt::Display for ContentionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            Contention::Lock => write!(
                f,
                "contention on locked type `{}` in sync call `{}`: this call is declared \
                 `on_contention = \"error\"`, whose contract converts a contended lock into \
                 this error instead of blocking. Retry, or use the async form to wait safely.",
                self.type_name, self.method
            ),
            // Deliberately not "retry": the token is spent, so there is
            // nothing left to retry *with*. Saying what happened to the object
            // is the only useful thing here, because it is the one question a
            // caller cannot answer from the handle.
            Contention::Take => write!(
                f,
                "`{}` consumes the `{}` it was given, and a call on that object had not \
                 finished when it ran, so the object could not be taken. It has been \
                 released — the in-flight call still completes, and the object is dropped \
                 when it does. `take()` requires that every call on the handle has been \
                 awaited (or never started) before it is passed.",
                self.method, self.type_name
            ),
        }
    }
}

impl std::error::Error for ContentionError {}
