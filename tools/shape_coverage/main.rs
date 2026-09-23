//! Gate: every **member shape** the checker accepts must be instantiated by
//! this repo's own bridge sources.
//!
//!     cargo run -p shape-coverage            the gate
//!     cargo run -p shape-coverage -- --census  every legal cell, covered or not
//!
//! A member's generated glue is a function of a small set of facts — which
//! dispatch arm it lands in, the concurrency model of the opaque it touches,
//! how the receiver and the parameters take that opaque — borrowed, or
//! consumed, and for a receiver in which of its two spellings — whether the opaque
//! is a `dyn` trait, and the three declared flags (`on_contention`,
//! `native_only`, `no_block`). Call one assignment of those facts a **cell**.
//! Codegen emits per cell; a cell no source instantiates is a cell nothing
//! compiles and nothing runs, which is how two defects reached this tree —
//! a confined method taking a parameter of its own type (two references over
//! one `Box`, one of them `&mut`), and an `async fn` on a locked type (the
//! generated future held a `std` guard across the user body's awaits, which
//! made it `!Send`). Both were legal to write, and neither had ever been
//! written. The second is fixed — the `locked` model's lock now awaits its
//! acquisition and its guards are `Send` — which is what a missing cell is
//! *for*: it named a shape nothing compiled, and the answer turned out to be
//! that codegen was wrong, not that the shape was.
//!
//! # The checker is the oracle
//!
//! Which cells are **legal** is not decided here and must not be. For each
//! grid point this synthesizes a bridge source, runs `parse::parse_source`
//! and `check::check`, and takes "accepted" as the definition. A second copy
//! of the FR rules would drift from the first, and the drift would be silent
//! in the direction that matters: a cell this file believed illegal would
//! never be reported missing.
//!
//! So a rejection is never this tool's verdict. It records the FR codes and
//! moves on — see `--census`, which prints them.
//!
//! # What a cell is, and what it deliberately forgets
//!
//! Coverage is a **covers-relation**, not a partition. One member covers one
//! cell per opaque type it touches (and a single "no opaque" cell when it
//! touches none), with every fact read *relative to that type*: the receiver
//! kind if the member hangs off it, how many parameters have it, how those
//! parameters borrow, and whether some parameter is a *different* opaque.
//! Facts about anything else are projected away — `transfer(&mut Vault,
//! &mut Vault, i64)` covers the Vault cell for two mutable borrows, and the
//! `i64` changes no handle glue, so it changes no cell.
//!
//! Projected away deliberately, each because it does not change which glue
//! codegen emits for a handle: extra value parameters, the return type,
//! whether an argument-less member is a free function or an associated one,
//! `Deferred`, and typed errors. Parameter count saturates at two — the
//! third handle of a type meets exactly the code the second one did.
//!
//! Members synthesized by the checker (a trait's default bodies onto an
//! implementor, the per-actor drop) are classified like any other: they are
//! emitted and they are dispatched, so they cover what they exhibit.
//!
//! # What it cannot decide
//!
//! **The legal set is the image of this file's own grid under the checker.**
//! A shape no grid point can spell is invisible here — not reported legal,
//! and therefore never reported missing. Two things keep that honest rather
//! than merely stated:
//!
//!   * Every accepted spelling is **round-tripped**: the checked IR is
//!     classified and must land in the cell the grid point meant. A synthesis
//!     bug that quietly tests the wrong shape fails here instead of shipping
//!     a green gate over nothing.
//!   * The reverse difference is reported too. A cell this repo instantiates
//!     that no grid point can spell means the grid is narrower than the
//!     sources it is judging, and that number is printed on every run.
//!
//! A grid point is tried in several spellings (free function, associated
//! function, method, constructor, a handle lent inside a container, a member
//! hung off the sibling; a trait's supertraits written out). The
//! synthesizer's job is to find *a* legal spelling; only when none of them
//! round-trips is the point recorded as unspellable. So a rejection can still
//! be an artifact of how this file writes Rust rather than a fact about the
//! shape — which is why the census prints the FR codes rather than a verdict.
//!
//! One hole in the grid is known and named rather than left to be found: a
//! member that touches a type only through its *receiver* while its
//! parameters are some **other** opaque — `fn m(&mut self, p: &mut Other)` —
//! has no grid point, because [`Shape::TwoDiff`] always gives the cell's own
//! type a parameter too. Such a member's cell is therefore absent from the
//! legal set, and the shapes it would have contributed are under-reported.
//! [`tests::every_cell_this_repo_instantiates_lies_inside_the_grid`] is what
//! makes that loud: the first member of that kind written into `test_api`
//! fails it by name.
//!
//! # What a finding means
//!
//! A missing cell is a shape a user can write that nothing in this repo
//! compiles or runs. The remedy is one of two things, and this tool does not
//! choose between them: instantiate it in `tests/test_api`, or make the
//! checker refuse it. A cell can also turn out to need a third: the locked
//! `async fn` above was uninstantiable until codegen stopped handing a
//! blocking lock to a cooperative executor. "Nothing compiles this" is a
//! finding about the repo, not always about the shape.
//!
//! # Coverage is per emitted code path, not per cell
//!
//! A cell is an assignment of *declared facts*, and several assignments can
//! produce byte-identical dispatch — there is then one thing to compile and one
//! thing to run, so instantiating any of them covers all of them. The tool
//! emits for every legal cell with `emit_rust` and groups by what comes out
//! ([`emission_classes`]), so which axes fold is decided by the emitter rather
//! than declared irrelevant here. An axis that starts mattering re-splits its
//! group without anyone remembering to.
//!
//! Measured, and worth knowing before reading a number: two axes fold, and the
//! second is conditional. `no_block` never reaches the dispatch body — it only
//! feeds the claim census, which `check_block.dart` gates separately — so its
//! two values always emit the same code. `native_only` folds only where the
//! member is *already* native-only by derivation (an `on_contention = "block"`
//! contract, a returning callback on a non-`async fn`), because there the
//! declaration adds no cfg that derivation did not. Every other axis changes
//! the emitted dispatch: the arm changes the
//! function shape, the model and borrow change the acquisition, `dyn` adds
//! carrier enums, the receiver changes guard mutability, the parameter count
//! changes the decode, `on_contention` picks try-lock or blocking, and
//! `native_only` adds the cfg wrapper.

use frustrate_codegen::ir::*;
use frustrate_codegen::{check, parse};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

// ------------------------------------------------------------- the cell --

/// Which dispatch arm codegen emits the member into. Derived from the checked
/// IR, never from what the source declared: a `#[bridge]` method on a confined
/// type is rewritten to sync by the checker, and the arm is what it ends up as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Arm {
    /// `fn call_N(&mut ByteReader) -> Outcome`, run on the calling thread.
    Sync,
    /// `fn spawn_N(&mut ByteReader, call_id)`, handed to the thread pool.
    Pool,
    /// `fn spawn_N(&mut ByteReader, call_id)`, handed to the cooperative
    /// executor because the user body is a Rust `async fn`.
    AsyncFn,
    /// `fn actor_N(...)`, run on the instance's own executor.
    Actor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ModelAxis {
    /// The member touches no opaque type at all.
    None,
    Confined,
    Resident,
    Frozen,
    Locked,
    Actor,
}

/// How the member's receiver takes the type the cell is about. `None` also
/// covers "the member does not hang off this type" — a free function, a
/// method on some other opaque, or a member of a type's **data** half, whose
/// receiver rides the request as a value and reaches no handle at all.
///
/// `Value` and `Boxed` are two cells rather than one. They cross identically —
/// one handle, taken — and they differ only in what the glue hands the body,
/// which is exactly the kind of fact this file projects away *when the emitter
/// agrees*. Here it does not: on a `locked` handle the dispatch reads
/// `Ok(this) => this` against `Ok(this) => Box::new(this)`, and on a
/// `confined` one `*box_this` against `box_this`. Folding them would declare
/// an axis irrelevant that `emit_rust` reads — and would do it irreversibly,
/// because only one of the two spellings would ever be synthesized, so the
/// group could never re-split when the emitter changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Recv {
    None,
    Ref,
    RefMut,
    /// `self` or `mut self`: the call takes the object.
    Value,
    /// `self: Box<Self>`: the call takes the object and the body receives the
    /// registry's `Box` rather than the value inside it.
    Boxed,
}

/// The strongest borrow among the parameters of the cell's type. `None` when
/// there are none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Bor {
    None,
    Value,
    Ref,
    RefMut,
}

/// How many parameters have the cell's type, saturating at two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Count {
    Zero,
    One,
    Two,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Oc {
    None,
    Error,
    Block,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Cell {
    arm: Arm,
    model: ModelAxis,
    /// The cell's opaque is a bridged trait, so its handle is a
    /// `Box<dyn Trait>` and a parameter of it carries an impl tag.
    dyn_trait: bool,
    recv: Recv,
    params: Count,
    borrow: Bor,
    /// Some parameter is an opaque of a *different* type — the shape that
    /// makes lock ordering and handle comparison a question at all.
    sibling: bool,
    on_contention: Oc,
    native_only: bool,
    no_block: bool,
}

/// Everything about a cell except the arm, which is derived rather than
/// declared. This is what a grid point asks for and what the round-trip check
/// compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Intent {
    model: ModelAxis,
    dyn_trait: bool,
    recv: Recv,
    params: Count,
    borrow: Bor,
    sibling: bool,
    on_contention: Oc,
    native_only: bool,
    no_block: bool,
}

