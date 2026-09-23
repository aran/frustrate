//! Gate: a hostile request must never reach undefined behaviour.
//!
//!     cargo +<pin> miri test -p test_api --lib hostile
//!
//! The Dart side of this bridge is generated and type-safe, but the *wire* is
//! bytes, and a member's glue meets those bytes before anything checks them.
//! This drives requests a caller could actually send at the real dispatch
//! entry — `frustrate_call_sync`, the same `#[no_mangle]` the transport calls —
//! and lets Miri decide whether the glue did anything undefined.
//!
//! The requests are derived from the interface, not written by hand: the
//! member list, the parameter types, the wire layout and the `fn_id`s all come
//! out of the IR `build.rs` wrote beside the glue it generated from the same
//! parse. A member added to `api.rs` is attacked on the next run without
//! anyone remembering to add it, which is the whole point — the two defects
//! this exists to have caught were both shapes nobody had thought to write.
//!
//! # Miri's default flags are the instrument
//!
//! Stacked Borrows is what reports the confined aliasing defect: two
//! references over one `Box`, one of them `&mut`, live at once. Run it with
//! `-Zmiri-tree-borrows` and that class goes unreported. Leak checking is on,
//! and every handle this mints is reclaimed, so an unreclaimed one is a
//! finding too.
//!
//! # What it attacks, and why exactly these
//!
//! An input is in scope when the runtime or the generated glue **makes a claim
//! about it**. That is the line, and it is what keeps this a gate rather than
//! a fishing trip:
//!
//!   * **One handle in two positions that can both accept it.** Dart-reachable
//!     with nothing unusual — the caller passes the same object twice — and
//!     claimed against on both sides: `handle::alias_check` refuses a confined
//!     duplicate where either borrow is mutable, `handle::lock_plan` refuses a
//!     locked duplicate outright. Positions are paired by the type the handle
//!     registry actually holds, so a concrete object reaches a `&dyn Trait`
//!     parameter under its own impl tag and pairs with a receiver of the
//!     concrete type.
//!   * **Trailing bytes.** `ByteReader::assert_consumed`, which every sync arm
//!     calls before invoking the body, exists to refuse them.
//!   * **A truncated request.** `ByteReader::chunk` bounds-checks every read.
//!   * **A length prefix larger than the buffer.** The bulk list readers
//!     `checked_mul` the element count by the width for exactly this.
//!   * **An impl tag no implementor has.** The generated `match` on a
//!     `&dyn Trait` parameter ends in a `panic!` naming the trait.
//!   * **A `fn_id` the interface does not contain.** The sync dispatch's own
//!     default arm.
//!
//! Each of those refusals happens **before** the user body runs, which is what
//! makes driving them at every member safe: this driver never has to bound the
//! runtime of code it did not write.
//!
//! # What it does not attack, and why not
//!
//! * **A handle of the wrong type, and a tag that disagrees with the handle
//!   beside it.** The wire cannot tell — and neither can this. But
//!   `handle::confined_ref` and its siblings state a *precondition* ("a live
//!   handle created by the matching `*_new`"), and generated Dart cannot break
//!   it: each opaque has its own Dart class, and a trait parameter's tag is
//!   read off the very object whose handle travels with it. Driving one would
//!   report the precondition being violated, which is not a defect in anything
//!   here. There is no claim to test, so there is no test.
//! * **A fabricated or already-freed handle.** Same reason, one step further.
//! * **Boundary scalars in a body.** Extreme values go only into buffers that
//!   are refused before the body runs. A wire driver cannot bound an arbitrary
//!   user body — `sleep(i64::MAX)` is a legal thing to write behind
//!   `#[bridge]` — so parameters that will actually reach a body are filled
//!   with zeros and empties. The codec's own extremes are covered by
//!   `frustrate::codec`'s unit tests, and the wire's by the fixture's
//!   `*_extremes` members through the Dart integration suite.
//!
//! # What it cannot reach
//!
//! **Sync arms only.** A pool, cooperative-executor or actor arm answers
//! through `frustrate::post` on another thread, needs a live runtime, and
//! delivers its response somewhere this driver cannot read. Standing that up
//! under Miri is a larger piece of work than this, and pretending to cover it
//! would be worse than saying so.
//!
//! A member is also out of reach when its request cannot be built at all, and
//! each such member is counted with a named reason ([`Skip`]) rather than
//! quietly dropped — a parameter that is a Dart-object handle (the peer that
//! answers it lives in the Dart isolate), a bridge-external type (only the
//! user's codec makes those bytes), or an opaque whose constructors all refuse
//! this driver's arguments. [`the_census_of_what_is_and_is_not_driven`] prints
//! the split, and fails if the attacks reached nothing, because a gate over
//! zero members gates nothing.

use frustrate::codec::{ByteReader, ByteWriter};
use frustrate_codegen::ir::{Borrow, Exec, Function, Interface, Model, Receiver, Type};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::OnceLock;

// ------------------------------------------------------------- the wire --

/// The interface `build.rs` wrote beside the glue it generated from the same
/// parse. Embedded rather than read from disk: Miri's isolation refuses
/// `std::fs`, and re-parsing 120KB of Rust with syn inside the interpreter
/// would cost more than every request built from the result.
fn ir() -> &'static Interface {
    static IR: OnceLock<Interface> = OnceLock::new();
    IR.get_or_init(|| {
        serde_json::from_str(include_str!("frustrate_ir.json")).expect("the IR build.rs wrote")
    })
}

/// Drive one sync call through the real FFI entry, and answer with the
/// response envelope (status byte first).
fn call_sync(fn_id: u32, req: &[u8]) -> Vec<u8> {
    // Wide enough that the lease path is the exception; the lease is still
    // handled, because a leaked one is a Miri leak failure that reads as a
    // finding and is not one.
    let mut out = [0u8; 8192];
    let n = unsafe {
        crate::frustrate_generated::frustrate_call_sync(
            fn_id,
            req.as_ptr(),
            req.len() as u64,
            out.as_mut_ptr(),
            out.len() as u64,
        )
    };
    if n >= 0 {
        return out[..n as usize].to_vec();
    }
    let mut triple = [0u64; 3];
    unsafe {
        std::ptr::copy_nonoverlapping(out.as_ptr(), triple.as_mut_ptr() as *mut u8, 24);
    }
    let [ptr, len, cap] = triple;
    let payload = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) }.to_vec();
    unsafe { frustrate::frustrate_buffer_free(ptr as *mut u8, len, cap) };
    payload
}

