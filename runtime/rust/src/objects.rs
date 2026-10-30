//! The executor's object table: every object an actor's executor hosts.
//!
//! An actor's executor is a single-threaded world. Each object it hosts is one
//! entry here, and a Dart handle to one carries the entry's **slot id**, never
//! a pointer. A call names its object by slot; the executor looks it up, and
//! the lookup is the only way from a slot to an object.
//!
//! The table is thread-local. Natively it belongs to the actor's host thread
//! (actor.rs), the only thread that runs that actor's calls. On web it belongs
//! to the actor's worker instance, which is single-threaded, so the
//! thread-local there is the instance's table. A lookup from any other thread
//! sees a different, empty table and fails loudly.
//!
//! What the table guarantees, and how:
//!
//! - **A stale or foreign slot misses loudly.** Slot ids come from one counter
//!   that only increments, so an id is never reused: a slot whose object has
//!   been dropped or taken cannot come to name a newer object. Natively the
//!   counter is process-wide, so a slot minted on another executor misses too;
//!   on web each instance has its own counter, and the Dart wrapper, which
//!   records its host, is what routes a slot to the executor that minted it.
//! - **A slot is type-checked on every lookup.** An entry records its type's
//!   `TypeId`, and a lookup for any other type panics naming both. The id is
//!   stored rather than asked of the `dyn Any`, because asking would make a
//!   reference to the object, which a live `&mut T` to it forbids.
//! - **Changing the table cannot invalidate a live borrow.** Entries hold raw
//!   pointers from `Box::into_raw`, and the map moves only those pointers. A
//!   `&mut T` the executor handed to a running call points into its own
//!   allocation, which no insert, removal or rebalance touches, and a lookup
//!   copies the raw pointer rather than reborrowing through an owner. No
//!   `Box<T>`, whose uniqueness every use asserts, exists while the object is
//!   in the table. `cargo miri test -p frustrate --lib objects` checks the
//!   tests below under Stacked Borrows.
//! - **No user code runs under the table's borrow.** A removed entry is
//!   returned and dropped by the caller, after the borrow ends, so a `Drop`
//!   that touches the table (by minting or dropping another entry) finds it
//!   free.
//! - **Teardown cannot miss an entry.** [`drain`] removes everything still in
//!   the table, newest first, and drops it as a leak: any channel the object
//!   still holds ends as *leaked*, naming its type. An object reaches this only
//!   when nothing disposed it.

use std::any::{Any, TypeId};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// Slot ids, never reused. Starts at 1 so a zero slot is never valid.
static NEXT_SLOT: AtomicU64 = AtomicU64::new(1);

/// One hosted object.
struct Entry {
    /// From `Box::<T>::into_raw`. Owned by the table while the entry is in it;
    /// dropping it as a `Box<dyn Any>` frees it as the `Box<T>` it came from.
    ptr: *mut dyn Any,
    type_id: TypeId,
    /// The Rust type, for a mismatch diagnostic.
    rust_type: &'static str,
    /// The bridged type name, for the leak diagnostic.
    #[cfg_attr(target_family = "wasm", allow(dead_code))]
    label: &'static str,
}

thread_local! {
    /// The slot of this executor's root: the actor object its constructor
    /// built, which an `Addr`'s posted closures run on. Zero until then. A
    /// slot id is never reused, so once the root leaves the table this names
    /// nothing for good.
    static ROOT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// Keyed by slot id. A `BTreeMap` because ids increase with minting, so
    /// iteration order is minting order and [`drain`] can go newest first.
    static TABLE: RefCell<BTreeMap<u64, Entry>> = const { RefCell::new(BTreeMap::new()) };
}

/// Put `value` in this executor's table and return its slot id.
///
/// `label` is the bridged type name, which the leak diagnostic uses if the
/// object is reached only by [`drain`].
pub(crate) fn insert<T: 'static>(value: T, label: &'static str) -> u64 {
    let slot = NEXT_SLOT.fetch_add(1, Ordering::Relaxed);
    let entry = Entry {
        ptr: Box::into_raw(Box::new(value)),
        type_id: TypeId::of::<T>(),
        rust_type: std::any::type_name::<T>(),
        label,
    };
    TABLE.with(|t| t.borrow_mut().insert(slot, entry));
    slot
}

/// Put `value` in this executor's table as its root, and return its slot id.
///
/// An executor has one root: each actor constructor spawns its own executor.
/// Panics on a second.
pub(crate) fn insert_root<T: 'static>(value: T, label: &'static str) -> u64 {
    let slot = insert(value, label);
    ROOT.with(|r| {
        assert!(
            r.get() == 0,
            "frustrate: a second actor root on one executor — bridge bug"
        );
        r.set(slot);
    });
    slot
}

