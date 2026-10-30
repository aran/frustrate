//! Who hears a panic that no call is waiting on.
//!
//! A closure an actor posts to itself ([`crate::addr::Addr::post`]) and a
//! detached task ([`crate::runtime::spawn`]) answer no call. When one panics,
//! Dart hears it as an uncaught error in the zone the work belongs to: an
//! actor's work belongs to the zone its Dart handle was built in, and other
//! work to the zone frustrate was initialised in. This module carries that
//! owner from where the work is queued to where it panics.
//!
//! An owner is an id on the isolate's port, with the isolate in its high bits
//! like every call id ([`crate::post`]). An actor's owner is an id Dart
//! allocated for it and passed to `frustrate_actor_spawn`. An isolate's own
//! owner is its tag with a zero sequence, which no call ever uses. Zero is no
//! owner: the panic reaches the panic listener ([`crate::panic`]) and no Dart.
//!
//! Natively the owner is captured when the work is queued:
//!
//! - on an actor's host thread, it is that actor;
//! - inside a task the executor is polling, it is that task's owner, and a
//!   call's task is owned by its actor or else by its isolate;
//! - inside a pool job or a sync call, it is the call's isolate (Dart passes
//!   a sync call its isolate's tag);
//! - on a thread frustrate does not own, outside any bridge call, there is
//!   none.
//!
//! On web an instance reports its own traps, so each actor worker and the
//! page already know whose a trap with no call is, and owners stay zero.

use std::cell::Cell;

thread_local! {
    /// The owner of work queued from this thread right now. Zero when no
    /// frustrate frame has said.
    static OWNER: Cell<u64> = const { Cell::new(0) };
}

/// The owner of work queued from here.
pub(crate) fn current() -> u64 {
    OWNER.with(Cell::get)
}

/// The owner of a call's own task, given the owner in force where it was
/// spawned: an actor's deferred completion belongs to the actor, and any other
/// call to the isolate that made it.
pub(crate) fn of_call(call_id: u64) -> u64 {
    let owner = OWNER.with(Cell::get);
    if owner != 0 {
        return owner;
    }
    #[cfg(not(target_family = "wasm"))]
    {
        call_id & !((1u64 << crate::post::ISOLATE_SHIFT) - 1)
    }
    #[cfg(target_family = "wasm")]
    {
        let _ = call_id;
        0
    }
}

/// Run `f` with `owner` as the owner of what it queues, and restore the
/// previous owner after, on unwind too.
pub(crate) fn with<R>(owner: u64, f: impl FnOnce() -> R) -> R {
    struct Restore(u64);
    impl Drop for Restore {
        fn drop(&mut self) {
            OWNER.with(|c| c.set(self.0));
        }
    }
    let _restore = Restore(OWNER.with(|c| c.replace(owner)));
    f()
}

/// Make `owner` the owner of everything this thread queues from now on. For an
/// actor's host thread, which is that actor's for its whole life.
#[cfg(not(target_family = "wasm"))]
pub(crate) fn enter(owner: u64) {
    OWNER.with(|c| c.set(owner));
}

/// Tell `owner`'s zone about a panic that answered no call. `reply` is the
/// panic envelope, already reported to the panic listener when it was built
/// ([`crate::envelope::panic_envelope`]).
pub(crate) fn report(owner: u64, reply: crate::codec::FramedWriter) {
    #[cfg(not(target_family = "wasm"))]
    if owner != 0 {
        crate::post::report_uncaught(owner, reply);
        return;
    }
    let _ = owner;
    // A panic envelope is a string and mints nothing.
    reply.into_parts().1.delivered();
}