impl Cell {
    fn intent(&self) -> Intent {
        Intent {
            model: self.model,
            dyn_trait: self.dyn_trait,
            recv: self.recv,
            params: self.params,
            borrow: self.borrow,
            sibling: self.sibling,
            on_contention: self.on_contention,
            native_only: self.native_only,
            no_block: self.no_block,
        }
    }
}

// ------------------------------------------------------ cells as axes --
//
// One cell is one point in a 10-axis space. The canonical form is a fixed-width
// array of small integers so that one renderer serves it, and so that the axis
// order is stated once rather than at every print site.

const AXES: usize = 10;

/// A fully specified cell.
type Key = [u8; AXES];

/// A cell with `None` on any axis meaning "every legal value here is missing".
type Pattern = [Option<u8>; AXES];

/// Per axis: its name, and its values in the order the discriminants take.
const AXIS: [(&str, &[&str]); AXES] = [
    ("arm", &["sync", "pool", "async_fn", "actor"]),
    ("model", &["-", "confined", "resident", "frozen", "locked", "actor"]),
    ("dyn", &["no", "yes"]),
    ("recv", &["-", "&self", "&mut self", "self", "self: Box<Self>"]),
    ("params", &["0", "1", "2+"]),
    ("borrow", &["-", "value", "&", "&mut"]),
    ("sibling", &["no", "yes"]),
    ("on_contention", &["-", "error", "block"]),
    ("native_only", &["no", "yes"]),
    ("no_block", &["no", "yes"]),
];

/// Column widths, so a list of patterns reads as a table.
const WIDTH: [usize; AXES] = [8, 8, 3, 15, 2, 5, 3, 5, 3, 3];

impl Cell {
    fn key(&self) -> Key {
        [
            self.arm as u8,
            self.model as u8,
            self.dyn_trait as u8,
            self.recv as u8,
            self.params as u8,
            self.borrow as u8,
            self.sibling as u8,
            self.on_contention as u8,
            self.native_only as u8,
            self.no_block as u8,
        ]
    }
}

fn render(pat: &Pattern) -> String {
    let mut out = String::new();
    for (i, v) in pat.iter().enumerate() {
        let (name, values) = AXIS[i];
        let shown = match v {
            Some(v) => values[*v as usize],
            None => "*",
        };
        out.push_str(&format!("{name}={shown:<w$} ", w = WIDTH[i]));
    }
    out.truncate(out.trim_end().len());
    out
}

impl fmt::Display for Cell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", render(&self.key().map(Some)))
    }
}


// ------------------------------------------------------- classification --

/// The opaque facts `cells_of` needs, indexed by type name.
struct Opaques(BTreeMap<String, (Model, bool)>);

impl Opaques {
    fn of(iface: &Interface) -> Self {
        Opaques(
            iface
                .opaques
                .iter()
                .map(|o| (o.name.clone(), (o.model, o.dyn_trait)))
                .collect(),
        )
    }
    fn get(&self, name: &str) -> Option<(Model, bool)> {
        self.0.get(name).copied()
    }
}

/// Every cell `f` covers. One per opaque type it touches; a single `model=-`
/// cell when it touches none.
///
/// "Touches" is by the type graph, so `Vec<T>` and `Option<T>` in a return
/// count exactly as a bare `T` does — the handles cross either way.
fn cells_of(iface: &Interface, op: &Opaques, f: &Function) -> Vec<Cell> {
    let mut touched: BTreeSet<String> = BTreeSet::new();
    let mut note = |t: &Type| {
        t.walk(&mut |t| {
            if let Type::Opaque(n) = t {
                touched.insert(n.clone());
            }
        })
    };
    for p in &f.params {
        note(&p.ty);
    }
    if let Some(r) = &f.ret {
        note(r);
    }
    // The *member's* half, not the parent's name: a type declaring two
    // representations has members on a value class that touch no handle at
    // all, and counting those as handle members puts a cell in the census
    // that the grid cannot spell.
    if let Some(o) = iface.member_handle(f) {
        touched.insert(o.name.clone());
    }

    let on_contention = match f.on_contention {
        None => Oc::None,
        Some(OnContention::Error) => Oc::Error,
        Some(OnContention::Block) => Oc::Block,
    };

    if touched.is_empty() {
        // No opaque anywhere. The parameter axes still say something — how
        // many value parameters, and how the strongest one borrows.
        let borrow = strongest(f.params.iter().map(|p| p.borrow));
        return vec![Cell {
            arm: arm_of(iface, f),
            model: ModelAxis::None,
            dyn_trait: false,
            recv: Recv::None,
            params: count(f.params.len()),
            borrow,
            sibling: false,
            on_contention,
            native_only: f.native_only,
            no_block: f.no_block,
        }];
    }

    touched
        .iter()
        .map(|name| {
            let (model, dyn_trait) = op.get(name).expect("touched names come from the IR");
            let mine: Vec<&Param> = f
                .params
                .iter()
                .filter(|p| matches!(&p.ty, Type::Opaque(n) if n == name))
                .collect();
            let sibling = f.params.iter().any(
                |p| matches!(&p.ty, Type::Opaque(n) if n != name),
            );
            Cell {
                arm: arm_of(iface, f),
                model: match model {
                    Model::Confined => ModelAxis::Confined,
                    Model::Resident => ModelAxis::Resident,
                    Model::Frozen => ModelAxis::Frozen,
                    Model::Locked => ModelAxis::Locked,
                    Model::Actor => ModelAxis::Actor,
                },
                dyn_trait,
                recv: match (
                    iface.receiver_handle(f).is_some_and(|o| o.name == *name),
                    f.receiver,
                ) {
                    (true, Some(Receiver::Ref)) => Recv::Ref,
                    (true, Some(Receiver::RefMut)) => Recv::RefMut,
                    (true, Some(Receiver::Value)) => Recv::Value,
                    (true, Some(Receiver::Boxed)) => Recv::Boxed,
                    // `cells_of` only ever reads an interface `check::check`
                    // returned, and FR0057 refuses an explicitly typed
                    // receiver that is not `Box<Self>` — so none survives to
                    // here. `emit_rust::receiver_use` and `hostile::steps`
                    // rest on the same fact.
                    (true, Some(Receiver::Typed)) => {
                        unreachable!("FR0057 refuses an explicitly typed receiver")
                    }
                    (true, None) | (false, _) => Recv::None,
                },
                params: count(mine.len()),
                borrow: strongest(mine.iter().map(|p| p.borrow)),
                sibling,
                on_contention,
                native_only: f.native_only,
                no_block: f.no_block,
            }
        })
        .collect()
}

fn count(n: usize) -> Count {
    match n {
        0 => Count::Zero,
        1 => Count::One,
        _ => Count::Two,
    }
}

fn strongest(borrows: impl Iterator<Item = Borrow>) -> Bor {
    borrows
        .map(|b| match b {
            Borrow::Value => Bor::Value,
            Borrow::Ref => Bor::Ref,
            Borrow::RefMut => Bor::RefMut,
        })
        .max()
        .unwrap_or(Bor::None)
}

/// Which arm codegen emits `f` into. The checker has already rewritten `exec`
/// where a model forces it, so this reads the finalized IR and nothing else.
fn arm_of(iface: &Interface, f: &Function) -> Arm {
    let parent_is_actor = iface
        .member_handle(f)
        .is_some_and(|o| o.model == Model::Actor);
    if parent_is_actor {
        Arm::Actor
    } else if f.rust_async {
        Arm::AsyncFn
    } else if f.exec == Exec::Sync {
        Arm::Sync
    } else {
        Arm::Pool
    }
}

// ---------------------------------------------------------------- grid --

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecDecl {
    /// `#[bridge(sync)]`
    Sync,
    /// `#[bridge]` on a plain `fn`
    Default,
    /// `#[bridge]` on an `async fn`
    AsyncFn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Zero,
    One,
    TwoSame,
    TwoDiff,
}

#[derive(Debug, Clone, Copy)]
struct Point {
    model: ModelAxis,
    dyn_trait: bool,
    exec: ExecDecl,
    recv: Recv,
    shape: Shape,
    borrow: Bor,
    on_contention: Oc,
    native_only: bool,
    no_block: bool,
}

impl Point {
    fn intent(&self) -> Intent {
        let (params, sibling) = match (self.shape, self.model) {
            (Shape::Zero, _) => (Count::Zero, false),
            (Shape::One, _) => (Count::One, false),
            (Shape::TwoSame, _) => (Count::Two, false),
            // With no opaque there is no "different opaque"; two parameters of
            // two value types are two parameters, and the cell says so.
            (Shape::TwoDiff, ModelAxis::None) => (Count::Two, false),
            (Shape::TwoDiff, _) => (Count::One, true),
        };
        Intent {
            model: self.model,
            dyn_trait: self.dyn_trait,
            recv: self.recv,
            params,
            borrow: if params == Count::Zero {
                Bor::None
            } else {
                self.borrow
            },
            sibling,
            on_contention: self.on_contention,
            native_only: self.native_only,
            no_block: self.no_block,
        }
    }
}