/// This executor's root, if it is a `T` still in the table. `None` before the
/// constructor has returned, and once the root has been dropped or taken.
///
/// Panics if the root is another type: an `Addr` names its actor's type, and
/// generated glue builds it for the actor whose executor this is.
pub(crate) fn root<T: 'static>() -> Option<*mut T> {
    let slot = ROOT.with(|r| r.get());
    if slot == 0 {
        return None;
    }
    TABLE.with(|t| match t.borrow().get(&slot) {
        Some(e) if e.type_id == TypeId::of::<T>() => Some(e.ptr as *mut T),
        Some(e) => mismatch::<T>(slot, e.rust_type),
        None => None,
    })
}

/// The object at `slot`, which must be a `T`.
///
/// Panics if this executor's table has no such slot (stale, already dropped or
/// taken, minted by another executor, or looked up off the executor's thread)
/// or if the slot holds another type. Either is a bridge bug, not a user error:
/// the Dart wrapper refuses a disposed handle before it sends anything.
pub(crate) fn get<T: 'static>(slot: u64) -> *mut T {
    TABLE.with(|t| match t.borrow().get(&slot) {
        Some(e) if e.type_id == TypeId::of::<T>() => e.ptr as *mut T,
        Some(e) => mismatch::<T>(slot, e.rust_type),
        None => missing(slot),
    })
}

/// Take the object at `slot` out of the table: the caller owns it now.
///
/// Panics as [`get`] does. A slot of the wrong type is left in the table.
pub(crate) fn remove<T: 'static>(slot: u64) -> Box<T> {
    let ptr = TABLE.with(|t| {
        let mut t = t.borrow_mut();
        match t.get(&slot) {
            Some(e) if e.type_id == TypeId::of::<T>() => {}
            Some(e) => mismatch::<T>(slot, e.rust_type),
            None => missing(slot),
        }
        t.remove(&slot).expect("checked above").ptr
    });
    unsafe { Box::from_raw(ptr as *mut T) }
}

/// Remove and drop every entry still in this executor's table, newest first.
///
/// Each drop runs as a finalization (`stream::finalize_scope`), so a channel
/// the object still holds ends as *leaked* naming its type: being here means
/// nothing disposed it. A panicking drop is reported to the panic listener and
/// the drain goes on, so one bad `Drop` cannot strand the rest.
///
/// Entries minted by a drop while the drain runs are drained too.
///
/// Native only in use: on web an actor's instance ends with its Worker, and
/// terminating the Worker reclaims the instance's whole memory.
#[cfg_attr(target_family = "wasm", allow(dead_code))]
pub(crate) fn drain() {
    while let Some(entry) = TABLE.with(|t| t.borrow_mut().pop_last().map(|(_, e)| e)) {
        let run =
            || crate::stream::finalize_scope(entry.label, || drop(unsafe { Box::from_raw(entry.ptr) }));
        if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
            crate::panic::observed(crate::panic::message(&*payload));
        }
    }
}

/// This executor's root slot (0 before one is built).
#[cfg(test)]
pub(crate) fn root_slot() -> u64 {
    ROOT.with(|r| r.get())
}

/// How many objects this executor's table holds.
#[cfg(test)]
pub(crate) fn len() -> usize {
    TABLE.with(|t| t.borrow().len())
}

#[cold]
fn missing(slot: u64) -> ! {
    panic!(
        "frustrate: slot {slot} is not in this executor's object table: it was \
         dropped or taken already, or it belongs to another executor — bridge bug"
    )
}

