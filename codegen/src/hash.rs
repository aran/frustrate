//! The wire-schema fingerprint.
//!
//! The frustrate wire protocol is positional in its shapes — enum tags by
//! position, struct fields by order — but a member's dispatch id is derived
//! from what the member *is* ([`member_ids`]), so that a caller generated from
//! a different interface asks for an id nobody defines rather than reaching a
//! member that inherited its slot. Nothing else ties a loaded native library /
//! wasm module to the Dart bindings compiled against it — so a stale generated
//! Dart file would decode wrong types straight into `unsafe` handle derefs.
//! That is undefined behaviour, not an error.
//!
//! [`schema_hash`] closes that gap: a deterministic 64-bit hash over the
//! canonical finalized IR, emitted into both surfaces and compared at init.
//! Identical IR hashes identically; any wire-affecting change (fn order/ids,
//! type shapes, enum tags, field order, models, externs, which of a type's
//! representations a member is on, and the set and order of the generic
//! instantiations `check` expands) changes it.
//!
//! What does NOT move it is everything that shapes only the Dart surface —
//! `no_eq`, `dart_interface`, `dart_identifier`, `getter`, doc comments,
//! `no_block`, a `Box` around a value type, and a generic **template**, which
//! has no wire form of its own: the declarations its uses expand into do, and
//! those are hashed.

use crate::ir::{Function, Interface};

/// A stable 64-bit fingerprint of the finalized interface.
///
/// Computed over `serde_json` of the whole [`Interface`]. The IR serializes
/// through `Vec`s and scalars only (no `HashMap`/`HashSet`), so the JSON byte
/// stream is deterministic for a given IR — same IR in, same bytes, same hash.
/// The digest is a hand-rolled FNV-1a so the fingerprint has no third-party
/// hashing dependency and is trivial to re-derive on either side.
///
/// Must be called on the finalized IR (after `check::finalize`, so `fn_id`s
/// and declaration order are fixed); both emitters do exactly that.
pub fn schema_hash(iface: &Interface) -> u64 {
    // Infallible: the IR derives Serialize with no custom impls that can fail.
    let json = serde_json::to_string(iface).expect("interface IR serializes to JSON");
    fnv1a_64(json.as_bytes())
}

/// Dispatch ids for a whole function list, in its order, derived from what
/// each member *is* rather than where it sits.
///
/// A member's id is [`fnv1a_64`] over its own serialized IR, folded to 32 bits
/// (the width of the `fn_id` parameter the entry points take). Serializing the
/// `Function` is what makes this right rather than merely convenient: every
/// judgement about what is and is not wire-relevant is already recorded as
/// `#[serde(skip)]` on its fields, so the per-call id and [`schema_hash`] are
/// derived from one list of facts instead of two that could drift. A doc
/// comment or a `dart_identifier` moves no id; a parameter, a return type, an
/// execution model or a receiver moves one.
///
/// **Why not ordinals.** Between a patch landing and the Dart reload that
/// accompanies it — a window that stays open forever when the Dart fails to
/// compile — the running library and the calling Dart were generated from
/// different interfaces. With positional ids every member declared after an
/// insertion inherits a neighbour's slot, so a stale caller reaches a
/// *different member* with a different signature and the decode runs on the
/// wrong bytes. With these ids a stale caller's id is simply absent, which the
/// entry points already answer with a loud `unknown fn_id` panic. The cost is
/// a sparse match instead of a jump table on every call; see
/// `//tests/native_bench:dispatch` for what that is worth today.
///
/// **Collisions are resolved, not reported.** Two members whose ids fold to
/// the same 32 bits are rehashed with an incrementing salt until each is free.
/// The colliding members are taken in order of their serialized bytes, not
/// their declaration order, so an id stays a function of the member plus
/// whatever it genuinely collides with: inserting an unrelated member
/// elsewhere still moves nothing. Naming a collision instead would mean asking
/// a developer to rename their own function to suit an internal hash, which is
/// not a fact they can act on.
///
/// Uniqueness within one interface is therefore total, by construction. It is
/// *not* enough on its own: an id freed by a removal can be taken by a later
/// addition, and a caller still holding the old meaning would reach the new
/// member. Nothing here can see that — it spans two builds — so the patch
/// builder keeps a ledger of every id the running process has seen and
/// refuses a patch that reuses one (`hotpatch/src/state.rs`).
pub fn member_ids(functions: &[Function]) -> Vec<u32> {
    let mut ids: Vec<u32> = functions.iter().map(member_id).collect();

    // Resolve in content order so the outcome does not depend on where the
    // colliding members happen to be declared.
    let mut order: Vec<usize> = (0..functions.len()).collect();
    order.sort_by_cached_key(|&i| serialize(&functions[i]));

    let mut taken: std::collections::HashSet<u32> = std::collections::HashSet::new();
    for &i in &order {
        if taken.insert(ids[i]) {
            continue;
        }
        // Rehash with a salt rather than probing linearly: a neighbouring id
        // may belong to a member that has not been placed yet, and stepping
        // onto it would make this member's id depend on that one's.
        let base = serialize(&functions[i]);
        let mut salt: u32 = 0;
        let id = loop {
            salt += 1;
            let candidate = fold32(fnv1a_64(&[base.as_bytes(), &salt.to_le_bytes()].concat()));
            if !taken.contains(&candidate) {
                break candidate;
            }
        };
        taken.insert(id);
        ids[i] = id;
    }
    ids
}

