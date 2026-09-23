//! Whether a changed binding contract can be delivered by a patch, and in
//! what words.
//!
//! The contract is frustrate's interface IR — the input the wire-schema hash is
//! computed over, and the thing both halves of a bridge are generated from. A
//! patch changes only Rust, so the question here is always the same one: if the
//! Dart generated beside this patch arrives (or fails to arrive), can both
//! pairings work?
//!
//! Two properties of the rest of the system decide most of it:
//!
//! * Dispatch ids are derived from each member's own wire facts
//!   (`codegen/src/hash.rs`), so a member's id does not move when its
//!   neighbours change, and an id a stale caller holds either still means the
//!   same member or means nothing at all. "Nothing at all" is the generated
//!   `unknown fn_id` panic — loud and attributed.
//! * A patch installs whole new dispatch tables (the entry points are routed
//!   through slots), so a member the launch build never had is reachable in a
//!   patched image.
//!
//! What that does *not* cover is a type whose shape moved. Every member using
//! it keeps its id, because the id names the type rather than describing it, so
//! a stale caller would encode the old shape into a decoder for the new one —
//! and for a type carrying handles that is an unsafe deref. Those restart.

use serde_json::Value;
use std::collections::BTreeMap;

/// How a contract difference must be handled.
pub struct Classification {
    /// Differences a patch can serve, as sentences, for the reply and the log.
    pub carried: Vec<String>,
    /// Differences that force a restart, as sentences naming the cause.
    pub refused: Vec<String>,
}

impl Classification {
    pub fn is_empty(&self) -> bool {
        self.carried.is_empty() && self.refused.is_empty()
    }
}

/// A section of the IR, and whether a patch can serve additions to it.
struct Section {
    key: &'static str,
    noun: &'static str,
    /// Why an addition cannot be served, where it cannot.
    addition_refused: Option<&'static str>,
}

const SECTIONS: &[Section] = &[
    Section {
        key: "functions",
        noun: "function",
        addition_refused: None,
    },
    Section {
        key: "structs",
        noun: "struct",
        addition_refused: None,
    },
    Section {
        key: "enums",
        noun: "enum",
        addition_refused: None,
    },
    Section {
        key: "opaques",
        noun: "opaque type",
        // The Dart transport binds a type's drop and finalizer by looking the
        // symbols up in the library it loaded (`runtime_native.dart`,
        // `_lookupHandleDrop`). A patch is loaded RTLD_LOCAL and adds no
        // export to that image, so the first handle minted would fail its
        // lookup — loudly, but at a point that says nothing about the edit.
        addition_refused: Some(
            "its drop and finalizer must be exported by the library Dart loaded, and a patch adds no exports to it",
        ),
    },
    Section {
        key: "externs",
        noun: "extern type",
        addition_refused: None,
    },
];

/// Classify every difference between two contracts.
pub fn classify(old: &[u8], new: &[u8]) -> Classification {
    let mut carried = Vec::new();
    let mut refused = Vec::new();
    if old == new {
        return Classification { carried, refused };
    }
    let (Ok(old), Ok(new)) = (
        serde_json::from_slice::<Value>(old),
        serde_json::from_slice::<Value>(new),
    ) else {
        refused.push(
            "the bridge's interface changed, and it could not be read to say how".into(),
        );
        return Classification { carried, refused };
    };

    // Anything outside the keyed sections — the crate name, a capability, a
    // field the IR grows later — is a difference this module has no opinion
    // about, and a patch does not get the benefit of the doubt.
    if stripped(&old) != stripped(&new) {
        refused.push(
            "the bridge's interface changed outside its declarations, so what a patch would serve cannot be established".into(),
        );
        return Classification { carried, refused };
    }

    for section in SECTIONS {
        let a = keyed(&old, section.key);
        let b = keyed(&new, section.key);
        let noun = section.noun;
        for (key, item) in &a {
            match b.get(key) {
                None => match section.key {
                    // A removed member's id is absent from the new tables, so
                    // a caller still holding it gets the unknown-id panic
                    // rather than another member.
                    "functions" => carried
                        .push(format!("{noun} `{key}` was removed from the bridge")),
                    // A removed type is safe on analysis — no surviving member
                    // can name it — and refused anyway: "a type the app has
                    // seen is fixed while it runs" is one rule instead of a
                    // case analysis per section, and removing a type is not an
                    // edit anyone makes on its own. No principle beyond that.
                    _ => refused.push(format!(
                        "{noun} `{key}` was removed, and a type the app has already seen is fixed while it runs"
                    )),
                },
                Some(other) if other != item => match section.key {
                    // A changed signature is a removal plus an addition: the
                    // member's id moved with its facts, so the stale id is
                    // absent and the new id is served by the patch.
                    "functions" => carried
                        .push(format!("{noun} `{key}` changed its bridged signature")),
                    _ => refused.push(format!(
                        "{noun} `{key}` changed shape, and every member using it keeps its dispatch id — a caller generated before the change would encode the old shape"
                    )),
                },
                _ => {}
            }
        }
        for key in b.keys() {
            if a.contains_key(key) {
                continue;
            }
            match section.addition_refused {
                None => carried.push(format!("{noun} `{key}` was added to the bridge")),
                Some(why) => refused.push(format!("{noun} `{key}` was added, and {why}")),
            }
        }
    }
    Classification { carried, refused }
}