/// The status byte, and the message when the envelope carries one.
fn outcome(resp: &[u8]) -> (u8, String) {
    let status = resp[0];
    let msg = match status {
        frustrate::envelope::STATUS_ERROR
        | frustrate::envelope::STATUS_PANIC
        | frustrate::envelope::STATUS_CONTENTION => ByteReader::new(&resp[1..]).read_string(),
        _ => String::new(),
    };
    (status, msg)
}

// ------------------------------------------------ reclaiming what we mint --

/// The `frustrate_drop_*` export for each opaque this driver can mint.
///
/// A per-type `#[no_mangle]` rather than a dispatch id, so there is no way to
/// reach it from the IR the way every call is reached; the table is the
/// bridge. [`every_mintable_opaque_has_a_reclaim`] fails if an opaque is added
/// and not listed, so the table cannot silently fall behind — and without it
/// the leak checker would report every handle this mints.
type Reclaim = unsafe extern "C" fn(*mut core::ffi::c_void);

fn reclaim_table() -> BTreeMap<&'static str, Reclaim> {
    use crate::frustrate_generated as g;
    BTreeMap::from_iter([
        ("Abacus", g::frustrate_drop_Abacus as Reclaim),
        ("Chit", g::frustrate_drop_Chit as Reclaim),
        ("CountingStore", g::frustrate_drop_CountingStore as Reclaim),
        ("Counter", g::frustrate_drop_Counter as Reclaim),
        ("Greeter", g::frustrate_drop_Greeter as Reclaim),
        ("Jar", g::frustrate_drop_Jar as Reclaim),
        ("Ledger", g::frustrate_drop_Ledger as Reclaim),
        ("LiveProbe", g::frustrate_drop_LiveProbe as Reclaim),
        ("LockProbe", g::frustrate_drop_LockProbe as Reclaim),
        ("Note", g::frustrate_drop_Note as Reclaim),
        ("ReentrantProbe", g::frustrate_drop_ReentrantProbe as Reclaim),
        ("RobotGreeter", g::frustrate_drop_RobotGreeter as Reclaim),
        ("Scene", g::frustrate_drop_Scene as Reclaim),
        ("Slip", g::frustrate_drop_Slip as Reclaim),
        ("Snapshot", g::frustrate_drop_Snapshot as Reclaim),
        ("Store", g::frustrate_drop_Store as Reclaim),
        ("Tag", g::frustrate_drop_Tag as Reclaim),
        ("Tally", g::frustrate_drop_Tally as Reclaim),
        ("Tape", g::frustrate_drop_Tape as Reclaim),
        ("TextDoc", g::frustrate_drop_TextDoc as Reclaim),
        ("Vault", g::frustrate_drop_Vault as Reclaim),
    ]
    .into_iter()
    // The generated fixture's opaques, derived by the same run that wrote
    // them. Hand-written types keep their hand-written rows above; a
    // regenerated fixture must not require anyone to paste names into a list.
    .chain(include!("shapes_reclaim.rs")))
}

// --------------------------------------------------- why a member is out --

/// Why the driver could not build a request for a member. Named rather than
/// counted, so the census says what is uncovered instead of only how much.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Skip {
    /// Not a sync arm — see the module header, "What it cannot reach".
    NotSync,
    /// A Dart-object handle (stream sink, callback, closure mirror). The peer
    /// that answers it lives in the Dart isolate.
    DartHandle,
    /// A bridge-external type: only the user's own `BytesCodec` makes bytes
    /// this decoder will accept.
    ExternCodec,
    /// An opaque no constructor reachable from this driver produces.
    NoHandle(String),
    /// A value type nested deeper than the driver walks.
    TooDeep,
    /// The generated shape fixture — compiled by rustc, deliberately not
    /// fuzzed here. See [`sync_members`].
    GeneratedShape,
}

impl fmt::Display for Skip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Skip::NotSync => write!(f, "not a sync arm"),
            Skip::DartHandle => write!(f, "takes a Dart-object handle"),
            Skip::ExternCodec => write!(f, "takes a bridge-external type"),
            Skip::NoHandle(t) => write!(f, "needs a `{t}` handle nothing here can mint"),
            Skip::TooDeep => write!(f, "a value type nests deeper than the encoder walks"),
            Skip::GeneratedShape => {
                write!(f, "generated shape fixture: compiled, not fuzzed")
            }
        }
    }
}

// ---------------------------------------------------------- the fixture --

/// One test's live objects. Everything minted is reclaimed by [`Env::reclaim`];
/// nothing is shared between tests, so two tests running at once never hand
/// one object to two threads.
struct Env {
    ir: &'static Interface,
    minted: Vec<(String, u64)>,
}

impl Env {
    fn new() -> Self {
        Env {
            ir: ir(),
            minted: vec![],
        }
    }

    /// A fresh object of opaque type `name`, through the wire.
    ///
    /// Fresh every time, deliberately. A sync member that takes a lock and
    /// then meets a malformed buffer panics while holding the guard, which
    /// poisons the lock — so reusing an object across attacks would make every
    /// later call on it fail for a reason that is not the one under test.
    fn mint(&mut self, name: &str) -> Result<u64, Skip> {
        let ctors: Vec<&Function> = self
            .ir
            .functions
            .iter()
            .filter(|f| {
                f.exec == Exec::Sync
                    && !f.rust_async
                    && matches!(&f.ret, Some(Type::Opaque(n)) if n == name)
                    && f.receiver.is_none()
                    && !f.params.iter().any(|p| matches!(p.ty, Type::Opaque(_)))
            })
            .collect();
        for f in ctors {
            for fill in [Fill::Zero, Fill::One] {
                let mut w = ByteWriter::new();
                if params_of(self.ir, f, &mut w, fill).is_err() {
                    continue;
                }
                let resp = call_sync(f.fn_id, &w.take());
                if resp[0] == frustrate::envelope::STATUS_OK {
                    let h = ByteReader::new(&resp[1..]).read_handle();
                    self.minted.push((name.to_string(), h));
                    return Ok(h);
                }
            }
        }
        Err(Skip::NoHandle(name.to_string()))
    }