/// Every grid point, pruned only where a point cannot be *spelled* at all —
/// never where it looks illegal. Legality is the checker's to say.
fn grid() -> Vec<Point> {
    let mut out = vec![];
    for model in [
        ModelAxis::None,
        ModelAxis::Confined,
        ModelAxis::Resident,
        ModelAxis::Frozen,
        ModelAxis::Locked,
        ModelAxis::Actor,
    ] {
        let has_type = model != ModelAxis::None;
        for dyn_trait in [false, true] {
            // No opaque, no trait to make it out of.
            if dyn_trait && !has_type {
                continue;
            }
            for exec in [ExecDecl::Sync, ExecDecl::Default, ExecDecl::AsyncFn] {
                for recv in [Recv::None, Recv::Ref, Recv::RefMut, Recv::Value, Recv::Boxed] {
                    // A receiver needs a type to hang off.
                    if recv != Recv::None && !has_type {
                        continue;
                    }
                    for shape in [Shape::Zero, Shape::One, Shape::TwoSame, Shape::TwoDiff] {
                        for borrow in [Bor::Value, Bor::Ref, Bor::RefMut] {
                            // With no parameters the borrow axis has nothing
                            // to describe; one representative, not three.
                            if shape == Shape::Zero && borrow != Bor::Value {
                                continue;
                            }
                            for on_contention in [Oc::None, Oc::Error, Oc::Block] {
                                for native_only in [false, true] {
                                    for no_block in [false, true] {
                                        out.push(Point {
                                            model,
                                            dyn_trait,
                                            exec,
                                            recv,
                                            shape,
                                            borrow,
                                            on_contention,
                                            native_only,
                                            no_block,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    out
}

// --------------------------------------------------------- synthesis --

/// The member every synthesized source declares. Classification looks at this
/// name and nothing else, so a source may carry as much scaffolding as a
/// spelling needs.
const PROBE: &str = "probe";

fn model_word(m: ModelAxis) -> &'static str {
    match m {
        ModelAxis::None => unreachable!("no opaque to declare"),
        ModelAxis::Confined => "confined",
        ModelAxis::Resident => "resident",
        ModelAxis::Frozen => "frozen",
        ModelAxis::Locked => "locked",
        ModelAxis::Actor => "actor",
    }
}

/// Supertraits written on a synthesized bridged trait. Chosen so that a
/// rejection is attributable to the *shape* rather than to a bound this file
/// forgot; the checker still decides whether they are enough.
fn supertraits(m: ModelAxis) -> &'static str {
    match m {
        ModelAxis::Confined => ": Send",
        ModelAxis::Frozen | ModelAxis::Locked => ": Send + Sync",
        // Resident writes none, deliberately: it has no thread bound to
        // inherit (FR0021 does not apply to it), and a `Send` here would
        // quietly shrink the shapes this grid can spell to the ones confined
        // already covers.
        _ => "",
    }
}

/// The `#[bridge(...)]` attribute for the member.
fn member_attr(p: &Point) -> String {
    let mut opts: Vec<&str> = vec![];
    if p.exec == ExecDecl::Sync {
        opts.push("sync");
    }
    match p.on_contention {
        Oc::None => {}
        Oc::Error => opts.push("on_contention = \"error\""),
        Oc::Block => opts.push("on_contention = \"block\""),
    }
    if p.native_only {
        opts.push("native_only");
    }
    if p.no_block {
        opts.push("no_block");
    }
    if opts.is_empty() {
        "#[bridge]".into()
    } else {
        format!("#[bridge({})]", opts.join(", "))
    }
}

/// How a parameter of opaque `name` is spelled at this borrow.
fn param_ty(name: &str, dyn_trait: bool, borrow: Bor) -> String {
    let base = if dyn_trait {
        format!("dyn {name}")
    } else {
        name.to_string()
    };
    match borrow {
        // A trait object is unsized, so ownership of one is spelled through a
        // `Box`. That `Box` is not a node in the type — `Box<dyn T>` resolves
        // to the opaque at the name, exactly as `dyn T` behind a reference
        // does — so this is the same cell, written the only way rustc takes it.
        Bor::None | Bor::Value if dyn_trait => format!("Box<{base}>"),
        Bor::None | Bor::Value => base,
        Bor::Ref => format!("&{base}"),
        Bor::RefMut => format!("&mut {base}"),
    }
}

/// The parameter list, for a member whose cell is about type `T`.
fn params_src(p: &Point) -> String {
    if p.model == ModelAxis::None {
        // No opaque: `String` is the value type all three borrows can spell.
        let one = match p.borrow {
            Bor::None | Bor::Value => "String".to_string(),
            Bor::Ref => "&String".to_string(),
            Bor::RefMut => "&mut String".to_string(),
        };
        return match p.shape {
            Shape::Zero => String::new(),
            Shape::One => format!("p0: {one}"),
            Shape::TwoSame => format!("p0: {one}, p1: {one}"),
            Shape::TwoDiff => format!("p0: {one}, p1: i64"),
        };
    }
    let t = param_ty("T", p.dyn_trait, p.borrow);
    let u = param_ty("U", p.dyn_trait, p.borrow);
    match p.shape {
        Shape::Zero => String::new(),
        Shape::One => format!("p0: {t}"),
        Shape::TwoSame => format!("p0: {t}, p1: {t}"),
        Shape::TwoDiff => format!("p0: {t}, p1: {u}"),
    }
}

/// The opaque declarations a spelling needs. `U` only when the point asks for
/// a sibling; a spare bridged type would change nothing but is noise.
fn decls(p: &Point) -> String {
    if p.model == ModelAxis::None {
        return String::new();
    }
    let word = model_word(p.model);
    let mut out = String::new();
    let names: &[&str] = if p.shape == Shape::TwoDiff {
        &["T", "U"]
    } else {
        &["T"]
    };
    for n in names {
        if p.dyn_trait {
            out.push_str(&format!(
                "#[bridge({word})]\npub trait {n}{} {{}}\n\n",
                supertraits(p.model)
            ));
        } else {
            out.push_str(&format!(
                "#[bridge({word})]\npub struct {n} {{ pub v: i64 }}\n\n"
            ));
        }
    }
    out
}

/// `fn` or `async fn`.
fn fn_word(p: &Point) -> &'static str {
    if p.exec == ExecDecl::AsyncFn {
        "async fn"
    } else {
        "fn"
    }
}

/// Every source this file knows how to write for `p`. Tried in order; the
/// first that the checker accepts *and* that round-trips into the point's own
/// cell is what makes the cell legal.
fn spellings(p: &Point) -> Vec<String> {
    let attr = member_attr(p);
    let word = fn_word(p);
    let params = params_src(p);
    let decls = decls(p);
    let mut out = vec![];

    match p.recv {
        Recv::None => {
            // A free function, and the associated-function form of the same
            // thing. Both classify identically (the cell forgets which), so
            // whichever the checker likes is fine.
            //
            // The free form is offered only when it can actually touch the
            // cell's type — through a parameter, or (below) through its
            // return. A free `fn probe() -> i64` beside an opaque declaration
            // is a member of the `model=-` cell, not a spelling of this one,
            // and offering it would make an illegal shape look merely
            // unspellable.
            if p.model == ModelAxis::None || p.shape != Shape::Zero {
                out.push(format!(
                    "{decls}{attr}\npub {word} {PROBE}({params}) -> i64 {{ todo!() }}\n"
                ));
            }
            if p.model != ModelAxis::None && !p.dyn_trait {
                out.push(format!(
                    "{decls}#[bridge]\nimpl T {{\n    {attr}\n    pub {word} {PROBE}({params}) -> i64 {{ todo!() }}\n}}\n"
                ));
            }
            // With no receiver and no parameter of `T`, the only way the
            // member touches `T` at all is by returning it — the constructor
            // and factory shapes.
            if p.model != ModelAxis::None && p.shape == Shape::Zero {
                let ret = if p.dyn_trait {
                    "Box<dyn T>".to_string()
                } else {
                    "T".to_string()
                };
                out.push(format!(
                    "{decls}{attr}\npub {word} {PROBE}() -> {ret} {{ todo!() }}\n"
                ));
                if !p.dyn_trait {
                    out.push(format!(
                        "{decls}#[bridge]\nimpl T {{\n    {attr}\n    pub {word} {PROBE}() -> Self {{ todo!() }}\n}}\n"
                    ));
                }
                // And, last, a handle carried **inside a container**. It is a
                // spelling of this same cell and not of `Shape::One`, because
                // `cells_of` counts a parameter only where the *top level* is
                // the opaque — a `Vec<&T>` puts `T` in `touched` and adds
                // nothing to `params` or `borrow`, so its intent is this one.
                //
                // Offered after the constructors precisely because it is the
                // weaker claim about the cell: where a constructor spells the
                // point, that is the emission recorded. It is what rescues the
                // points the constructors cannot reach — `on_contention` is
                // FR0010 on a member that acquires nothing, and a constructor
                // acquires nothing, while a lent container does.
                let inner = if p.dyn_trait { "&dyn T" } else { "&T" };
                out.push(format!(
                    "{decls}{attr}\npub {word} {PROBE}(p0: Vec<{inner}>) -> i64 {{ todo!() }}\n"
                ));
            }
            // Last: the member hangs off the **sibling**, and reaches this
            // cell's type only through a parameter. `survey` matches the
            // intent against every cell the probe covers, and `U::probe(&self,
            // p0: U, p1: T)` covers `T`'s cell as exactly this point: no
            // receiver of its own, one parameter of its type, one parameter of
            // a different opaque.
            //
            // What it reaches that the free function above cannot: a member
            // with a receiver **acquires**, and `on_contention` is FR0010 on a
            // call that acquires nothing. Two handles taken by value and no
            // receiver take no guard, so the free form can never spell a
            // contended one.
            //
            // Hung off `U` rather than `T` on purpose. The `T` spelling is the
            // `Recv::Ref` point's own source, so both points would record one
            // interface, one emission and therefore one class — and a class
            // mixing two `recv` values reads as the emitter having stopped
            // distinguishing them, which is the thing
            // [`tests::a_class_differs_in_at_most_one_declared_fact`] is for.
            if p.model != ModelAxis::None && p.shape == Shape::TwoDiff {
                let t = param_ty("T", p.dyn_trait, p.borrow);
                let u = param_ty("U", p.dyn_trait, p.borrow);
                if p.dyn_trait {
                    let word_t = model_word(p.model);
                    let sup = supertraits(p.model);
                    out.push(format!(
                        "#[bridge({word_t})]\npub trait T{sup} {{}}\n\n\
                         #[bridge({word_t})]\npub trait U{sup} {{\n    {attr}\n    \
                         {word} {PROBE}(&self, p0: {u}, p1: {t}) -> i64;\n}}\n"
                    ));
                } else {
                    out.push(format!(
                        "{decls}#[bridge]\nimpl U {{\n    {attr}\n    \
                         pub {word} {PROBE}(&self, p0: {u}, p1: {t}) -> i64 {{ todo!() }}\n}}\n"
                    ));
                }
            }
        }
        Recv::Ref | Recv::RefMut | Recv::Value | Recv::Boxed => {
            let this = match p.recv {
                Recv::Ref => "&self",
                Recv::RefMut => "&mut self",
                Recv::Value => "self",
                Recv::Boxed => "self: Box<Self>",
                Recv::None => unreachable!("the receiverless forms are the arm above"),
            };
            let sig_params = if params.is_empty() {
                this.to_string()
            } else {
                format!("{this}, {params}")
            };
            if p.dyn_trait {
                // A bridged trait's members are declared on the trait; the
                // sibling `U` is declared alongside by `decls`.
                let word_t = model_word(p.model);
                let sup = supertraits(p.model);
                let extra = if p.shape == Shape::TwoDiff {
                    format!("#[bridge({word_t})]\npub trait U{sup} {{}}\n\n")
                } else {
                    String::new()
                };
                out.push(format!(
                    "{extra}#[bridge({word_t})]\npub trait T{sup} {{\n    {attr}\n    {word} {PROBE}({sig_params}) -> i64;\n}}\n"
                ));
            } else {
                out.push(format!(
                    "{decls}#[bridge]\nimpl T {{\n    {attr}\n    pub {word} {PROBE}({sig_params}) -> i64 {{ todo!() }}\n}}\n"
                ));
            }
        }
    }
    out
}

// ------------------------------------------------------------ the run --

struct Survey {
    legal: BTreeSet<Cell>,
    /// The checked interface each legal cell landed from, kept so the emitter
    /// can be run over it. `check::check` has already called `finalize`, so
    /// these are emit-ready. Only [`emission_classes`] reads it.
    ifaces: BTreeMap<Cell, Interface>,
    /// The grid point and the exact spelling that landed each legal cell —
    /// what `--write-fixture` reuses. Kept rather than regenerated because
    /// `spellings` offers several forms per point and only one of them is the
    /// one the checker took to this cell; guessing again later would be a
    /// second, silently different, choice.
    spelling: BTreeMap<Cell, (Point, String)>,
    /// FR code -> how many grid points every spelling of which it refused.
    refused: BTreeMap<String, usize>,
    /// The distinct messages `parse_source` gave to points nothing else
    /// explains. Kept rather than counted: a parse refusal is the one outcome
    /// that can mean this file wrote bad Rust, and the message is what says so.
    parse_refused: BTreeMap<String, usize>,
    /// Points a spelling of which the checker accepted, but which classified
    /// into some other cell — so this file cannot write the shape it meant.
    /// Listed rather than counted: each one is a hole in the grid.
    misspelled: Vec<Point>,
}

fn survey() -> Survey {
    let mut legal = BTreeSet::new();
    let mut ifaces: BTreeMap<Cell, Interface> = BTreeMap::new();
    let mut spelling: BTreeMap<Cell, (Point, String)> = BTreeMap::new();
    let mut refused: BTreeMap<String, usize> = BTreeMap::new();
    let mut parse_refused: BTreeMap<String, usize> = BTreeMap::new();
    let mut misspelled = vec![];

    for p in grid() {
        let want = p.intent();
        let mut codes: BTreeSet<String> = BTreeSet::new();
        let mut parse_errs: Vec<String> = vec![];
        let mut saw_accept = false;
        let mut landed = None;

        for src in spellings(&p) {
            let parsed = match parse::parse_source(&src, "crate::api") {
                Ok(i) => i,
                Err(e) => {
                    parse_errs.push(e.to_string());
                    continue;
                }
            };
            // `parse_source` leaves `crate_name` empty, and it lands in the
            // emitted header — so the emission of two identical shapes would
            // differ only in a blank. `merge` is what fills it in the real
            // flow; giving every probe the same name keeps the emitter's output
            // a function of the shape alone.
            let parsed = parse::merge(PROBE, vec![parsed]);
            let iface = match check::check(parsed) {
                Ok(i) => i,
                Err(diags) => {
                    codes.extend(diags.iter().map(|d| d.code.to_string()));
                    continue;
                }
            };
            saw_accept = true;
            let op = Opaques::of(&iface);
            let probe = iface
                .functions
                .iter()
                .find(|f| f.name == PROBE)
                .expect("every spelling declares the probe member");
            if let Some(cell) = cells_of(&iface, &op, probe).into_iter().find(|c| c.intent() == want) {
                ifaces.insert(cell, iface.clone());
                spelling.insert(cell, (p, src.clone()));
                landed = Some(cell);
                break;
            }
        }

        match landed {
            Some(cell) => {
                legal.insert(cell);
            }
            // The three ways a point produces no cell, kept apart because they
            // mean different things: the shape was refused, or this file wrote
            // Rust the parser will not take, or it wrote the wrong shape.
            None if saw_accept => misspelled.push(p),
            None if !codes.is_empty() => {
                for c in codes {
                    *refused.entry(c).or_default() += 1;
                }
            }
            None => {
                for e in parse_errs {
                    *parse_refused.entry(e).or_default() += 1;
                }
            }
        }
    }

    Survey {
        legal,
        ifaces,
        spelling,
        refused,
        parse_refused,
        misspelled,
    }
}

/// Group legal cells by the code the emitter actually produces for them.
///
/// The premise the whole coverage question rests on: a *cell* is an assignment
/// of declared facts, and many assignments produce identical dispatch. Covering
/// any member of such a group covers all of it, because there is only one code
/// path there to compile or run. Which axes fold is not argued here — the
/// emitter's own output decides, so a change that makes an axis start mattering
/// re-splits the group without anyone remembering to.
///
/// Only `emit_rust` is run. The Dart surface is a function of the same IR and
/// would add cost without adding a distinct path to cover.
fn emission_classes(s: &Survey) -> BTreeMap<u64, BTreeSet<Cell>> {
    let mut classes: BTreeMap<u64, BTreeSet<Cell>> = BTreeMap::new();
    for (cell, iface) in &s.ifaces {
        let code = normalize_emission(&frustrate_codegen::emit_rust::emit(iface));
        classes.entry(fnv1a(&code)).or_default().insert(*cell);
    }
    classes
}

/// Strip the two things that vary between emissions of the *same* shape.
///
/// `fn_id`s are assigned by `finalize` in declaration order, so a shape whose
/// spelling needed an extra constructor numbers its probe differently while
/// emitting the same dispatch. The schema hash is a digest of the whole
/// interface and so differs whenever anything does, including the parts that
/// produce no code.
///
/// Deliberately nothing else. Identifiers are already uniform — `spellings`
/// emits `T`, `U` and `probe` for every point — so `f(&T, &T)` and `f(&T, &U)`
/// stay textually distinct, which they must: aliasing is the difference between
/// them and it is exactly what the emitted acquisition encodes.
fn normalize_emission(code: &str) -> String {
    let mut out = String::with_capacity(code.len());
    for line in code.lines() {
        if line.contains("frustrate_schema_hash") {
            continue;
        }
        let mut rest = line;
        while let Some(i) = ["call_", "spawn_", "actor_"]
            .iter()
            .filter_map(|p| rest.find(p).map(|i| i + p.len()))
            .min()
        {
            let (head, tail) = rest.split_at(i);
            out.push_str(head);
            let digits = tail.len() - tail.trim_start_matches(|c: char| c.is_ascii_digit()).len();
            if digits > 0 {
                out.push('N');
            }
            rest = &tail[digits..];
        }
        out.push_str(rest);
        out.push('\n');
    }
    out
}

// ------------------------------------------------------- fixture writing --

/// The opaque types the generated fixture hangs its members off.
///
/// One pair per (model, `dyn`) that any uncovered path needs — a primary and a
/// *sibling*, because `sibling` is a property of a member's parameter list (an
/// opaque of a **different** type), not of the file. Sharing them is therefore
/// safe and keeps the count at a handful rather than one per member, which
/// matters twice over: every generated opaque needs a constructor `Env::mint`
/// will accept, and every one it cannot mint turns its members into
/// `Skip::NoHandle` and quietly undrives them.
fn shared_ty(model: ModelAxis, dyn_trait: bool, sibling: bool) -> String {
    let m = model_word(model);
    let d = if dyn_trait { "dyn" } else { "s" };
    let n = if sibling { "b" } else { "a" };
    format!("Shape{}{}{}", cap(m), cap(d), cap(n))
}

fn cap(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Declare one shared opaque and the constructor that makes it mintable.
///
/// `#[bridge(sync)]`, no parameters, infallible: exactly the shape
/// `Env::mint` looks for (sync, not `rust_async`, returns the opaque, no
/// receiver, no opaque parameters). A fallible or parameterised constructor
/// would still compile and would still be *legal* — it would just never be
/// minted, and the members hanging off it would be reported driven when they
/// were skipped.
fn shared_decl(model: ModelAxis, dyn_trait: bool, sibling: bool) -> String {
    let ty = shared_ty(model, dyn_trait, sibling);
    let word = model_word(model);
    let ctor = format!("new_{}", ty.to_lowercase());
    if dyn_trait {
        let imp = format!("{ty}Impl");
        format!(
            "#[bridge({word})]\npub trait {ty}{} {{}}\n\n\
             pub struct {imp};\nimpl {ty} for {imp} {{}}\n\n\
             #[bridge(sync)]\npub fn {ctor}() -> Box<dyn {ty}> {{ Box::new({imp}) }}\n\n",
            supertraits(model)
        )
    } else if model == ModelAxis::Actor {
        // FR0015 refuses a free constructor (each spawn creates an executor),
        // and FR0014 refuses a sync one. So an actor's constructor is
        // necessarily async, and `Env::mint` — which asks for `exec == Sync` —
        // can never mint one.
        //
        // That costs nothing here, and it is worth writing down why. The
        // hostile pass drives **sync arms only**: a pool, executor or actor arm
        // answers through `post` on another thread, where the driver cannot
        // read it (hostile.rs, "What it cannot reach"). An actor member was
        // never going to be driven whatever its constructor looked like, which
        // is already true of the hand-written `Miner`. What the fixture buys
        // for these members is the other half — codegen emits them and rustc
        // compiles them, which is the half that caught all three of the defects
        // this tool exists for.
        format!(
            "#[bridge({word})]\npub struct {ty} {{ pub v: i64 }}\n\n\
             #[bridge]\nimpl {ty} {{\n    \
             pub fn {ctor}() -> Self {{ {ty} {{ v: 0 }} }}\n}}\n\n"
        )
    } else {
        format!(
            "#[bridge({word})]\npub struct {ty} {{ pub v: i64 }}\n\n\
             #[bridge(sync)]\npub fn {ctor}() -> {ty} {{ {ty} {{ v: 0 }} }}\n\n"
        )
    }
}

/// Rewrite one landed spelling into a member of the shared fixture.
///
/// The spelling is machine-generated with known identifiers (`T`, `U`,
/// `probe`), so a word-boundary rename is exact rather than a guess at parsing
/// Rust. `todo!()` becomes a real value for the reason the plan gives: a
/// `todo!()` body panics before the response is encoded, so the return path —
/// the half most likely to be wrong — never runs.
enum FixtureItem {
    /// A free function, an inherent `impl` block, or a factory — emitted as-is.
    Item(String),
    /// A bridged trait's member. `spellings` declares these *inside* the trait,
    /// so they cannot stand alone: every `dyn` member of one model has to be
    /// folded into a single trait declaration and a single impl, or each would
    /// redeclare the type (FR0002).
    TraitMethod {
        model: ModelAxis,
        decl: String,
        imp: String,
    },
}

fn fixture_member(cell: &Cell, point: &Point, src: &str, n: usize) -> FixtureItem {
    let body = src
        .strip_prefix(&decls(point))
        .unwrap_or(src)
        .trim_start()
        .to_string();
    // `model = -` is a member that names no opaque at all, so `T`/`U` never
    // appear in its spelling and there is nothing to point at.
    let (prim, sib) = if cell.model == ModelAxis::None {
        (String::new(), String::new())
    } else {
        (
            shared_ty(cell.model, cell.dyn_trait, false),
            shared_ty(cell.model, cell.dyn_trait, true),
        )
    };
    let name = format!("{PROBE}_{n}");
    let mut out = String::new();
    for tok in split_idents(&body) {
        match tok.as_str() {
            "T" => out.push_str(&prim),
            "U" => out.push_str(&sib),
            PROBE => out.push_str(&name),
            _ => out.push_str(&tok),
        }
    }
    // `Self` inside an `impl` block already names the shared type; a
    // constructor form returning it needs a value, not a panic.
    out = out.replace("-> Self { todo!() }", "-> Self { Self { v: 0 } }");
    out = out.replace(
        &format!("-> Box<dyn {prim}> {{ todo!() }}"),
        &format!("-> Box<dyn {prim}> {{ Box::new({prim}Impl) }}"),
    );
    out = out.replace(
        &format!("-> {prim} {{ todo!() }}"),
        &format!("-> {prim} {{ {prim} {{ v: 0 }} }}"),
    );
    out = out.replace("-> i64 { todo!() }", "-> i64 { 0 }");

    // Whether this has to be folded into the shared trait is a question about
    // the **spelling**, not about the cell: a member with a receiver on one
    // bridged trait covers the cell of a *sibling* trait it takes as a
    // parameter, and that cell has no receiver of its own. So ask what is left
    // after the shared declarations are stripped — only a spelling that
    // declares its own trait still carries `pub trait`, and every other dyn
    // form (the factory, the lent container) has had its `decls` removed above.
    if cell.dyn_trait && out.contains("pub trait") {
        // Take what sits between the trait's braces. The declaration keeps the
        // `;`; the impl needs a body, and no `#[bridge]` attribute (the trait
        // declares the member, the impl only supplies it).
        // The LAST `pub trait` is the one carrying the probe: a sibling
        // parameter makes `spellings` declare `U` first, and anchoring on the
        // first brace would swallow that declaration whole.
        let at = out.rfind("pub trait").expect("a dyn member declares its trait");
        let open = out[at..]
            .find('{')
            .map(|i| at + i)
            .expect("a trait declaration has a body");
        let close = out.rfind('}').expect("a trait declaration is closed");
        let inner = out[open + 1..close].trim().to_string();
        let decl = format!("    {inner}
");
        let imp = inner
            .lines()
            .filter(|l| !l.trim_start().starts_with("#["))
            .map(|l| format!("    {}", l.trim()))
            .collect::<Vec<_>>()
            .join("
")
            .replace("-> i64;", "-> i64 { 0 }");
        return FixtureItem::TraitMethod {
            model: cell.model,
            decl,
            imp: format!("{imp}
"),
        };
    }
    FixtureItem::Item(out)
}

/// Split into identifier and non-identifier runs, so a rename is on whole
/// words. `T` must not match the `T` inside `TryFrom`, and `probe` must not
/// match a longer name that contains it.
fn split_idents(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut in_ident = false;
    for c in s.chars() {
        let is_ident = c.is_alphanumeric() || c == '_';
        if is_ident != in_ident && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        in_ident = is_ident;
        cur.push(c);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Write `tests/test_api/src/shapes.rs`: one member per emitted code path that
/// nothing instantiates.
///
/// Generated rather than hand-written because the set is derived — it is
/// whatever the emitter and the checker between them say is uncovered today,
/// and re-running this is how it stays true. The file is registered in three
/// places by hand (`build.rs`, `BUILD.bazel`, `lib.rs`); both build flows must
/// scan the same set or fn ids and the schema hash diverge.
fn write_fixture(survey: &Survey, covered: &BTreeSet<Cell>, out: &Path) -> std::io::Result<usize> {
    let groups = emission_classes(survey);
    let mut reps: Vec<&Cell> = groups
        .values()
        .filter(|g| !g.iter().any(|c| covered.contains(c)))
        .filter_map(|g| g.iter().next())
        .filter(|c| exempt(c).is_none())
        .collect();
    reps.sort();

    let mut kinds: BTreeSet<(ModelAxis, bool)> = BTreeSet::new();
    for c in &reps {
        if c.model != ModelAxis::None {
            kinds.insert((c.model, c.dyn_trait));
        }
    }

    let mut f = String::new();
    f.push_str(
        "// @generated by `cargo run -p shape-coverage -- --write-fixture`. Do not edit.\n\
         //\n\
         // A parameter exists here to make a *shape*, and a body that used it\n\
         // would be making a claim about behaviour this file does not test. The\n\
         // allow is on the module rather than per item so the generator has no\n\
         // reason to guess which members need it.\n\
         // `ptr_arg` fires on `&String`, which is a *cell* here — `borrow = &` on\n\
         // a non-opaque parameter — not an oversight. Rewriting it to `&str`\n\
         // would silence the lint by deleting the shape under test. `boxed_local`\n\
         // fires on `self: Box<Self>` for the same reason: the box is the cell,\n\
         // and the body has no use for it because these bodies use nothing.\n\
         #![allow(unused_variables, dead_code, clippy::ptr_arg, clippy::boxed_local)]\n\
         //\n\
         // One member per emitted code path that nothing else in this repo\n\
         // instantiates. The point is not that anyone reads these: it is that each\n\
         // one is a dispatch the codegen emits, the compiler compiles, and the\n\
         // hostile-request pass drives — so a path that was never exercised cannot\n\
         // stay that way silently.\n\
         //\n\
         // Regenerate rather than edit. The set is derived from the checker and the\n\
         // emitter, so a change to either is meant to change this file.\n\n\
         use frustrate::bridge;\n\n",
    );
    for (model, dyn_trait) in &kinds {
        // A `dyn` primary is declared with its members below; declaring it here
        // too would be FR0002. Its sibling never carries a member, so it is
        // declared here either way.
        if !dyn_trait {
            f.push_str(&shared_decl(*model, *dyn_trait, false));
        }
        f.push_str(&shared_decl(*model, *dyn_trait, true));
    }
    // Trait members are collected per model and emitted as one declaration and
    // one impl; everything else stands alone.
    let mut items: Vec<String> = vec![];
    let mut trait_decls: BTreeMap<ModelAxis, Vec<String>> = BTreeMap::new();
    let mut trait_impls: BTreeMap<ModelAxis, Vec<String>> = BTreeMap::new();
    for (n, cell) in reps.iter().enumerate() {
        let (point, src) = survey
            .spelling
            .get(*cell)
            .expect("every legal cell recorded the spelling that landed it");
        match fixture_member(cell, point, src, n) {
            FixtureItem::Item(t) => items.push(t),
            FixtureItem::TraitMethod { model, decl, imp } => {
                trait_decls.entry(model).or_default().push(decl);
                trait_impls.entry(model).or_default().push(imp);
            }
        }
    }
    for (model, decls) in &trait_decls {
        let ty = shared_ty(*model, true, false);
        f.push_str(&format!(
            "#[bridge({})]\npub trait {ty}{} {{\n{}}}\n\n",
            model_word(*model),
            supertraits(*model),
            decls.join("")
        ));
        f.push_str(&format!(
            "pub struct {ty}Impl;\nimpl {ty} for {ty}Impl {{\n{}}}\n\n\
             #[bridge(sync)]\npub fn new_{}() -> Box<dyn {ty}> {{ Box::new({ty}Impl) }}\n\n",
            trait_impls[model].join(""),
            ty.to_lowercase()
        ));
    }
    for t in items {
        f.push_str(&t);
        f.push('\n');
    }
    std::fs::write(out, f)?;

    // Step 5: the reclaim rows for the types this just wrote, as a fragment
    // `hostile.rs` includes. Hand-written rows for hand-written types stay hand
    // written; these are derived, because a regenerated fixture would otherwise
    // fail `every_mintable_opaque_has_a_reclaim` every time and the fix would
    // be to paste names into a list — which is the thing this file exists to
    // stop anyone doing.
    let mut rows = String::from(
        "// @generated by `cargo run -p shape-coverage -- --write-fixture`. Do not edit.\n\
         //\n\
         // One row per opaque `shapes.rs` declares, included by `hostile.rs`'s\n\
         // `reclaim_table`. Derived for the reason the fixture is: a hand-kept list\n\
         // beside a generated set is a list that goes stale on the next regeneration.\n[\n",
    );
    for (model, dyn_trait) in &kinds {
        let mut tys = vec![shared_ty(*model, *dyn_trait, true)];
        if !dyn_trait {
            tys.push(shared_ty(*model, *dyn_trait, false));
        }
        if *dyn_trait && trait_decls.contains_key(model) {
            tys.push(shared_ty(*model, true, false));
        }
        for ty in tys {
            // Actors are reclaimed by their own teardown, and the table is
            // asserted against the interface's *non-actor* opaques.
            if *model == ModelAxis::Actor {
                continue;
            }
            rows.push_str(&format!(
                "    (\"{ty}\", g::frustrate_drop_{ty} as Reclaim),\n"
            ));
        }
    }
    rows.push_str("]\n");
    std::fs::write(out.with_file_name("shapes_reclaim.rs"), rows)?;
    Ok(reps.len())
}

/// Classes no fixture can instantiate, each with the reason and the condition
/// that would make it instantiable again.
///
/// Green means "every class is covered **or** explicitly accounted for". The
/// table earns that only if it cannot rot, so an entry whose class turns out to
/// be instantiated fails the run rather than being quietly tolerated — see
/// [`stale_exemptions`].
///
/// One entry, and its history is the argument for the discipline. This table
/// was designed for `locked` + `async fn`, which looked permanent — the emitted
/// future held a `std` guard across the user body's await and `executor::spawn`
/// requires `Send`. That turned out to be a defect in frustrate rather than a
/// property of the shape, and fixing the lock made the cell instantiable. The
/// entry below is the genuine article, and the difference is worth stating:
/// nothing frustrate does can fix it, because it is not frustrate's to fix.
fn exempt(cell: &Cell) -> Option<&'static str> {
    if cell.dyn_trait && cell.arm == Arm::AsyncFn {
        return Some(
            "a bridged trait's handle is `Box<dyn Trait>`, and a trait with an \
             `async fn` member is not dyn compatible (E0038): the return type is \
             opaque per impl, so there is no vtable to build. Not a frustrate \
             limitation — a user's own crate cannot name `Box<dyn Trait>` either, \
             and rustc says so at their declaration (\"...because method `f` is \
             `async`\"). Instantiable again if Rust gains dyn-compatible async \
             trait methods, or if frustrate accepts the boxed-future spelling \
             (`fn f(&self) -> Pin<Box<dyn Future<Output = T> + Send + '_>>`) as \
             an async member",
        );
    }
    None
}

/// Exemptions that have stopped being true: a class the table calls
/// uninstantiable that this repo nonetheless instantiates.
///
/// This is what stops the table from becoming a list of things nobody has
/// rechecked. It is also not hypothetical — the entry this table was written
/// for became instantiable within a week of being written.
fn stale_exemptions<'a>(
    groups: &'a BTreeMap<u64, BTreeSet<Cell>>,
    covered: &BTreeSet<Cell>,
) -> Vec<&'a Cell> {
    groups
        .values()
        .filter_map(|g| {
            let rep = g.iter().next()?;
            if exempt(rep).is_some() && g.iter().any(|c| covered.contains(c)) {
                Some(rep)
            } else {
                None
            }
        })
        .collect()
}

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// Parse and check the repo's own bridge sources exactly as its build does,
/// and classify every member the checked interface contains.
fn instantiated(sources: &[(PathBuf, &str)]) -> Result<BTreeSet<Cell>, String> {
    let mut parts = vec![];
    for (path, module) in sources {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("reading {}: {e}", path.display()))?;
        parts.push(
            parse::parse_source(&text, module)
                .map_err(|e| format!("parsing {}: {e}", path.display()))?,
        );
    }
    let iface = check::check(parse::merge("test_api", parts))
        .map_err(|d| d.iter().map(|d| d.to_string()).collect::<Vec<_>>().join("\n"))?;
    let op = Opaques::of(&iface);
    Ok(iface
        .functions
        .iter()
        .flat_map(|f| cells_of(&iface, &op, f))
        .collect())
}

/// The bridge sources this repo declares, as its build.rs and its Bazel target
/// both name them.
/// The generated fixture, excluded when deciding what the fixture should
/// contain. Reading it back while writing it makes the file cover itself: the
/// second run finds nothing uncovered and truncates it to nothing.
const FIXTURE: &str = "tests/test_api/src/shapes.rs";

fn repo_sources_less_fixture(root: &Path) -> Vec<(PathBuf, &'static str)> {
    repo_sources(root)
        .into_iter()
        .filter(|(p, _)| !p.ends_with("shapes.rs"))
        .collect()
}

fn repo_sources(root: &Path) -> Vec<(PathBuf, &'static str)> {
    vec![
        (
            root.join("tests/test_api/src/api.rs"),
            "crate::api",
        ),
        (
            root.join("tests/test_api/src/cancel_api.rs"),
            "crate::cancel_api",
        ),
        // Written by `--write-fixture`, and read back here: what the tool
        // generated is what the tool then judges, so a member it failed to
        // generate correctly shows up as still-missing rather than as covered.
        (
            root.join("tests/test_api/src/shapes.rs"),
            "crate::shapes",
        ),
    ]
}

const USAGE: &str = "\
Reports member shapes the checker accepts and this repo's bridge sources never
instantiate.

  cargo run -p shape-coverage                  the gate
  cargo run -p shape-coverage -- --census      every legal cell, covered or not
  cargo run -p shape-coverage -- --write-fixture
                                               regenerate tests/test_api/src/shapes.rs

Coverage is per emitted code path, not per cell: several cells can emit the
same dispatch, and instantiating any of them covers all of them. Exit status is
0 only when every path has an instance or an entry in the exemption table.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut census = false;
    let mut write_fixture_flag = false;
    for a in &args {
        match a.as_str() {
            "--census" => census = true,
            "--write-fixture" => write_fixture_flag = true,
            "--help" | "-h" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("shape_coverage: unrecognised {other}\n\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }

    // The repo root, from this crate's own manifest rather than from the
    // working directory, so the gate answers the same wherever it is run.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("tools/shape_coverage sits two levels below the repo root")
        .to_path_buf();

    let survey = survey();

    if write_fixture_flag {
        let out = root.join(FIXTURE);
        let base = match instantiated(&repo_sources_less_fixture(&root)) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("shape_coverage: the hand-written bridge sources do not check:\n{e}");
                return ExitCode::from(2);
            }
        };
        match write_fixture(&survey, &base, &out) {
            Ok(n) => {
                println!("shape coverage: wrote {n} member(s) to {}", out.display());
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                eprintln!("shape_coverage: could not write the fixture: {e}");
                return ExitCode::from(2);
            }
        }
    }

    let covered = match instantiated(&repo_sources(&root)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("shape_coverage: the repo's own bridge sources do not check:\n{e}");
            return ExitCode::from(2);
        }
    };

    let outside: Vec<&Cell> = covered.difference(&survey.legal).collect();


    if census {
        // Cell-level, deliberately, and it is a different question from the
        // gate's: the gate asks whether each emitted *path* has an instance,
        // and several cells can share one. A cell marked `??` here whose path
        // is covered is not a gap — it is the collapse doing its job. `xx`
        // marks a cell the exemption table accounts for.
        println!(
            "legal cells ({}) — `??` = this cell has no instance of its own, \
             which is not a gap\n  when another cell emitting the same code does; \
             `xx` = exempt.",
            survey.legal.len()
        );
        for c in &survey.legal {
            let mark = if covered.contains(c) {
                "   "
            } else if exempt(c).is_some() {
                "xx "
            } else {
                "?? "
            };
            println!("  {mark}{c}");
        }
        println!("\ngrid points with no legal spelling, by FR code:");
        for (code, n) in &survey.refused {
            println!("  {n:>5}  {code}");
        }
        if !survey.parse_refused.is_empty() {
            println!("\nspellings the parser refused (this file's Rust, not a shape):");
            for (msg, n) in &survey.parse_refused {
                println!("  {n:>5}  {msg}");
            }
        }
        if !survey.misspelled.is_empty() {
            println!(
                "\ngrid points the checker accepted but that classified into some \
                 other cell\n(this file cannot spell what it meant, so the shape is \
                 not in the legal set):"
            );
            for p in &survey.misspelled {
                println!("  {p:?}");
            }
        }
        println!();
    }

    // Coverage is per *emitted code path*, not per cell. Two cells whose
    // dispatch is byte-identical are one thing to compile and one thing to run,
    // so instantiating either covers both — and which cells those are is
    // decided by the emitter's own output, not by an axis declared irrelevant
    // here (see `emission_classes`).
    let groups = emission_classes(&survey);
    let stale = stale_exemptions(&groups, &covered);
    let uncovered: Vec<&BTreeSet<Cell>> = groups
        .values()
        .filter(|g| !g.iter().any(|c| covered.contains(c)))
        .filter(|g| g.iter().next().is_none_or(|c| exempt(c).is_none()))
        .collect();
    let exempted = groups
        .values()
        .filter(|g| g.iter().next().is_some_and(|c| exempt(c).is_some()))
        .count();
    println!(
        "shape coverage: {} legal cells over {} emitted code path(s), \
         {} path(s) with no instantiated member",
        survey.legal.len(),
        groups.len(),
        uncovered.len()
    );
    if exempted > 0 {
        println!("  ({exempted} path(s) accounted for as uninstantiable — see `exempt`)");
    }
    if !stale.is_empty() {
        // Not "one more missing cell": an exemption that has stopped being true
        // is worse than a gap, because it is a claim the gate is trusting.
        println!(
            "\n{} exemption(s) are stale — the table calls these uninstantiable and \
             this repo\ninstantiates them. Delete the entry:",
            stale.len()
        );
        for c in &stale {
            println!("  {c}");
        }
        return ExitCode::FAILURE;
    }
    if !outside.is_empty() {
        // Not a failure: it measures this file's grid against the sources it
        // judges, and a grid narrower than the sources under-reports rather
        // than over-reports.
        println!(
            "  ({} cell(s) this repo instantiates lie outside the grid — see the \
             module header, \"What it cannot decide\")",
            outside.len()
        );
        for c in &outside {
            println!("     outside: {c}");
        }
    }
    if uncovered.is_empty() {
        return ExitCode::SUCCESS;
    }
    // One representative per uncovered path. Printing every cell would print
    // the same code path twice under different declared facts, which is the
    // report this tool used to give and the reason it could not be acted on.
    println!(
        "\n{} emitted code path(s) nothing instantiates, one representative each:",
        uncovered.len()
    );
    for g in &uncovered {
        let rep = g.iter().next().expect("a class is never empty");
        let others = g.len() - 1;
        if others == 0 {
            println!("  {rep}");
        } else {
            println!("  {rep}   (+{others} cell(s) emitting the same code)");
        }
    }
    println!(
        "\nEach line is a distinct dispatch this repo has never compiled or run. \
         Instantiate\none member of it in tests/test_api, or make the checker refuse \
         the shape."
    );
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {

    /// The premise the gate rests on: cells that emit the same code are one
    /// path. If normalization ever over-collapsed, coverage would claim paths
    /// nothing compiles; if it under-collapsed, the fixture would grow members
    /// that add nothing. Both are silent, so both are pinned.
    #[test]
    fn a_class_differs_in_at_most_one_declared_fact() {
        let s = survey();
        let groups = emission_classes(&s);
        for g in groups.values() {
            // Small, because only two axes can fold: `no_block`, which never
            // reaches the dispatch body, and `native_only` where the member is
            // *already* native-only by derivation (an `on_contention = "block"`
            // contract, say) so declaring it changes no cfg. A class larger
            // than that means normalization erased something the emitter reads.
            assert!(g.len() <= 2, "over-collapsed class: {g:?}");
            if g.len() == 2 {
                let mut it = g.iter();
                let (a, b) = (it.next().unwrap(), it.next().unwrap());
                let diffs = usize::from(a.no_block != b.no_block)
                    + usize::from(a.native_only != b.native_only);
                assert_eq!(diffs, 1, "a pair differing other than in one foldable axis: {a} / {b}");
                // The axes that certainly change the emitted dispatch never do.
                assert_eq!(a.arm, b.arm, "{a} / {b}");
                assert_eq!(a.model, b.model, "{a} / {b}");
                assert_eq!(a.recv, b.recv, "{a} / {b}");
                assert_eq!(a.params, b.params, "{a} / {b}");
                assert_eq!(a.borrow, b.borrow, "{a} / {b}");
                assert_eq!(a.dyn_trait, b.dyn_trait, "{a} / {b}");
                assert_eq!(a.on_contention, b.on_contention, "{a} / {b}");
            }
        }
    }

    /// Normalization must erase what differs between emissions of the *same*
    /// shape and nothing else. `fn_id`s move when a spelling needs an extra
    /// constructor; the schema hash moves whenever any declared fact does.
    #[test]
    fn normalization_erases_ids_and_the_hash_only() {
        let a = normalize_emission(
            "fn call_0_probe(r: &mut ByteReader) {}\npub extern \"C\" fn frustrate_schema_hash() -> u64 { 0x1 }\n",
        );
        let b = normalize_emission(
            "fn call_7_probe(r: &mut ByteReader) {}\npub extern \"C\" fn frustrate_schema_hash() -> u64 { 0x2 }\n",
        );
        assert_eq!(a, b, "fn id and schema hash must not distinguish two emissions");
        // But a real difference still is one.
        let c = normalize_emission("fn call_0_probe(r: &mut ByteWriter) {}\n");
        assert_ne!(a, c, "a genuine difference must survive normalization");
    }

    /// Aliasing is the difference between `f(&T, &T)` and `f(&T, &U)`, and it
    /// is exactly what the emitted acquisition encodes — so the two must never
    /// land in one class. A rename that mapped both type names to one token
    /// would merge them silently.
    #[test]
    fn a_repeated_opaque_is_not_the_same_path_as_two_distinct_ones() {
        let s = survey();
        let groups = emission_classes(&s);
        for g in groups.values() {
            let siblings: Vec<bool> = g.iter().map(|c| c.sibling).collect();
            assert!(
                siblings.iter().all(|x| *x == siblings[0]),
                "a class mixing repeated and distinct opaques: {g:?}"
            );
        }
    }

    /// The exemption table is a claim the gate trusts, so it has to be a claim
    /// somebody made — an empty table would let `exempt` become decoration
    /// while the gate still reported paths "accounted for".
    #[test]
    fn the_exemption_table_is_not_empty() {
        let s = survey();
        assert!(
            s.legal.iter().any(|c| exempt(c).is_some()),
            "no legal cell is exempt: delete `exempt` and its reporting, or fix it"
        );
    }

    /// And every entry must still be uninstantiable. This is the anti-rot
    /// property, and it is not hypothetical: the entry this table was designed
    /// for (`locked` + `async fn`) became instantiable once codegen stopped
    /// handing a blocking lock to a cooperative executor.
    #[test]
    fn no_exemption_has_gone_stale() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("two levels below the repo root")
            .to_path_buf();
        let covered = instantiated(&repo_sources(&root)).expect("the repo's sources check");
        let s = survey();
        let groups = emission_classes(&s);
        let stale = stale_exemptions(&groups, &covered);
        assert!(stale.is_empty(), "exemptions that are no longer true: {stale:?}");
    }

    use super::*;

    fn checked(src: &str) -> Interface {
        check::check(parse::parse_source(src, "crate::api").expect("parses")).expect("checks")
    }

    fn cell_of(src: &str, name: &str) -> Vec<Cell> {
        let iface = checked(src);
        let op = Opaques::of(&iface);
        let f = iface
            .functions
            .iter()
            .find(|f| f.name == name)
            .expect("member is present");
        cells_of(&iface, &op, f)
    }

    /// The classifier is the whole tool: the same function decides what the
    /// grid produced and what the repo instantiates, so a member and the
    /// synthesized shape that means to match it must land on one cell.
    #[test]
    fn a_member_and_its_synthesized_twin_land_on_one_cell() {
        let real = cell_of(
            "#[bridge(confined)]\npub struct Ledger { pub total: i64 }\n\
             #[bridge]\nimpl Ledger {\n  #[bridge(sync)]\n  \
             pub fn absorb(&mut self, other: &Ledger) -> i64 { 0 }\n}\n",
            "absorb",
        );
        let point = Point {
            model: ModelAxis::Confined,
            dyn_trait: false,
            exec: ExecDecl::Sync,
            recv: Recv::RefMut,
            shape: Shape::One,
            borrow: Bor::Ref,
            on_contention: Oc::None,
            native_only: false,
            no_block: false,
        };
        let synth = &spellings(&point)[0];
        let synth_cell = cell_of(synth, PROBE);
        assert_eq!(real.len(), 1, "{real:?}");
        assert_eq!(synth_cell.len(), 1, "{synth_cell:?}");
        assert_eq!(real[0].intent(), point.intent());
        assert_eq!(real[0], synth_cell[0], "\n{synth}");
    }

    /// The confined aliasing shape — one of the two defects this tool exists
    /// to have found — is a cell the checker accepts. If it ever stops being
    /// legal, the gate must stop demanding it.
    #[test]
    fn the_confined_same_type_parameter_shape_is_legal() {
        let point = Point {
            model: ModelAxis::Confined,
            dyn_trait: false,
            exec: ExecDecl::Sync,
            recv: Recv::RefMut,
            shape: Shape::One,
            borrow: Bor::Ref,
            on_contention: Oc::None,
            native_only: false,
            no_block: false,
        };
        assert!(
            spellings(&point)
                .iter()
                .any(|s| check::check(parse::parse_source(s, "crate::api").unwrap()).is_ok()),
            "no spelling of the confined aliasing shape checks"
        );
    }

    /// A member covers one cell per opaque it touches, and each cell's facts
    /// are read relative to *its* type. Two locked parameters and an unrelated
    /// scalar is one cell, not three.
    #[test]
    fn extra_value_parameters_are_projected_away() {
        let src = "#[bridge(locked)]\npub struct Vault { pub balance: i64 }\n\
                   #[bridge]\npub fn transfer(from: &mut Vault, to: &mut Vault, amount: i64) \
                   -> i64 { 0 }\n";
        let cells = cell_of(src, "transfer");
        assert_eq!(cells.len(), 1, "{cells:?}");
        assert_eq!(cells[0].params, Count::Two);
        assert_eq!(cells[0].borrow, Bor::RefMut);
        assert!(!cells[0].sibling);
        assert_eq!(cells[0].model, ModelAxis::Locked);
    }

    /// A parameter of a *different* opaque is the sibling fact, and both types
    /// get a cell.
    #[test]
    fn two_opaque_types_produce_two_cells_each_seeing_the_other_as_a_sibling() {
        let src = "#[bridge(locked)]\npub struct A { pub v: i64 }\n\
                   #[bridge(locked)]\npub struct B { pub v: i64 }\n\
                   #[bridge]\npub fn mix(a: &mut A, b: &mut B) -> i64 { 0 }\n";
        let cells = cell_of(src, "mix");
        assert_eq!(cells.len(), 2, "{cells:?}");
        assert!(cells.iter().all(|c| c.sibling && c.params == Count::One));
    }

    /// The arm is derived, never declared: the checker rewrites a confined
    /// method to sync whatever the source said, and the cell must follow.
    #[test]
    fn the_arm_follows_the_checker_not_the_declaration() {
        let src = "#[bridge(confined)]\npub struct D { pub v: i64 }\n\
                   #[bridge]\nimpl D {\n  pub fn m(&self) -> i64 { 0 }\n}\n";
        assert_eq!(cell_of(src, "m")[0].arm, Arm::Sync);
    }

    /// `self` and `self: Box<Self>` are two cells, and this is the fact that
    /// makes them two rather than an opinion about receivers: `emit_rust`
    /// writes different dispatch for them. Folding them would be declaring an
    /// axis irrelevant that the emitter reads — and irreversibly, because a
    /// single cell records a single spelling, so the group could never
    /// re-split when the emitter changed. If this ever stops being true, the
    /// two cells should become one; until then they must not.
    #[test]
    fn a_boxed_consuming_receiver_emits_different_code_from_a_bare_one() {
        let src = |recv: &str| {
            format!(
                "#[bridge(locked)]\npub struct T {{ pub v: i64 }}\n\
                 #[bridge]\nimpl T {{\n  #[bridge(sync)]\n  \
                 pub fn m({recv}) -> i64 {{ 0 }}\n}}\n"
            )
        };
        let bare = cell_of(&src("self"), "m");
        let boxed = cell_of(&src("self: Box<Self>"), "m");
        assert_eq!(bare[0].recv, Recv::Value);
        assert_eq!(boxed[0].recv, Recv::Boxed);
        assert_ne!(bare[0], boxed[0]);
        let emitted = |s: &str| {
            normalize_emission(&frustrate_codegen::emit_rust::emit(&checked(&src(s))))
        };
        assert_ne!(emitted("self"), emitted("self: Box<Self>"));
    }

    /// A consuming receiver reaches the cell it means through the grid too —
    /// the classifier is shared, so a real member and the point that means to
    /// stand for it must agree.
    #[test]
    fn a_consuming_member_and_its_synthesized_twin_land_on_one_cell() {
        let real = cell_of(
            "#[bridge(frozen)]\npub struct Tape { pub v: i64 }\n\
             #[bridge]\nimpl Tape {\n  #[bridge(sync)]\n  \
             pub fn into_count(self) -> i64 { 0 }\n}\n",
            "into_count",
        );
        let point = Point {
            model: ModelAxis::Frozen,
            dyn_trait: false,
            exec: ExecDecl::Sync,
            recv: Recv::Value,
            shape: Shape::Zero,
            borrow: Bor::Value,
            on_contention: Oc::None,
            native_only: false,
            no_block: false,
        };
        assert_eq!(real[0].intent(), point.intent());
        let synth = &spellings(&point)[0];
        assert_eq!(real[0], cell_of(synth, PROBE)[0], "\n{synth}");
    }

    /// A **data** type's by-value receiver reaches no handle: it rides the
    /// request as a value and decodes into a local, exactly as a parameter
    /// does. So it is not a `recv` value here — the axis is about handles, and
    /// a member of a data half touches no opaque at all.
    #[test]
    fn a_data_receiver_is_not_a_receiver_on_this_axis() {
        let cells = cell_of(
            "#[bridge(data)]\npub struct P { pub x: i64 }\n\
             #[bridge]\nimpl P {\n  #[bridge(sync)]\n  \
             pub fn into_x(self) -> i64 { 0 }\n}\n",
            "into_x",
        );
        assert_eq!(cells.len(), 1, "{cells:?}");
        assert_eq!(cells[0].recv, Recv::None);
        assert_eq!(cells[0].model, ModelAxis::None);
    }

    /// A `dyn` trait's cell says so, because its handle is a `Box<dyn Trait>`
    /// and a parameter of it carries an impl tag — different glue entirely.
    #[test]
    fn a_bridged_trait_is_a_different_cell_from_a_struct_of_the_same_model() {
        let as_trait = cell_of(
            "#[bridge(frozen)]\npub trait G: Send + Sync {\n  \
             #[bridge(sync)]\n  fn m(&self) -> i64;\n}\n",
            "m",
        );
        let as_struct = cell_of(
            "#[bridge(frozen)]\npub struct G { pub v: i64 }\n\
             #[bridge]\nimpl G {\n  #[bridge(sync)]\n  pub fn m(&self) -> i64 { 0 }\n}\n",
            "m",
        );
        assert!(as_trait[0].dyn_trait && !as_struct[0].dyn_trait);
        assert_ne!(as_trait[0], as_struct[0]);
    }

    /// Every grid point must be spellable in at least the sense that the
    /// synthesizer produces something; a point with no candidate source is a
    /// hole that would silently shrink the legal set.
    #[test]
    fn every_grid_point_has_at_least_one_candidate_spelling() {
        for p in grid() {
            assert!(!spellings(&p).is_empty(), "{p:?}");
        }
    }

    /// [`render`] indexes [`AXIS`] with a discriminant. A variant added to one
    /// of the axis enums without a value added to its row would either panic
    /// or print the wrong word, and printing the wrong word is the worse of
    /// the two.
    #[test]
    fn the_axis_table_has_a_word_for_every_value_of_every_axis() {
        let widest: Key = [
            Arm::Actor as u8,
            ModelAxis::Actor as u8,
            true as u8,
            Recv::Boxed as u8,
            Count::Two as u8,
            Bor::RefMut as u8,
            true as u8,
            Oc::Block as u8,
            true as u8,
            true as u8,
        ];
        for (i, (name, values)) in AXIS.iter().enumerate() {
            assert_eq!(
                values.len() as u8,
                widest[i] + 1,
                "axis `{name}` has {} words for {} values",
                values.len(),
                widest[i] + 1
            );
        }
    }


    /// The grid must be able to spell everything this repo actually writes.
    /// It is allowed not to be — the header says so — but the number is the
    /// measure of how much of the report can be trusted, so it is pinned at
    /// the only value that needs no caveat.
    #[test]
    fn every_cell_this_repo_instantiates_lies_inside_the_grid() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .unwrap()
            .to_path_buf();
        let covered = instantiated(&repo_sources(&root)).expect("the repo's own sources check");
        let survey = survey();
        let outside: Vec<&Cell> = covered.difference(&survey.legal).collect();
        assert!(
            outside.is_empty(),
            "the grid cannot spell {} cell(s) this repo instantiates:\n{}",
            outside.len(),
            outside
                .iter()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}