/// The interface with its keyed sections removed, so the remainder can be
/// compared whole.
fn stripped(root: &Value) -> Value {
    let mut root = root.clone();
    if let Some(obj) = root.as_object_mut() {
        for section in SECTIONS {
            obj.remove(section.key);
        }
    }
    root
}

fn keyed(root: &Value, section: &str) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    for item in root.get(section).and_then(Value::as_array).into_iter().flatten() {
        let part = |k: &str| item.get(k).and_then(Value::as_str).map(str::to_string);
        let mut key = part("module_path").unwrap_or_default();
        if let Some(parent) = part("parent") {
            key = format!("{key}::{parent}");
        }
        key = format!("{key}::{}", part("name").unwrap_or_default());
        let key = key.trim_start_matches("crate::").to_string();
        // The id is a function of the rest of the member, so comparing it too
        // would only restate whatever else differs — and on a member that did
        // not change it is equal anyway.
        let mut item = item.clone();
        if let Some(obj) = item.as_object_mut() {
            obj.remove("fn_id");
        }
        out.insert(key, item);
    }
    out
}

/// Every member's dispatch id and name, for the id ledger
/// (`state::record_ids`).
pub fn member_ids(contract: &[u8]) -> Result<Vec<(u32, String)>, String> {
    let ir: Value = serde_json::from_slice(contract)
        .map_err(|e| format!("cannot read the binding contract: {e}"))?;
    let mut out = Vec::new();
    for (key, item) in keyed_with_ids(&ir) {
        let id = item
            .get("fn_id")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("member `{key}` has no fn_id"))?;
        out.push((id as u32, key));
    }
    Ok(out)
}

/// `keyed` without stripping the id, for the ledger.
fn keyed_with_ids(root: &Value) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    for item in root
        .get("functions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let part = |k: &str| item.get(k).and_then(Value::as_str).map(str::to_string);
        let mut key = part("module_path").unwrap_or_default();
        if let Some(parent) = part("parent") {
            key = format!("{key}::{parent}");
        }
        key = format!("{key}::{}", part("name").unwrap_or_default());
        out.insert(key.trim_start_matches("crate::").to_string(), item.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: &[u8] = br#"{"functions":[{"fn_id":10,"name":"a","module_path":"crate::api","params":[]},{"fn_id":20,"name":"b","module_path":"crate::api","params":[]}]}"#;

    #[test]
    fn an_added_removed_or_changed_member_is_carried() {
        let new = br#"{"functions":[{"fn_id":11,"name":"a","module_path":"crate::api","params":["i32"]},{"fn_id":30,"name":"c","module_path":"crate::api","params":[]}]}"#;
        let c = classify(OLD, new);
        assert_eq!(
            c.carried,
            vec![
                "function `api::a` changed its bridged signature".to_string(),
                "function `api::b` was removed from the bridge".to_string(),
                "function `api::c` was added to the bridge".to_string(),
            ]
        );
        assert!(c.refused.is_empty(), "{:?}", c.refused);
    }

    #[test]
    fn an_identical_contract_has_nothing_to_say() {
        assert!(classify(OLD, OLD).is_empty());
    }

    /// A data type's shape is the case ids cannot help with: its members keep
    /// their ids while their encoding moves.
    #[test]
    fn a_changed_data_type_is_refused() {
        let old = br#"{"structs":[{"name":"P","module_path":"crate::api","fields":["i32"]}]}"#;
        let new = br#"{"structs":[{"name":"P","module_path":"crate::api","fields":["i64"]}]}"#;
        let c = classify(old, new);
        assert!(c.carried.is_empty(), "{:?}", c.carried);
        assert_eq!(c.refused.len(), 1);
        assert!(c.refused[0].contains("keeps its dispatch id"), "{:?}", c.refused);
    }

    #[test]
    fn a_new_data_type_is_carried_but_a_new_opaque_is_not() {
        let none = br#"{}"#;
        let with_struct = br#"{"structs":[{"name":"P","module_path":"crate::api","fields":[]}]}"#;
        assert!(classify(none, with_struct).refused.is_empty());

        let with_opaque = br#"{"opaques":[{"name":"Doc","module_path":"crate::api"}]}"#;
        let c = classify(none, with_opaque);
        assert!(c.carried.is_empty(), "{:?}", c.carried);
        assert!(c.refused[0].contains("adds no exports"), "{:?}", c.refused);
    }

    /// A difference the sections do not cover must not be waved through by a
    /// classifier that only knows how to look at declarations.
    #[test]
    fn a_change_outside_the_declarations_is_refused() {
        let old = br#"{"crate_name":"a","functions":[]}"#;
        let new = br#"{"crate_name":"b","functions":[]}"#;
        let c = classify(old, new);
        assert!(c.carried.is_empty());
        assert!(c.refused[0].contains("outside its declarations"), "{:?}", c.refused);
    }

    #[test]
    fn ids_are_read_out_for_the_ledger() {
        assert_eq!(
            member_ids(OLD).unwrap(),
            vec![(10, "api::a".to_string()), (20, "api::b".to_string())]
        );
    }
}