    /// The call took the object behind `h`, so it is no longer this
    /// driver's to reclaim. A consuming member ends an object mid-call: what
    /// the generated glue adopts (`handle::*_adopt`) it drops — in the body,
    /// on a refused take, or during a later panic's unwind — and a second
    /// free from [`Env::reclaim`] would be exactly the double free the Dart
    /// token exists to rule out.
    fn forget(&mut self, h: u64) {
        self.minted.retain(|(_, m)| *m != h);
    }

    fn reclaim(self) {
        let table = reclaim_table();
        for (name, h) in self.minted {
            let drop_it = table
                .get(name.as_str())
                .unwrap_or_else(|| panic!("no reclaim for `{name}`"));
            unsafe { drop_it(h as *mut core::ffi::c_void) };
        }
    }
}

/// Which value a parameter that will reach a body is filled with. Both are
/// benign: see the module header on why extremes are not driven into bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fill {
    /// Zero, false, empty, the epoch, the first variant.
    Zero,
    /// One, true, a single element. The second thing to try when a
    /// constructor refuses the first.
    One,
}

// ------------------------------------------------------ value encoding --

/// The wire form of one value, exactly as `emit_rust`'s `decode_expr` reads
/// it back.
fn value(iface: &Interface, w: &mut ByteWriter, ty: &Type, fill: Fill, depth: u32) -> Result<(), Skip> {
    if depth > 6 {
        return Err(Skip::TooDeep);
    }
    let n = |z: i64, o: i64| if fill == Fill::Zero { z } else { o };
    match ty {
        Type::Bool => w.write_bool(fill == Fill::One),
        Type::I8 => w.write_i8(n(0, 1) as i8),
        Type::I16 => w.write_i16(n(0, 1) as i16),
        Type::I32 => w.write_i32(n(0, 1) as i32),
        Type::I64 => w.write_i64(n(0, 1)),
        Type::U8 => w.write_u8(n(0, 1) as u8),
        Type::U16 => w.write_u16(n(0, 1) as u16),
        Type::U32 => w.write_u32(n(0, 1) as u32),
        Type::U64 => w.write_u64(n(0, 1) as u64),
        Type::I128 => w.write_i128(n(0, 1) as i128),
        Type::U128 => w.write_u128(n(0, 1) as u128),
        Type::F32 => w.write_f32(n(0, 1) as f32),
        Type::F64 => w.write_f64(n(0, 1) as f64),
        Type::Usize => w.write_usize(n(0, 1) as usize),
        Type::Isize => w.write_isize(n(0, 1) as isize),
        Type::String => w.write_string(if fill == Fill::Zero { "" } else { "x" }),
        Type::Char => w.write_char('x'),
        // Every peer reads i64 microseconds; 0 is the epoch, and inside every
        // peer's range.
        Type::Duration(_) | Type::SystemTime(_) => w.write_i64(0),
        Type::Bytes => w.write_bytes(if fill == Fill::Zero { &[] } else { &[0u8] }),
        Type::ByteArray(len) => w.write_byte_array(&vec![0u8; *len]),
        // `[T; N]`: exactly N elements and NO length prefix — the length is in
        // the type, so `fill` cannot vary the count the way it does for a list.
        Type::Array(inner, len) => {
            for _ in 0..*len {
                value(iface, w, inner, fill, depth + 1)?;
            }
        }
        // A `Box` is transparent on the wire; the reader puts it back. So is a
        // borrow: `Vec<&str>` and `Vec<String>` are the same bytes, and only
        // the Rust glue differs. A borrow of a *handle* never reaches here —
        // [`steps`] peels it into a handle position first.
        Type::Boxed(inner) | Type::Ref { inner, .. } => value(iface, w, inner, fill, depth)?,
        // Length prefix then elements; the container kind changes only what
        // the reader reconstructs, never the bytes.
        Type::List(inner, _) | Type::Set(inner, _) => {
            if fill == Fill::Zero {
                w.write_len(0);
            } else {
                w.write_len(1);
                value(iface, w, inner, fill, depth + 1)?;
            }
        }
        Type::Map(k, v, _) => {
            if fill == Fill::Zero {
                w.write_len(0);
            } else {
                w.write_len(1);
                value(iface, w, k, fill, depth + 1)?;
                value(iface, w, v, fill, depth + 1)?;
            }
        }
        Type::Option(inner) => {
            if fill == Fill::Zero {
                w.write_bool(false);
            } else {
                w.write_bool(true);
                value(iface, w, inner, fill, depth + 1)?;
            }
        }
        Type::Tuple(ts) => {
            for t in ts {
                value(iface, w, t, fill, depth + 1)?;
            }
        }
        Type::Struct(name) => {
            let s = iface
                .structs
                .iter()
                .find(|s| &s.name == name)
                .expect("a resolved struct is declared");
            for f in &s.fields {
                value(iface, w, &f.ty, fill, depth + 1)?;
            }
        }
        Type::Enum(name) => {
            let e = iface
                .enums
                .iter()
                .find(|e| &e.name == name)
                .expect("a resolved enum is declared");
            // Tag then the variant's fields, positionally — the first variant,
            // which every bridged enum has.
            w.write_u32(0);
            for f in &e.variants[0].fields {
                value(iface, w, &f.ty, fill, depth + 1)?;
            }
        }
        Type::Extern(_) => return Err(Skip::ExternCodec),
        Type::DartObject(_) => return Err(Skip::DartHandle),
        Type::Opaque(_) | Type::Named(_) | Type::Claimed(..) | Type::App(..) | Type::Param(_) => {
            unreachable!("the checker forbids these in a value position")
        }
    }
    Ok(())
}