#[cold]
fn mismatch<T>(slot: u64, held: &str) -> ! {
    panic!(
        "frustrate: slot {slot} holds a `{held}`, not a `{}` — bridge bug",
        std::any::type_name::<T>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn panic_message(f: impl FnOnce()) -> String {
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
            .expect_err("expected a panic");
        crate::panic::message(&*payload).to_string()
    }

    #[test]
    fn a_slot_names_its_object_until_it_is_removed() {
        let slot = insert(41i64, "N");
        unsafe { *get::<i64>(slot) += 1 };
        assert_eq!(unsafe { *get::<i64>(slot) }, 42);
        assert_eq!(*remove::<i64>(slot), 42);
        assert!(panic_message(|| {
            get::<i64>(slot);
        })
        .contains(&format!(
            "slot {slot} is not in this executor's object table"
        )));
    }

    /// A slot id is never reused: a removed slot keeps missing after newer
    /// objects are minted, rather than coming to name one of them.
    #[test]
    fn a_stale_slot_misses_after_newer_objects_are_minted() {
        let stale = insert(1u8, "A");
        drop(remove::<u8>(stale));
        let fresh: Vec<u64> = (0..8).map(|i| insert(i as u8, "A")).collect();
        assert!(!fresh.contains(&stale), "a slot id was reused");
        assert!(panic_message(|| {
            get::<u8>(stale);
        })
        .contains("is not in this executor's object table"));
        for s in fresh {
            drop(remove::<u8>(s));
        }
    }

    /// A slot looked up as another type panics naming both, and stays in the
    /// table: the failed lookup takes nothing.
    #[test]
    fn a_slot_of_another_type_panics_and_is_kept() {
        let slot = insert(String::from("kept"), "S");
        let msg = panic_message(|| {
            get::<u32>(slot);
        });
        assert!(
            msg.contains("holds a `alloc::string::String`, not a `u32`"),
            "{msg}"
        );
        let msg = panic_message(|| {
            remove::<u32>(slot);
        });
        assert!(msg.contains("not a `u32`"), "{msg}");
        assert_eq!(*remove::<String>(slot), "kept");
    }

    /// Another thread's table is not this one: a slot minted here misses
    /// there. This is the native executor's confinement, checked loudly.
    #[test]
    fn a_slot_misses_on_another_thread() {
        let slot = insert(7u16, "T");
        let msg = std::thread::spawn(move || {
            panic_message(|| {
                get::<u16>(slot);
            })
        })
        .join()
        .unwrap();
        assert!(
            msg.contains("is not in this executor's object table"),
            "{msg}"
        );
        drop(remove::<u16>(slot));
    }

    /// The Stacked Borrows property the raw-pointer entries exist for: a
    /// `&mut T` handed to a running call stays valid while the table grows
    /// and shrinks under it.
    #[test]
    fn a_live_borrow_survives_inserts_and_removals() {
        let slot = insert(vec![1u32], "V");
        let live: &mut Vec<u32> = unsafe { &mut *get::<Vec<u32>>(slot) };
        let others: Vec<u64> = (0..64).map(|i| insert(i as u64, "O")).collect();
        live.push(2);
        for s in &others[..32] {
            drop(remove::<u64>(*s));
        }
        live.push(3);
        assert_eq!(*live, [1, 2, 3]);
        for s in &others[32..] {
            drop(remove::<u64>(*s));
        }
        assert_eq!(*remove::<Vec<u32>>(slot), [1, 2, 3]);
    }

    /// Zero-sized objects get distinct slots, and each is its own entry.
    #[test]
    fn zero_sized_objects_have_distinct_slots() {
        struct Unit;
        let a = insert(Unit, "U");
        let b = insert(Unit, "U");
        assert_ne!(a, b);
        drop(remove::<Unit>(a));
        get::<Unit>(b);
        drop(remove::<Unit>(b));
    }

    /// Drain drops newest first, survives a panicking drop, and drains what a
    /// drop mints while it runs.
    #[test]
    fn drain_drops_newest_first_through_a_panic_and_what_drops_mint() {
        // Its own thread, so the table starts empty. The panicking drop is
        // reported to the process's panic listener, so this holds the lock
        // the tests that record that listener hold.
        let _serial = crate::post::test_lock();
        std::thread::spawn(|| {
            static ORDER: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());
            static LATE: AtomicUsize = AtomicUsize::new(0);
            struct Noted(u32);
            impl Drop for Noted {
                fn drop(&mut self) {
                    ORDER.lock().unwrap().push(self.0);
                    if self.0 == 2 {
                        panic!("entry 2 exploded");
                    }
                    if self.0 == 3 {
                        // A drop that mints: the drain must reach this too.
                        struct Late;
                        impl Drop for Late {
                            fn drop(&mut self) {
                                LATE.fetch_add(1, Ordering::SeqCst);
                            }
                        }
                        insert(Late, "Late");
                    }
                }
            }
            for i in 1..=3 {
                insert(Noted(i), "Noted");
            }
            drain();
            assert_eq!(*ORDER.lock().unwrap(), [3, 2, 1]);
            assert_eq!(
                LATE.load(Ordering::SeqCst),
                1,
                "a minted-during-drain entry leaked"
            );
            assert_eq!(len(), 0);
        })
        .join()
        .unwrap();
    }
}