/// One member's id before collision resolution: the fold of its serialized IR.
fn member_id(f: &Function) -> u32 {
    fold32(fnv1a_64(serialize(f).as_bytes()))
}

/// A member's wire-relevant facts as bytes. `fn_id` is zeroed first: it is the
/// value being derived, and leaving whatever it currently holds in the input
/// would make the derivation depend on its own previous answer.
fn serialize(f: &Function) -> String {
    let mut f = f.clone();
    f.fn_id = 0;
    serde_json::to_string(&f).expect("a function serializes to JSON")
}

/// Fold 64 bits onto 32 by xor, so both halves of the digest reach the result.
/// Truncating would discard the high word, and FNV-1a's low bits are the ones
/// its multiply stirs least.
fn fold32(h: u64) -> u32 {
    ((h >> 32) ^ h) as u32
}

/// FNV-1a, 64-bit. Deterministic, dependency-free, and adequate for a
/// change-detection fingerprint (this guards against a stale build, not an
/// adversary).
fn fnv1a_64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::check;
    use crate::parse::parse_source;
    use std::fmt::Write as _;

    fn hash_src(src: &str) -> u64 {
        schema_hash(&check(parse_source(src, "crate::api").unwrap()).unwrap())
    }

    #[test]
    fn identical_ir_hashes_identically() {
        let src = "#[bridge(sync)] pub fn add(a: i32, b: i32) -> i32 { a + b }";
        assert_eq!(hash_src(src), hash_src(src));
    }

    #[test]
    fn a_reordered_surface_changes_the_hash() {
        // The fingerprint is taken over the serialized interface, whose
        // function array is ordered, so a swap moves it. Deliberately
        // conservative rather than necessary: since ids became content-derived
        // (`member_ids`) a pure reorder changes nothing a caller can observe.
        // Both halves are generated from the same IR, so this costs nothing at
        // init — it only means a reorder is not, on its own, grounds to claim
        // two builds are interchangeable.
        let a = hash_src("#[bridge(sync)] pub fn a() {} #[bridge(sync)] pub fn b() {}");
        let b = hash_src("#[bridge(sync)] pub fn b() {} #[bridge(sync)] pub fn a() {}");
        assert_ne!(a, b);
    }

    /// Ids by member name, for the tests below.
    fn ids_src(src: &str) -> std::collections::BTreeMap<String, u32> {
        let iface = check(parse_source(src, "crate::api").unwrap()).unwrap();
        iface
            .functions
            .iter()
            .map(|f| (f.name.clone(), f.fn_id))
            .collect()
    }

    /// The property the whole scheme exists for: a member's id survives its
    /// neighbours changing, so a caller generated before an insertion still
    /// reaches the member it meant.
    #[test]
    fn an_inserted_member_moves_no_other_id() {
        let before = ids_src("#[bridge(sync)] pub fn a() {} #[bridge(sync)] pub fn c() {}");
        let after = ids_src(
            "#[bridge(sync)] pub fn a() {} #[bridge(sync)] pub fn b() {} #[bridge(sync)] pub fn c() {}",
        );
        assert_eq!(before["a"], after["a"]);
        assert_eq!(before["c"], after["c"]);
        assert!(!before.contains_key("b"));
    }

    /// The property name-hashing would not have: a changed signature takes a
    /// new id, so a stale caller's request cannot be decoded by the new body.
    /// Without this, old Dart would write the old parameter list into a body
    /// reading the new one — no byte count would notice.
    #[test]
    fn a_signature_change_moves_the_id() {
        let a = ids_src("#[bridge(sync)] pub fn f(x: i32) -> i32 { x }");
        let b = ids_src("#[bridge(sync)] pub fn f(x: i64) -> i64 { x }");
        assert_ne!(a["f"], b["f"]);
    }

    /// Dart-surface-only facts are `#[serde(skip)]`, so they are not in the
    /// id's input. Editing a doc comment must not renumber the wire.
    #[test]
    fn a_dart_surface_only_change_moves_no_id() {
        let a = ids_src("#[bridge(sync)] pub fn f() {}");
        let b = ids_src("/// What it does.\n#[bridge(sync)] pub fn f() {}");
        assert_eq!(a["f"], b["f"]);
    }

    /// Collisions are resolved silently. Two members with identical wire facts
    /// hash identically by construction, so this drives the salt loop without
    /// waiting for the ~1-in-2000 event a real collision is. (`check` refuses a
    /// genuine duplicate earlier, for its own reason; this exercises what
    /// happens when two *different* members land on one id.)
    #[test]
    fn a_collision_is_resolved_rather_than_reported() {
        let iface = check(parse_source(
            "#[bridge(sync)] pub fn a(x: i32) -> i32 { x }",
            "crate::api",
        ).unwrap())
        .unwrap();
        let mut functions = iface.functions.clone();
        functions.push(functions[0].clone());
        let ids = member_ids(&functions);
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1], "a collision must be resolved, not reported");
    }

    /// Resolution does not depend on where the colliding members sit: the same
    /// pair, declared in either order, gets the same pair of ids.
    #[test]
    fn collision_resolution_is_position_independent() {
        let iface = check(parse_source(
            "#[bridge(sync)] pub fn a(x: i32) -> i32 { x } #[bridge(sync)] pub fn b() {}",
            "crate::api",
        ).unwrap())
        .unwrap();
        let (a, b) = (iface.functions[0].clone(), iface.functions[1].clone());
        let forward = member_ids(&[a.clone(), a.clone(), b.clone()]);
        let reversed = member_ids(&[b, a.clone(), a]);
        // The duplicated member's two ids are the same set either way, and the
        // unrelated member keeps its own id.
        let mut f = vec![forward[0], forward[1]];
        let mut r = vec![reversed[1], reversed[2]];
        f.sort();
        r.sort();
        assert_eq!(f, r);
        assert_eq!(forward[2], reversed[0]);
    }

    /// Uniqueness at the scale a real bridge reaches. `tests/test_api` is the
    /// largest in this repo at 1951 members, where the birthday estimate for a
    /// 32-bit space is about 0.04% — so a surface this size is where a
    /// collision would first plausibly appear, and where the resolution in
    /// `member_ids` has to hold rather than merely be unlikely to be needed.
    ///
    /// Generated rather than read from that package: a test of the derivation
    /// should not depend on another target's sources, and varying the
    /// signatures exercises more of the serialized form than repeating one
    /// shape would.
    #[test]
    fn ids_are_unique_across_a_surface_of_realistic_size() {
        let types = ["i32", "i64", "f64", "String", "bool", "u8"];
        let mut src = String::new();
        for i in 0..2000 {
            let a = types[i % types.len()];
            let b = types[(i / types.len()) % types.len()];
            let _ = write!(
                src,
                "#[bridge(sync)] pub fn m{i}(x: {a}, y: {b}) -> {a} {{ todo!() }}\n"
            );
        }
        let iface = check(parse_source(&src, "crate::api").unwrap()).unwrap();
        assert_eq!(iface.functions.len(), 2000);
        let ids: std::collections::HashSet<u32> = iface.functions.iter().map(|f| f.fn_id).collect();
        assert_eq!(ids.len(), 2000, "{} distinct ids of 2000", ids.len());
    }

    /// Whether a member **takes** its receiver is wire-relevant, and the
    /// fingerprint is what refuses the mismatch at init: a binding compiled
    /// against `&self` writes the identical request bytes for a call that no
    /// longer gives the object back, so nothing on the wire could notice.
    /// The same for a handle parameter, borrowed against by-value.
    #[test]
    fn taking_rather_than_borrowing_changes_the_hash() {
        let decl = "#[bridge(confined)] pub struct D { n: i64 } \
                    #[bridge] impl D { #[bridge(sync)] pub fn new() -> Self { todo!() } ";
        assert_ne!(
            hash_src(&format!("{decl} #[bridge(sync)] pub fn n(&self) -> i64 {{ 0 }} }}")),
            hash_src(&format!("{decl} #[bridge(sync)] pub fn n(self) -> i64 {{ 0 }} }}")),
        );
        // `self` and `self: Box<Self>` carry one handle either way, so this
        // pair moves the fingerprint further than the wire requires — see
        // `Receiver`. Pinned because it is a decision, not an accident: the
        // cost is one rebuild after an edit nobody makes twice, and the
        // alternative is splitting one syntactic fact across two IR fields.
        assert_ne!(
            hash_src(&format!("{decl} #[bridge(sync)] pub fn n(self) -> i64 {{ 0 }} }}")),
            hash_src(&format!(
                "{decl} #[bridge(sync)] pub fn n(self: Box<Self>) -> i64 {{ 0 }} }}"
            )),
        );
        let mk = |p: &str| {
            hash_src(&format!(
                "#[bridge(confined)] pub struct D {{ n: i64 }} \
                 #[bridge] impl D {{ #[bridge(sync)] pub fn new() -> Self {{ todo!() }} }} \
                 #[bridge(sync)] pub fn eat(d: {p}) {{}}"
            ))
        };
        assert_ne!(mk("&D"), mk("D"));
    }

    #[test]
    fn a_changed_type_shape_changes_the_hash() {
        let a = hash_src("#[bridge(sync)] pub fn f(x: i32) {}");
        let b = hash_src("#[bridge(sync)] pub fn f(x: i64) {}");
        assert_ne!(a, b);
    }

    #[test]
    fn a_changed_field_order_changes_the_hash() {
        let a = hash_src(
            "#[bridge(data)] pub struct P { x: i32, y: i64 } #[bridge(sync)] pub fn mk() -> P { todo!() }",
        );
        let b = hash_src(
            "#[bridge(data)] pub struct P { y: i64, x: i32 } #[bridge(sync)] pub fn mk() -> P { todo!() }",
        );
        assert_ne!(a, b);
    }

    #[test]
    fn no_eq_is_a_dart_surface_choice_and_does_not_move_the_wire() {
        // `#[bridge(no_eq)]` changes only the generated Dart class body, never
        // the wire, so it must stay out of the schema fingerprint (it is
        // `#[serde(skip)]` on the IR). Same for structs and data enums.
        let plain_s = hash_src("#[bridge(data)] pub struct S { x: i64 } #[bridge(sync)] pub fn f(s: S) -> S { s }");
        let no_eq_s = hash_src("#[bridge(data, no_eq)] pub struct S { x: i64 } #[bridge(sync)] pub fn f(s: S) -> S { s }");
        assert_eq!(plain_s, no_eq_s);
        let plain_e = hash_src("#[bridge(data)] pub enum E { A(i64) } #[bridge(sync)] pub fn f(e: E) -> E { e }");
        let no_eq_e = hash_src("#[bridge(data, no_eq)] pub enum E { A(i64) } #[bridge(sync)] pub fn f(e: E) -> E { e }");
        assert_eq!(plain_e, no_eq_e);
        // Including on a type that carries a **handle**, which gets the same
        // structural `==` every other data class gets (the handle field
        // compares by identity, through `frDeepEquals`'s `==` fallback). Which
        // members a Dart class declares has never been wire-relevant, and a
        // handle in a field does not make it so.
        let handle = "#[bridge(frozen)] pub struct Doc { pub id: i64 } ";
        let plain_h = hash_src(&format!(
            "{handle} #[bridge(data)] pub struct W {{ pub doc: Doc }} \
             #[bridge(sync)] pub fn f() -> W {{ todo!() }}"
        ));
        let no_eq_h = hash_src(&format!(
            "{handle} #[bridge(data, no_eq)] pub struct W {{ pub doc: Doc }} \
             #[bridge(sync)] pub fn f() -> W {{ todo!() }}"
        ));
        assert_eq!(plain_h, no_eq_h);
    }

    /// `inbound` is the one flag on a data declaration that DOES move the
    /// fingerprint, and it is the exception that shows the rule.
    ///
    /// Everything else on that list — `no_eq`, `dart_interface`,
    /// `dart_identifier` — shapes only the Dart class, and two peers that
    /// disagree about it still speak the same wire. `inbound` decides which way
    /// a handle behind a field travels, and therefore whether the glue **mints**
    /// a handle or **adopts** one from an id the caller sent. A peer that
    /// disagreed would read a request-side id as a response-side mint, straight
    /// into an `unsafe` deref — the failure this fingerprint exists to refuse.
    #[test]
    fn inbound_moves_the_fingerprint_because_it_says_which_way_a_handle_travels() {
        let handle = "#[bridge(confined)] pub struct Doc { pub id: i64 } ";
        let out = hash_src(&format!(
            "{handle} #[bridge(data)] pub struct W {{ pub doc: Doc }} \
             #[bridge(sync)] pub fn f() -> W {{ todo!() }}"
        ));
        let inb = hash_src(&format!(
            "{handle} #[bridge(data, inbound)] pub struct W {{ pub doc: Doc }} \
             #[bridge(sync)] pub fn f(bundle: W) {{}}"
        ));
        assert_ne!(out, inb);
        // And it is `inbound` doing it, not the signature that had to change
        // with it: the same member, with and without the keyword, is not
        // checkable both ways — so the flag is compared on the finalized IR of
        // one accepted program against itself with the flag flipped by hand.
        let mut iface = check(parse_source(
            &format!(
                "{handle} #[bridge(data, inbound)] pub struct W {{ pub doc: Doc }} \
                 #[bridge(sync)] pub fn f(bundle: W) {{}}"
            ),
            "crate::api",
        )
        .unwrap())
        .unwrap();
        let with = schema_hash(&iface);
        iface.structs.iter_mut().for_each(|s| s.inbound = false);
        assert_ne!(with, schema_hash(&iface));
    }

    /// An explicit Rust discriminant is a value the enum **carries**, not the
    /// tag it travels under: the wire is the variant's position either way, so
    /// adding, changing or dropping the numbers must not move the fingerprint.
    /// If it did, an enum could not gain a `discriminant` getter without
    /// invalidating every binding already built against it.
    #[test]
    fn an_enum_discriminant_is_carried_and_does_not_move_the_wire() {
        let plain = hash_src("#[bridge(data)] pub enum E { A, B } #[bridge(sync)] pub fn f(e: E) -> E { e }");
        let numbered = hash_src("#[bridge(data)] pub enum E { A = 100, B = 404 } #[bridge(sync)] pub fn f(e: E) -> E { e }");
        assert_eq!(plain, numbered);
        // Reordering the variants still moves it: the position IS the tag.
        let reordered = hash_src("#[bridge(data)] pub enum E { B = 404, A = 100 } #[bridge(sync)] pub fn f(e: E) -> E { e }");
        assert_ne!(numbered, reordered);
    }

    #[test]
    fn dart_identifier_renames_a_dart_name_and_does_not_move_the_wire() {
        // The wire keys on `fn_id` and field order, never on a name, so
        // renaming what an item lands under in Dart must leave two halves
        // interoperable — including one generated before the rename.
        let plain = hash_src("#[bridge(sync)] pub fn f(x: i64) -> i64 { x }");
        let named = hash_src(
            "#[bridge(sync, dart_identifier = \"g\")] pub fn f(x: i64) -> i64 { x }",
        );
        assert_eq!(plain, named);
        // Same for a field, a variant and a type declaration.
        let plain = hash_src(
            "#[bridge(data)] pub struct S { pub x: i64 } #[bridge(sync)] pub fn f(s: S) -> S { s }",
        );
        let named = hash_src(
            "#[bridge(data, dart_identifier = \"T\")] pub struct S \
             { #[bridge(dart_identifier = \"y\")] pub x: i64 } \
             #[bridge(sync)] pub fn f(s: S) -> S { s }",
        );
        assert_eq!(plain, named);
        let plain = hash_src("#[bridge(data)] pub enum E { A(i64) } #[bridge(sync)] pub fn f(e: E) -> E { e }");
        let named = hash_src(
            "#[bridge(data)] pub enum E { #[bridge(dart_identifier = \"EOnlyA\")] A(i64) } \
             #[bridge(sync)] pub fn f(e: E) -> E { e }",
        );
        assert_eq!(plain, named);
    }

    /// `#[bridge(no_block)]` is a claim to be *proven*, not a contract two
    /// halves must agree on. It changes no shipped byte — the check roots it
    /// produces are behind a cfg no production build sets — so a Rust half that
    /// claims it must still interoperate with a Dart half generated before the
    /// claim existed. Moving the fingerprint would break that for no reason.
    #[test]
    fn no_block_is_a_claim_not_a_contract_and_does_not_move_the_wire() {
        let plain = hash_src("#[bridge(sync)] pub fn f(x: i64) -> i64 { x }");
        let claimed = hash_src("#[bridge(sync, no_block)] pub fn f(x: i64) -> i64 { x }");
        assert_eq!(plain, claimed);
    }

    /// The time *peer* is wire-relevant, unlike `SeqKind`.
    ///
    /// `Vec` and `VecDeque` are `#[serde(skip)]` because they accept the
    /// identical set of values, so a binding built against one is correct
    /// against the other. Time peers are not interchangeable that way:
    /// `std::time::Duration` is unsigned and the other two are signed, so the
    /// generated Dart differs (only the unsigned peer emits the negative-span
    /// check) and the set of legal wire values differs with it. A Dart binding
    /// built when a parameter was `chrono::Duration` would otherwise pass the
    /// init check against a dylib where it is now `std::time::Duration`, and
    /// the disagreement would surface later, from whichever negative value
    /// happened to cross first. The fingerprint exists to make that
    /// deterministic and immediate.
    #[test]
    fn the_time_peer_is_wire_relevant_and_moves_the_fingerprint() {
        let std_time = hash_src(
            "#[bridge(sync)] pub fn f(d: std::time::Duration, t: std::time::SystemTime) -> \
             std::time::Duration { d }",
        );
        let chrono = hash_src(
            "#[bridge(sync)] pub fn f(d: chrono::Duration, t: chrono::DateTime<chrono::Utc>) -> \
             chrono::Duration { d }",
        );
        let time = hash_src(
            "#[bridge(sync)] pub fn f(d: time::Duration, t: time::OffsetDateTime) -> \
             time::Duration { d }",
        );
        assert_ne!(std_time, chrono);
        assert_ne!(std_time, time);
        assert_ne!(chrono, time);
    }

    /// An interface that names no time type must serialize byte-identically to
    /// what it did before `Type::Duration`/`Type::SystemTime` were
    /// parameterized — the peers are a new field on those two variants and on
    /// nothing else, so nobody else's fingerprint may move under them.
    #[test]
    fn parameterizing_the_time_types_did_not_disturb_other_interfaces() {
        let json = serde_json::to_string(
            &check(
                parse_source(
                    "#[bridge(data)] pub struct P { x: i32, y: Vec<String> } \
                     #[bridge(sync)] pub fn f(p: P, o: Option<i64>) -> P { p }",
                    "crate::api",
                )
                .unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(!json.contains("duration"), "{json}");
        assert!(!json.contains("system_time"), "{json}");
        // And the two that *are* parameterized now carry their peer, which is
        // what puts it in the hash above.
        let timed = serde_json::to_string(
            &check(
                parse_source(
                    "#[bridge(sync)] pub fn f(d: std::time::Duration) {}",
                    "crate::api",
                )
                .unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(timed.contains(r#"{"duration":"std"}"#), "{timed}");
    }

    #[test]
    fn doc_comments_are_cosmetic_and_do_not_move_the_wire() {
        // Rustdoc reaches the Dart surface but never the wire, so it is
        // `#[serde(skip)]` on the IR — writing or editing a doc comment must
        // not invalidate an otherwise-matching pair of builds. Covers the
        // declarations that carry them: functions, methods, and opaque types.
        let bare = hash_src(
            "#[bridge(locked)] pub struct C { x: i64 } \
             #[bridge] impl C { #[bridge(sync)] pub fn new() -> Self { todo!() } } \
             #[bridge(sync)] pub fn f() {}",
        );
        let documented = hash_src(
            "/// A cache.\n#[bridge(locked)] pub struct C { x: i64 } \
             #[bridge] impl C { /// Builds one.\n#[bridge(sync)] pub fn new() -> Self { todo!() } } \
             /// Free.\n#[bridge(sync)] pub fn f() {}",
        );
        assert_eq!(bare, documented);
    }

    /// A closure's *fallibility* is wire-relevant: a fallible one's reply may
    /// carry `STATUS_TYPED_ERROR`, which an infallible binding has no decoder
    /// for. frustrate mints no per-closure symbol, so the fingerprint is where
    /// the distinction has to live (the other half is the distinct generated
    /// Dart parameter type; see emit_dart's `fallible_closure_type_names_the_error`).
    #[test]
    fn a_closures_fallibility_changes_the_hash() {
        let infallible = hash_src(
            "#[bridge(data)] pub enum E { A } #[bridge(sync)] pub fn mk() -> E { todo!() } \
             #[bridge] pub async fn f(c: DartFunction<i64, i64>) {}",
        );
        let fallible = hash_src(
            "#[bridge(data)] pub enum E { A } #[bridge(sync)] pub fn mk() -> E { todo!() } \
             #[bridge] pub async fn f(c: DartFunction<i64, Result<i64, E>>) {}",
        );
        assert_ne!(infallible, fallible);
    }

    /// `Box<T>` is a Rust-side indirection: identical bytes, identical Dart,
    /// identical set of values. So it must NOT move the fingerprint — the same
    /// rule `SeqKind` is skipped under. Two halves that disagree about a `Box`
    /// interoperate, and refusing them at init would be a false alarm.
    ///
    /// The second half is what says the erasure is real rather than an accident
    /// of this particular hash: the serialized IR contains no `boxed` key at
    /// all, at any depth.
    #[test]
    fn a_box_around_a_value_type_does_not_move_the_wire() {
        for (bare, boxed) in [
            (
                "#[bridge(sync)] pub fn f(x: i64) -> i64 { x }",
                "#[bridge(sync)] pub fn f(x: Box<i64>) -> Box<i64> { x }",
            ),
            (
                "#[bridge(data)] pub struct S { pub x: Option<i64> } \
                 #[bridge(sync)] pub fn f(s: S) -> S { s }",
                "#[bridge(data)] pub struct S { pub x: Option<Box<i64>> } \
                 #[bridge(sync)] pub fn f(s: S) -> S { s }",
            ),
            (
                "#[bridge(data)] pub enum E { A(Vec<i32>) } \
                 #[bridge(sync)] pub fn f(e: E) -> E { e }",
                "#[bridge(data)] pub enum E { A(Vec<Box<i32>>) } \
                 #[bridge(sync)] pub fn f(e: E) -> E { e }",
            ),
        ] {
            assert_eq!(hash_src(bare), hash_src(boxed), "{boxed}");
        }
        let json = serde_json::to_string(
            &check(
                parse_source(
                    "#[bridge(data)] pub struct N { pub next: Option<Box<Self>>, pub v: i64 } \
                     #[bridge(sync)] pub fn f(n: N) -> N { n }",
                    "crate::api",
                )
                .unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(!json.contains("boxed"), "{json}");
        assert!(json.contains(r#"{"option":{"struct":"N"}}"#), "{json}");
    }

    /// A fixed array's **length** is wire-relevant: it is how many elements
    /// the far side reads, and the wire carries no count of its own. So `N`
    /// is serialized, unlike `SeqKind`, and two halves that disagree about it
    /// must not pass the init check.
    ///
    /// The second half is what says adding the variant disturbed nobody:
    /// `[u8; N]` still parses to `ByteArray`, so every interface that already
    /// crosses one serializes byte-identically and its fingerprint stays put.
    #[test]
    fn a_fixed_arrays_length_is_wire_relevant() {
        let four = hash_src("#[bridge(sync)] pub fn f(a: [i32; 4]) {}");
        let five = hash_src("#[bridge(sync)] pub fn f(a: [i32; 5]) {}");
        assert_ne!(four, five);
        // …and it is a different type from the `Vec` it shares a Dart mapping
        // with, because the wire differs (no length prefix).
        assert_ne!(four, hash_src("#[bridge(sync)] pub fn f(a: Vec<i32>) {}"));

        let json = serde_json::to_string(
            &check(
                parse_source("#[bridge(sync)] pub fn f(a: [u8; 32]) {}", "crate::api").unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(json.contains(r#"{"byte_array":32}"#), "{json}");
        assert!(!json.contains("array\":["), "{json}");
    }

    /// A generic data type's wire form is the set of declarations its uses
    /// expand into, so the fingerprint moves when that set does — a new
    /// instantiation is a new declaration with a new codec, and a binding
    /// compiled before it has none.
    ///
    /// The template itself is `#[serde(skip)]` and hashes nothing: it has no
    /// fields the wire can describe until its parameters are bound.
    #[test]
    fn the_set_of_generic_instantiations_is_wire_relevant() {
        let one = hash_src(
            "#[bridge(data)] pub struct P<T> { pub x: T } \
             #[bridge(sync)] pub fn a(p: P<i64>) -> i64 { 0 }",
        );
        let two = hash_src(
            "#[bridge(data)] pub struct P<T> { pub x: T } \
             #[bridge(sync)] pub fn a(p: P<i64>) -> i64 { 0 } \
             #[bridge(sync)] pub fn b(p: P<String>) -> i64 { 0 }",
        );
        assert_ne!(one, two);
        // Two different arguments are two different wire shapes, even at the
        // same arity and the same field count.
        assert_ne!(
            hash_src(
                "#[bridge(data)] pub struct P<T> { pub x: T } \
                 #[bridge(sync)] pub fn a(p: P<i64>) -> i64 { 0 }"
            ),
            hash_src(
                "#[bridge(data)] pub struct P<T> { pub x: T } \
                 #[bridge(sync)] pub fn a(p: P<i32>) -> i64 { 0 }"
            )
        );
        // A representation marker inside an argument is erased before the
        // instantiation is formed, so it moves nothing — the property a marker
        // has in every other position.
        assert_eq!(
            hash_src(
                "#[bridge(data)] pub struct I { pub v: i64 } \
                 #[bridge(data)] pub struct P<T> { pub x: T } \
                 #[bridge(sync)] pub fn a(p: P<I>) -> i64 { 0 }"
            ),
            hash_src(
                "#[bridge(data)] pub struct I { pub v: i64 } \
                 #[bridge(data)] pub struct P<T> { pub x: T } \
                 #[bridge(sync)] pub fn a(p: P<Data<I>>) -> i64 { 0 }"
            )
        );
    }

    /// Adding the generic machinery disturbed nobody: an interface that
    /// declares no generic type serializes with no `param`, no `app` and no
    /// `instance` key at any depth, so every existing fingerprint stays put.
    #[test]
    fn a_generic_free_interface_serializes_exactly_as_before() {
        let json = serde_json::to_string(
            &check(
                parse_source(
                    "#[bridge(data)] pub struct P { x: i32, y: Vec<String> } \
                     #[bridge(data)] pub enum E { A(i64), B } \
                     #[bridge(sync)] pub fn f(p: P, e: E, o: Option<i64>) -> P { p }",
                    "crate::api",
                )
                .unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        for absent in ["\"param\"", "\"app\"", "instance", "generic"] {
            assert!(!json.contains(absent), "{absent}: {json}");
        }
    }

    #[test]
    fn a_changed_enum_tag_order_changes_the_hash() {
        let a = hash_src("#[bridge(data)] pub enum E { A, B } #[bridge(sync)] pub fn mk() -> E { todo!() }");
        let b = hash_src("#[bridge(data)] pub enum E { B, A } #[bridge(sync)] pub fn mk() -> E { todo!() }");
        assert_ne!(a, b);
    }

    /// `dart_interface` changes the generated Dart *class shape* and nothing
    /// else — the wire is the same N handle ids in field order, and the
    /// generated Rust is byte-identical (emit_rust's
    /// `a_dart_interface_generates_the_same_rust_as_the_class_form`). So it is
    /// `#[serde(skip)]` like `no_eq`, and must not move the fingerprint.
    ///
    /// Field order, which IS method order and IS wire-relevant, is serialized
    /// already — the second half of this test is what says the skip did not
    /// take that with it.
    #[test]
    fn the_dart_interface_form_does_not_move_the_hash() {
        let body = " pub struct W { pub log: DartCallback<String>, \
                          pub ask: DartFunction<i64, bool> } \
             #[bridge] pub async fn f(a: W) {}";
        assert_eq!(
            hash_src(&format!("#[bridge(data, dart_interface)]{body}")),
            hash_src(&format!("#[bridge(data)]{body}"))
        );
        // But reordering the methods does move it: the handle ids ride the
        // wire in field order, so two halves that disagree must not interoperate.
        assert_ne!(
            hash_src(&format!("#[bridge(data, dart_interface)]{body}")),
            hash_src(
                "#[bridge(data, dart_interface)] pub struct W { pub ask: DartFunction<i64, bool>, \
                      pub log: DartCallback<String> } \
                 #[bridge] pub async fn f(a: W) {}"
            )
        );
    }


    /// A member's *representation* is wire-relevant: at one `fn_id` the value
    /// half decodes the receiver out of the request and the handle half reads
    /// a handle id. Two halves of a binding that disagree about which must not
    /// pass the init check, so moving a member between them moves the hash.
    #[test]
    fn moving_a_member_between_a_types_two_halves_changes_the_hash() {
        let decl = r#"#[bridge(data(dart_identifier = "DocValue"), locked)]
                      pub struct Doc { pub title: String }"#;
        let on_data = hash_src(&format!(
            "{decl} #[bridge] impl Data<Doc> {{ #[bridge(sync)] pub fn n(&self) -> i64 {{ 0 }} }}"
        ));
        let on_handle = hash_src(&format!(
            "{decl} #[bridge] impl Locked<Doc> {{
                 #[bridge(sync, on_contention = \"error\")] pub fn n(&self) -> i64 {{ 0 }} }}"
        ));
        assert_ne!(on_data, on_handle);
    }
    /// Whether a container **lends** its handles or **takes** them is
    /// wire-relevant, and this is the only place it can live: `Vec<&Doc>` and
    /// `Vec<Doc>` write byte-identical requests — a length and a handle id per
    /// element — but one leaves the objects with Dart and the other takes them.
    /// Two halves that disagree would have Dart spending tokens Rust never took
    /// (a leak) or holding handles Rust freed (a use-after-free), and nothing on
    /// the wire could notice.
    #[test]
    fn lending_rather_than_taking_a_contained_handle_changes_the_hash() {
        let decl = "#[bridge(confined)] pub struct D { n: i64 } \
                    #[bridge] impl D { #[bridge(sync)] pub fn new() -> Self { todo!() } }";
        let mk =
            |p: &str| hash_src(&format!("{decl} #[bridge(sync)] pub fn f(d: {p}) {{ let _ = d; }}"));
        assert_ne!(mk("Vec<&D>"), mk("Vec<D>"));
        assert_ne!(mk("Option<&D>"), mk("Option<D>"));
        assert_ne!(mk("(&D, i64)"), mk("(D, i64)"));
    }

    /// Adding the variant disturbed nobody: an interface that names no borrow
    /// serializes with no `ref` key at any depth, so every existing fingerprint
    /// stays where it was.
    ///
    /// The second half pins the deliberate over-strictness beside it. A borrow
    /// of a **value** (`Vec<&str>` against `Vec<String>`) is identical bytes and
    /// an identical Dart type, so by the rule `SeqKind` is skipped under it
    /// would not have to move the fingerprint — and it does, because one variant
    /// carries both readings of `&` and splitting it would mean two IR shapes
    /// for one piece of syntax that every emitter would then have to keep
    /// agreeing about. The same call `Receiver::Boxed` makes, at the same cost:
    /// one rebuild after an edit nobody makes twice.
    #[test]
    fn a_borrow_is_absent_from_an_interface_that_has_none() {
        let json = serde_json::to_string(
            &check(
                parse_source(
                    "#[bridge(data)] pub struct P { x: i32, y: Vec<String> } \
                     #[bridge(sync)] pub fn f(p: P, o: Option<Vec<u8>>, s: &str) -> P { p }",
                    "crate::api",
                )
                .unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        // The variant key, not the bare word: `Param::borrow` serializes the
        // string "ref" for an ordinary top-level `&T`, which is not this.
        assert!(!json.contains("{\"ref\":"), "{json}");
        assert_ne!(
            hash_src("#[bridge(sync)] pub fn f(xs: Vec<String>) {}"),
            hash_src("#[bridge(sync)] pub fn f(xs: Vec<&str>) {}"),
        );
    }

}