/// Every value parameter of `f`, in order — for constructors, whose parameters
/// are all values by [`Env::mint`]'s own filter.
fn params_of(iface: &Interface, f: &Function, w: &mut ByteWriter, fill: Fill) -> Result<(), Skip> {
    for p in &f.params {
        value(iface, w, &p.ty, fill, 0)?;
    }
    Ok(())
}

// -------------------------------------------------------- acquisitions --

/// One position in a member's request that takes a handle.
#[derive(Debug, Clone)]
struct Acq {
    /// What the caller sees: `self`, or the parameter's name.
    name: String,
    /// The bridged type named at this position. For a `&dyn Trait` parameter
    /// that is the trait; the tag decides what the registry actually holds.
    named: String,
    /// Whether the position carries an impl tag before its handle.
    tagged: bool,
    mutable: bool,
    /// The call **takes** the object: `self`/`self: Box<Self>`, or a handle
    /// parameter by value. Once the glue has adopted it the object is the
    /// call's, whatever happens next, and [`Env::forget`] has to hear that.
    consumed: bool,
}

/// The types the handle registry could hold for `a`, each with the impl tag
/// that selects it.
///
/// An untagged position admits exactly one: the type its `fn_id` names. A
/// `&dyn Trait` parameter admits the boxed trait object (tag 0) and every
/// bridged implementor (tag i+1, in `trait_implementors` order) — which is why
/// a concrete object can meet a receiver of its own type and a trait-typed
/// parameter in the same call.
fn registry_types(iface: &Interface, a: &Acq) -> Vec<(String, u8)> {
    if !a.tagged {
        return vec![(a.named.clone(), 0)];
    }
    std::iter::once((a.named.clone(), 0u8))
        .chain(
            iface
                .trait_implementors(&a.named)
                .iter()
                .enumerate()
                .map(|(i, c)| (c.name.clone(), (i + 1) as u8)),
        )
        .collect()
}

/// Whether the value **hands over** a handle from inside itself — `Vec<Doc>`,
/// `Option<Doc>`, a set, a map, a tuple, and an inbound struct's fields.
///
/// The codegen crate's own predicate, not a second walk here: it is what the
/// emitter used to decide whether this member's glue takes or lends, so a
/// driver that answered differently would build a request for a member that
/// does not exist.
fn takes_handle(iface: &Interface, ty: &Type) -> bool {
    frustrate_codegen::check::takes_opaque(iface, ty)
}

/// Whether the value **lends** a handle from inside itself — `Vec<&Doc>`,
/// `Option<&Doc>`, `(&Doc, i64)`. See [`takes_handle`] on why this is the
/// emitter's own predicate, and [`LENT_ELEMENTS`] on how many the driver sends.
fn lends_handle(ty: &Type) -> bool {
    frustrate_codegen::check::lends_opaque(ty)
}

/// How many elements the driver puts in a container that lends handles.
///
/// Two, because one is the count that proves nothing: a single acquisition
/// cannot alias itself, and a plan over one lock can neither invert nor repeat.
/// Two is the smallest count that reaches both rules.
const LENT_ELEMENTS: usize = 2;

/// One step of a member's request, in wire order.
///
/// The handle positions and the bytes around them come out of **one** walk,
/// because two walks over the same type are how a driver comes to write a
/// request whose handles it then assigns to the wrong slots.
enum Step<'a> {
    /// A handle position: its impl tag, where the position carries one, then
    /// the handle taken from the assignment.
    Handle(Acq),
    /// A list's length prefix. Fixed at [`LENT_ELEMENTS`].
    Len(usize),
    /// An `Option`'s presence byte, always present: a `None` carries no handle
    /// and there would be nothing to attack.
    Present,
    /// A value the ordinary encoder writes.
    Value(&'a Type),
    /// A **data** receiver: the value itself, riding the request ahead of the
    /// parameters and decoded into a local exactly as a parameter is. Carried
    /// as the declaration's name rather than as a [`Type`], because the
    /// interface holds no `Type` node for a parent — `Function::parent` is a
    /// bare `String` — so one is built where it is written.
    Receiver(&'a str),
}

/// The wire shape of `f`'s request, or why there is none.
fn steps<'a>(iface: &'a Interface, f: &'a Function) -> Result<Vec<Step<'a>>, Skip> {
    if f.exec != Exec::Sync || f.rust_async {
        return Err(Skip::NotSync);
    }
    let mut out = vec![];
    // A **data** receiver rides the request as an encoded value, ahead of the
    // parameters, and the generated arm decodes it first (`let this =
    // dec_X(r);`). Without this step every such member's "baseline" was short
    // by a whole value, so what the attacks below mutated was a request whose
    // decode had already failed — and the census counted it as driven.
    // Members of a generic instantiation are reached the same way as any
    // other data member, so this covers both.
    if let Some(parent) = iface.data_receiver(f) {
        out.push(Step::Receiver(parent));
    }
    // The *member's* half, not the parent's name: a type declaring two
    // representations has value members whose receiver rides the request as
    // an encoded value, with no handle id to truncate.
    if let (Some(o), Some(recv)) = (iface.receiver_handle(f), f.receiver) {
        let consuming = matches!(recv, Receiver::Value | Receiver::Boxed);
        out.push(Step::Handle(Acq {
            name: "self".into(),
            // A **borrowed** receiver's registry type is the one its `fn_id`
            // names, trait or concrete, so it carries no tag: the Dart class
            // dispatched. A **consuming** one on a bridged trait does carry
            // one — its Dart member is an extension over `Consumed<T>`, which
            // Dart resolves from the static type, so the object behind the
            // token may be any implementor.
            tagged: consuming && o.dyn_trait,
            named: o.name.clone(),
            mutable: recv == Receiver::RefMut,
            consumed: match recv {
                Receiver::Value | Receiver::Boxed => true,
                Receiver::Ref | Receiver::RefMut => false,
                Receiver::Typed => {
                    unreachable!("the checker refuses a typed receiver it does not read (FR0057)")
                }
            },
        }));
    }
    for p in &f.params {
        match &p.ty {
            Type::Opaque(n) => {
                let dyn_trait = iface
                    .opaques
                    .iter()
                    .any(|o| &o.name == n && o.dyn_trait);
                out.push(Step::Handle(Acq {
                    name: p.name.clone(),
                    named: n.clone(),
                    tagged: dyn_trait,
                    mutable: p.borrow == Borrow::RefMut,
                    consumed: p.borrow == Borrow::Value,
                }));
            }
            // A handle inside a container, by value or by reference: the same
            // walk either way, because the wire is the same and only what the
            // glue then does with each id differs. As many positions as the
            // driver decides to send, the count being the caller's.
            ty if (p.borrow == Borrow::Value && takes_handle(iface, ty))
                || lends_handle(ty) =>
            {
                container_steps(iface, ty, &p.name, p.borrow == Borrow::Value, &mut out)
            }
            other => out.push(Step::Value(other)),
        }
    }
    Ok(out)
}

/// The steps for one parameter that carries handles inside a container, in
/// wire order.
///
/// `by_value` says what the *parameter* is, and it is the only thing that makes
/// a taken position taken: a handle reached without passing through a `&` in a
/// by-value parameter is one the call adopts, and [`Env::forget`] has to hear
/// so. A borrowed parameter lends every position it reaches, and one container
/// may do both (`Vec<(&Doc, Doc)>`) — which is why the `Ref` arm answers
/// `consumed: false` whatever `by_value` says.
fn container_steps<'a>(
    iface: &'a Interface,
    ty: &'a Type,
    name: &str,
    by_value: bool,
    out: &mut Vec<Step<'a>>,
) {
    let dyn_trait = |n: &str| iface.opaques.iter().any(|o| o.name == n && o.dyn_trait);
    match ty {
        Type::Ref { inner, mutable, .. } => match inner.as_ref() {
            Type::Opaque(n) => out.push(Step::Handle(Acq {
                name: name.to_string(),
                named: n.clone(),
                // A trait-typed position carries an impl tag wherever it sits,
                // inside a container as much as at the top of a parameter:
                // which registry holds the object is a per-element fact.
                tagged: dyn_trait(n),
                mutable: *mutable,
                consumed: false,
            })),
            other => out.push(Step::Value(other)),
        },
        // A handle the container hands over. Tagged for the reason a top-level
        // consume is: its Dart member is an extension over `Consumed<T>`, whose
        // static type is the trait, so the object behind the token may be any
        // implementor.
        Type::Opaque(n) if by_value => out.push(Step::Handle(Acq {
            name: name.to_string(),
            named: n.clone(),
            tagged: dyn_trait(n),
            mutable: false,
            consumed: true,
        })),
        // A set is a list on the wire, and a map is a list of key/value pairs;
        // both are staged flat, so the driver writes them the same way.
        Type::List(inner, _) | Type::Set(inner, _) => {
            out.push(Step::Len(LENT_ELEMENTS));
            for _ in 0..LENT_ELEMENTS {
                container_steps(iface, inner, name, by_value, out);
            }
        }
        Type::Map(k, v, _) => {
            out.push(Step::Len(LENT_ELEMENTS));
            for _ in 0..LENT_ELEMENTS {
                container_steps(iface, k, name, by_value, out);
                container_steps(iface, v, name, by_value, out);
            }
        }
        Type::Option(inner) => {
            out.push(Step::Present);
            container_steps(iface, inner, name, by_value, out);
        }
        Type::Tuple(ts) => {
            for t in ts {
                container_steps(iface, t, name, by_value, out);
            }
        }
        // An **inbound** struct is its fields, in declaration order, which is
        // wire order. A plain declaration is not walked: a handle behind one of
        // its fields is not handed over (FR0004), so the value encoder writes
        // it whole.
        Type::Struct(n) if frustrate_codegen::check::inbound_struct(iface, n).is_some() => {
            let decl = frustrate_codegen::check::inbound_struct(iface, n).expect("guarded");
            for f in &decl.fields {
                container_steps(iface, &f.ty, name, by_value, out);
            }
        }
        other => out.push(Step::Value(other)),
    }
}

/// The handle positions of `f`, in wire order, or why there is no request.
fn acquisitions(iface: &Interface, f: &Function) -> Result<Vec<Acq>, Skip> {
    Ok(steps(iface, f)?
        .into_iter()
        .filter_map(|s| match s {
            Step::Handle(a) => Some(a),
            _ => None,
        })
        .collect())
}

/// Build a request for `f`, taking each handle position's handle from
/// `assign` (indexed in acquisition order) and filling every value with `fill`.
fn request(
    iface: &Interface,
    f: &Function,
    assign: &[(u64, u8)],
    fill: Fill,
) -> Result<Vec<u8>, Skip> {
    let mut w = ByteWriter::new();
    let mut next = 0usize;
    for step in steps(iface, f)? {
        match step {
            Step::Handle(a) => {
                let (h, tag) = assign[next];
                if a.tagged {
                    w.write_u8(tag);
                }
                w.write_handle(h);
                next += 1;
            }
            Step::Len(n) => w.write_len(n),
            Step::Present => w.write_bool(true),
            Step::Value(t) => value(iface, &mut w, t, fill, 0)?,
            Step::Receiver(name) => {
                // The right variant, not a guess: `Struct` and `Enum` are
                // distinct nodes and `value` looks the declaration up by the
                // one it is handed.
                let ty = if iface.struct_decl(name).is_some() {
                    Type::Struct(name.to_string())
                } else {
                    Type::Enum(name.to_string())
                };
                value(iface, &mut w, &ty, fill, 0)?;
            }
        }
    }
    Ok(w.take())
}

/// A fresh object for one handle position, and the impl tag that names its
/// type.
///
/// Every registry type the position admits is tried, not just the first: a
/// `&dyn Trait` parameter of a trait nothing constructs directly is still
/// reachable through any bridged implementor, and refusing it would leave
/// every trait-typed member out of the gate.
fn mint_for(env: &mut Env, a: &Acq) -> Result<(u64, u8), Skip> {
    let admits = registry_types(env.ir, a);
    let mut last = Skip::NoHandle(a.named.clone());
    for (ty, tag) in admits {
        match env.mint(&ty) {
            Ok(h) => return Ok((h, tag)),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// One object per handle position, with the impl tag that names its type —
/// what [`request`] takes and [`baseline`] hands back beside the bytes.
type Assignment = Vec<(u64, u8)>;

/// Every member whose request this driver can build, with a distinct object
/// for each handle position — the baseline the malformed-buffer attacks
/// mutate, and the control that proves the aliased request reaches the same
/// code.
fn baseline(env: &mut Env, f: &Function) -> Result<(Vec<u8>, Assignment), Skip> {
    if is_generated_shape(f) {
        return Err(Skip::GeneratedShape);
    }
    let acqs = acquisitions(env.ir, f)?;
    let mut assign = vec![];
    for a in &acqs {
        assign.push(mint_for(env, a)?);
    }
    Ok((request(env.ir, f, &assign, Fill::Zero)?, assign))
}

/// Which handles a call **took**, settled after the answer.
///
/// The generated glue adopts a consumed handle after the request has decoded
/// and the duplicate check has passed, and before anything that can return a
/// refusal — so that a refused take releases what Dart gave up rather than
/// leaking it. Read from the driver's side, that ordering says: a `PANIC` on
/// a request that was refused *before* adoption (a duplicate, a truncation)
/// left the object alive; every other answer — the value, the member's own
/// `Err`, a contended take, a trailing-byte refusal (`assert_consumed` runs
/// after adoption) — means the call owned it and it is gone.
///
/// `taken` is therefore the caller's verdict, decided per attack from what it
/// sent; this only applies it.
fn settle(env: &mut Env, acqs: &[Acq], assign: &[(u64, u8)], taken: bool) {
    if !taken {
        return;
    }
    for (a, (h, _)) in acqs.iter().zip(assign) {
        if a.consumed {
            env.forget(*h);
        }
    }
}

/// The sync members this driver fuzzes: the hand-written surface, in
/// declaration order. `src/shapes.rs` is excluded.
///
/// The exclusion is measured rather than cautious. Driving the generated
/// fixture here — five hundred more members — took this pass from minutes to
/// hours under Miri, for a run whose whole point is to sit in the ordinary
/// analysis loop; sampling one member per (arm, model, dyn, params) only
/// halved that. `dart run tools/analyze.dart` prints what the leg costs now.
///
/// It is sound because the two fixtures buy different things. `shapes.rs`
/// exists so that every emitted dispatch is **compiled**, and rustc delivers
/// that whether or not this driver runs. What this pass uniquely adds is
/// fuzzing the request decode — and a generated member has nothing to fuzz:
/// grid scalars in, `0` out, no nested types and no real codec. The
/// hand-written surface is where the codecs, the nested types and the bodies
/// are, and it is driven in full.
///
/// Revisit if the generator ever emits members with real payloads.
///
/// [`baseline`] refuses one too, with [`Skip::GeneratedShape`], so the census
/// below accounts for the fixture by name instead of paying to drive it. The
/// two must agree: a census that reported coverage the gate does not have
/// would be worse than no census.
fn sync_members() -> Vec<&'static Function> {
    ir()
        .functions
        .iter()
        .filter(|f| f.exec == Exec::Sync && !f.rust_async)
        .filter(|f| !is_generated_shape(f))
        .collect()
}

/// A member of the generated fixture (`src/shapes.rs`), by the module path
/// codegen recorded for it. The one place the exclusion is decided.
fn is_generated_shape(f: &Function) -> bool {
    f.module_path.ends_with("shapes")
}

// ------------------------------------------------------------- the gate --

/// The bindings and the interface must describe one wire. If they ever do not,
/// every `fn_id` below addresses a different member than the one it names, and
/// nothing else here means anything.
#[test]
fn the_embedded_interface_is_the_one_the_glue_was_generated_from() {
    assert_eq!(
        frustrate_codegen::hash::schema_hash(ir()),
        crate::frustrate_generated::frustrate_schema_hash(),
    );
}

/// The reclaim table is the one place a type has to be named by hand. Adding
/// an opaque without adding its row would leave every handle of it leaked, and
/// a leak under Miri reads as a finding.
#[test]
fn every_mintable_opaque_has_a_reclaim() {
    let table = reclaim_table();
    let mintable: BTreeSet<&str> = ir()
        .opaques
        .iter()
        .filter(|o| o.model != Model::Actor)
        .map(|o| o.name.as_str())
        .collect();
    let listed: BTreeSet<&str> = table.keys().copied().collect();
    assert_eq!(
        mintable, listed,
        "the reclaim table and the interface's non-actor opaques disagree"
    );
}

/// The attack that found both of this week's defects: one object handed to two
/// positions that can each accept it.
///
/// Every pairing the wire admits is driven — receiver with parameter,
/// parameter with parameter, and a concrete object meeting a `&dyn Trait`
/// position under its own impl tag. The assertion is only that the call
/// *answers*: whether the answer is the value, a refusal naming the member, or
/// a contention error is the runtime's business. Undefined behaviour is not an
/// answer, and Miri is what says so.
#[test]
fn one_object_in_two_positions_is_answered_and_never_undefined() {
    let mut env = Env::new();
    let mut driven = 0usize;
    for f in sync_members() {
        let Ok(acqs) = acquisitions(env.ir, f) else {
            continue;
        };
        if acqs.len() < 2 {
            continue;
        }
        // Which registry type can serve more than one position — that is the
        // only way one object reaches two of them.
        let mut shared: BTreeSet<String> = BTreeSet::new();
        for (i, a) in acqs.iter().enumerate() {
            for (ty, _) in registry_types(env.ir, a) {
                if acqs.iter().enumerate().any(|(j, b)| {
                    j != i && registry_types(env.ir, b).iter().any(|(t, _)| *t == ty)
                }) {
                    shared.insert(ty);
                }
            }
        }
        for ty in shared {
            // The control first: distinct objects everywhere, so a green
            // attack cannot be green because the request never arrived.
            let Ok((base, base_assign)) = baseline(&mut env, f) else {
                continue;
            };
            let (status, msg) = outcome(&call_sync(f.fn_id, &base));
            // A well-formed request reaches the body, so a consuming member
            // has taken its objects whatever it answered.
            settle(&mut env, &acqs, &base_assign, true);
            // Any *answer* proves the request decoded and the body ran: a
            // decode failure is a panic, never an error, so an `Err` here is
            // the member's own verdict on zero-filled arguments and says
            // nothing about the wire.
            assert!(
                matches!(
                    status,
                    frustrate::envelope::STATUS_OK
                        | frustrate::envelope::STATUS_ERROR
                        | frustrate::envelope::STATUS_TYPED_ERROR
                ),
                "`{}` did not answer its own well-formed request: status {status} {msg}",
                f.name
            );

            let Ok(one) = env.mint(&ty) else { continue };
            let mut assign = vec![];
            let mut aliased: Vec<String> = vec![];
            let mut buildable = true;
            for a in &acqs {
                match registry_types(env.ir, a).iter().find(|(t, _)| *t == ty) {
                    Some((_, tag)) => {
                        assign.push((one, *tag));
                        aliased.push(format!(
                            "{}{}",
                            if a.mutable { "&mut " } else { "&" },
                            a.name
                        ));
                    }
                    None => match mint_for(&mut env, a) {
                        Ok(ht) => assign.push(ht),
                        Err(_) => buildable = false,
                    },
                }
            }
            if !buildable {
                continue;
            }
            let Ok(req) = request(env.ir, f, &assign, Fill::Zero) else {
                continue;
            };
            let resp = call_sync(f.fn_id, &req);
            let (status, _) = outcome(&resp);
            assert!(
                (0..=8).contains(&status),
                "`{}` answered status {status} to one `{ty}` in {} — not a status \
                 this wire defines",
                f.name,
                aliased.join(" and "),
            );
            // A duplicate involving a consume is refused before adoption
            // (`alias_check` runs on the staged ids), so a panic leaves the
            // object alive and anything else means the call took it.
            settle(
                &mut env,
                &acqs,
                &assign,
                status != frustrate::envelope::STATUS_PANIC,
            );
            driven += 1;
        }
    }
    env.reclaim();
    assert!(
        driven > 0,
        "no member was driven with one object in two positions — the attack that \
         found two defects reached nothing"
    );
}

/// A request with one byte too many. `assert_consumed` runs before the body on
/// every sync arm, so this never reaches user code — which is what makes it
/// safe to drive at every member. It runs *after* a consuming member has
/// adopted its handles, though, so those objects are dropped by the unwind and
/// are not this driver's to reclaim.
#[test]
fn a_request_with_a_trailing_byte_is_refused_attributably() {
    let mut env = Env::new();
    let mut driven = 0usize;
    for f in sync_members() {
        let Ok((mut req, assign)) = baseline(&mut env, f) else {
            continue;
        };
        let acqs = acquisitions(env.ir, f).expect("baseline built, so acquisitions did");
        settle(&mut env, &acqs, &assign, true);
        req.push(0xAB);
        let (status, msg) = outcome(&call_sync(f.fn_id, &req));
        assert_eq!(
            status,
            frustrate::envelope::STATUS_PANIC,
            "`{}` accepted a trailing byte: status {status}",
            f.name
        );
        assert!(
            msg.contains("frustrate"),
            "`{}` refused a trailing byte unattributably: {msg}",
            f.name
        );
        driven += 1;
    }
    env.reclaim();
    assert!(driven > 0, "no member was driven with a trailing byte");
}

/// A request one byte short. `ByteReader::chunk` bounds-checks every read, so
/// this too is refused before the body.
#[test]
fn a_truncated_request_is_refused_attributably() {
    let mut env = Env::new();
    let mut driven = 0usize;
    for f in sync_members() {
        // A cut can land on either side of a consumed handle's adoption —
        // the glue adopts at the read where no duplicate check defers it —
        // and this driver cannot tell from the refusal which side it was.
        // Reclaiming would be a double free on one side and skipping a leak
        // on the other, and Miri reports both, so the member is left to the
        // attacks whose outcome decides it.
        if acquisitions(env.ir, f).is_ok_and(|a| a.iter().any(|a| a.consumed)) {
            continue;
        }
        let Ok((req, _)) = baseline(&mut env, f) else {
            continue;
        };
        if req.is_empty() {
            continue;
        }
        let (status, msg) = outcome(&call_sync(f.fn_id, &req[..req.len() - 1]));
        assert_eq!(
            status,
            frustrate::envelope::STATUS_PANIC,
            "`{}` accepted a truncated request: status {status}",
            f.name
        );
        assert!(
            msg.contains("frustrate"),
            "`{}` refused a truncated request unattributably: {msg}",
            f.name
        );
        driven += 1;
    }
    env.reclaim();
    assert!(driven > 0, "no member was driven with a truncated request");
}

/// A length prefix far larger than the buffer, on the first parameter that
/// carries one. The bulk list readers `checked_mul` the count by the element
/// width precisely so a corrupt count cannot size a copy.
#[test]
fn an_impossible_length_prefix_is_refused_attributably() {
    let env = Env::new();
    let mut driven = 0usize;
    for f in sync_members() {
        if !acquisitions(env.ir, f).is_ok_and(|a| a.is_empty()) {
            continue;
        }
        let Some(i) = f.params.iter().position(|p| length_prefixed(&p.ty)) else {
            continue;
        };
        let mut w = ByteWriter::new();
        let mut built = true;
        for (j, p) in f.params.iter().enumerate() {
            if j == i {
                // The prefix, and nothing after it: whichever of the two
                // guards fires — the multiply or the bounds check — the read
                // cannot succeed.
                w.write_len(usize::MAX / 2);
                break;
            }
            if value(env.ir, &mut w, &p.ty, Fill::Zero, 0).is_err() {
                built = false;
                break;
            }
        }
        if !built {
            continue;
        }
        let (status, msg) = outcome(&call_sync(f.fn_id, &w.take()));
        assert_eq!(
            status,
            frustrate::envelope::STATUS_PANIC,
            "`{}` accepted an impossible length prefix: status {status}",
            f.name
        );
        assert!(
            msg.contains("frustrate"),
            "`{}` refused an impossible length prefix unattributably: {msg}",
            f.name
        );
        driven += 1;
    }
    env.reclaim();
    assert!(driven > 0, "no member was driven with an impossible length");
}

/// Whether a type's wire form starts with a length the caller supplies.
///
/// A fixed array does **not**: `[T; N]` and `[u8; N]` carry their length in
/// the type, so there is no count for a hostile request to lie about. A `Box`
/// is transparent, so the question is its inner type's.
fn length_prefixed(ty: &Type) -> bool {
    match ty {
        Type::Boxed(inner) => length_prefixed(inner),
        _ => matches!(
            ty,
            Type::String | Type::Bytes | Type::List(..) | Type::Set(..) | Type::Map(..)
        ),
    }
}

/// An impl tag no implementor has. The generated `match` on a `&dyn Trait`
/// parameter ends in a `panic!` naming the trait, which is the claim.
#[test]
fn an_unknown_impl_tag_is_refused_attributably() {
    let mut env = Env::new();
    let mut driven = 0usize;
    for f in sync_members() {
        let Ok(acqs) = acquisitions(env.ir, f) else {
            continue;
        };
        if !acqs.iter().any(|a| a.tagged) {
            continue;
        }
        // The unknown-tag panic fires when the trait position is acquired,
        // which may be after a consumed neighbour was adopted at its read:
        // see the truncation attack for why that is undecidable here.
        if acqs.iter().any(|a| a.consumed) {
            continue;
        }
        let mut assign = vec![];
        let mut buildable = true;
        for a in &acqs {
            match mint_for(&mut env, a) {
                // 0xFF is past every implementor: a trait with 255 bridged
                // implementors would need a wider tag on the wire first. The
                // handle beside it is a real one — the generated `match` reads
                // the tag first and panics before anything is dereferenced, so
                // this drives the unknown tag and nothing else.
                Ok((h, tag)) => assign.push((h, if a.tagged { 0xFF } else { tag })),
                Err(_) => buildable = false,
            }
        }
        if !buildable {
            continue;
        }
        let Ok(req) = request(env.ir, f, &assign, Fill::Zero) else {
            continue;
        };
        let (status, msg) = outcome(&call_sync(f.fn_id, &req));
        assert_eq!(
            status,
            frustrate::envelope::STATUS_PANIC,
            "`{}` accepted an unknown impl tag: status {status}",
            f.name
        );
        assert!(
            msg.contains("unknown impl tag"),
            "`{}` refused an unknown impl tag unattributably: {msg}",
            f.name
        );
        driven += 1;
    }
    env.reclaim();
    assert!(driven > 0, "no member was driven with an unknown impl tag");
}

/// A `fn_id` the interface does not contain. The sync dispatch's default arm.
#[test]
fn an_unknown_fn_id_is_refused_attributably() {
    let highest = ir().functions.iter().map(|f| f.fn_id).max().unwrap_or(0);
    let (status, msg) = outcome(&call_sync(highest + 1, &[]));
    assert_eq!(status, frustrate::envelope::STATUS_PANIC);
    assert!(msg.contains("unknown sync fn_id"), "{msg}");
}

/// What the gate above does and does not reach, printed rather than assumed.
///
/// The number that matters is the split, not a total: a total goes stale the
/// moment a member is added, and a gate whose coverage nobody can see is a
/// gate nobody can trust. Fails only when the driver reaches nothing at all.
#[test]
fn the_census_of_what_is_and_is_not_driven() {
    let mut env = Env::new();
    let mut driven: Vec<&str> = vec![];
    let mut skipped: BTreeMap<Skip, Vec<&str>> = BTreeMap::new();
    for f in ir().functions.iter() {
        match baseline(&mut env, f) {
            Ok(_) => driven.push(&f.name),
            Err(why) => skipped.entry(why).or_default().push(&f.name),
        }
    }
    env.reclaim();

    println!(
        "hostile: {} of {} members reachable from this driver",
        driven.len(),
        ir().functions.len()
    );
    for (why, members) in &skipped {
        println!("  {:>4}  {why}", members.len());
    }
    assert!(
        !driven.is_empty(),
        "the driver reached no member at all — every attack above is vacuous"
    );
}

/// The positive control for every attack above: the driver's **unmutated**
/// baseline must be a request the wire accepts.
///
/// Without it the attacks are vacuous wherever the driver builds the wrong
/// bytes — a codec refusal looks the same whether the mutation caused it or
/// the baseline was already malformed, and the trailing-byte and truncation
/// assertions pass either way. That was not hypothetical: before a data
/// receiver was staged (`Step::Receiver`), 20 members' baselines were short by
/// a whole value and every attack on them was measuring a decode that had
/// already failed.
///
/// A **codec** refusal only. A user body is free to panic on whatever the
/// driver's zero-filled arguments mean to it, and that is not this driver's
/// business; `ByteReader` refusing the buffer is.
///
/// **Not under Miri**, and it is the only test here that has to say so. Every
/// attack above is refused before the body runs, which is what lets them be
/// driven at every member — this one sends a request the arm accepts, so the
/// body *does* run, and the module's contract is that this driver never has to
/// bound the runtime of code it did not write. It runs under `cargo test`,
/// where that costs milliseconds, and that is where it does its work: the
/// property it pins is about the bytes the driver writes, which Miri would not
/// tell it anything more about.
#[cfg(not(miri))]
#[test]
fn every_baseline_request_is_one_the_wire_accepts() {
    let mut env = Env::new();
    let mut driven = 0usize;
    for f in sync_members() {
        let Ok((req, assign)) = baseline(&mut env, f) else {
            continue;
        };
        let acqs = acquisitions(env.ir, f).expect("baseline built, so acquisitions did");
        settle(&mut env, &acqs, &assign, true);
        let (status, msg) = outcome(&call_sync(f.fn_id, &req));
        if status == frustrate::envelope::STATUS_PANIC {
            assert!(
                !msg.contains("frustrate codec:"),
                "`{}`: the driver's own baseline is malformed, so every attack on this \
                 member is vacuous: {msg}",
                f.name
            );
        }
        driven += 1;
    }
    env.reclaim();
    assert!(driven > 0, "no baseline was built at all");
}
