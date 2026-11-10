//! Rust symbol names, as a reader sees them and as a path the check matches.
//!
//! `rustc-demangle` renders both manglings (legacy `_ZN…E` and v0 `_R…`) as
//! the same Rust-syntax text, `<std::fs::File as std::io::Read>::read` or
//! `core::ptr::drop_in_place::<alloc::vec::Vec<u8>>`. That text is a string,
//! not a tree, so an item's path is read off it by splitting at the `::`s
//! outside any bracket: a generic argument's own `::`s stay inside its
//! segment, which is then dropped.

use std::fmt::Write;

/// A symbol as a reader writes it, without legacy's hash. A name that is
/// neither mangling (a `#[no_mangle]` export, a C function) is returned
/// unchanged.
pub(crate) fn readable(sym: &str) -> String {
    demangled(sym).unwrap_or_else(|| sym.to_string())
}

/// The path a symbol's item is written at, segment by segment, without
/// generic arguments: `alloc::vec::Vec::push` for every instance of
/// `Vec::<T, A>::push`, in either mangling. A trait impl's method is its
/// implementing type's (`<std::fs::File as std::io::Read>::read` is
/// `std::fs::File::read`); [`trait_path`] gives the trait's. A closure keeps
/// its `{closure#0}` segment, so it is never its parent's path. A name that is
/// neither mangling is one segment.
pub(crate) fn own_path(sym: &str) -> Vec<String> {
    match demangled(sym) {
        Some(text) => item_path(&text).0,
        None => vec![sym.to_string()],
    }
}

/// A trait impl's item by the trait's path: `std::io::Read::read` for
/// `<std::fs::File as std::io::Read>::read`. `None` for anything else.
pub(crate) fn trait_path(sym: &str) -> Option<Vec<String>> {
    item_path(&demangled(sym)?).1
}

fn demangled(sym: &str) -> Option<String> {
    let d = rustc_demangle::try_demangle(sym).ok()?;
    let mut s = String::new();
    // `{:#}` leaves out legacy's hash. An error is the demangler's own size
    // limit, whose text it has already written.
    let _ = write!(s, "{d:#}");
    Some(s)
}

/// The own path and, for `<Self as Trait>::…`, the trait's path, of
/// demangled text.
fn item_path(text: &str) -> (Vec<String>, Option<Vec<String>>) {
    let mut own = Vec::new();
    let mut by_trait: Option<Vec<String>> = None;
    for (n, seg) in split_top(text, "::").into_iter().enumerate() {
        if let Some(inner) = seg.strip_prefix('<').and_then(|s| s.strip_suffix('>')) {
            // First: a qualified path, `<Self>` or `<Self as Trait>`. Later:
            // a turbofish's generic arguments, which name no item.
            if n == 0 {
                let mut parts = split_top(inner, " as ").into_iter();
                own = type_path(parts.next().unwrap_or(""));
                by_trait = parts.next().map(|t| item_path(t).0);
            }
            continue;
        }
        // Legacy writes a type's generic arguments inline: `Vec<T,A>`.
        let name = seg.split('<').next().unwrap_or("").to_string();
        own.push(name.clone());
        if let Some(t) = by_trait.as_mut() {
            t.push(name);
        }
    }
    (own, by_trait)
}

/// A type's path, behind any references, raw pointers or `dyn`. A type that
/// is not a path (a slice, a tuple, a function pointer) is one segment.
fn type_path(ty: &str) -> Vec<String> {
    let mut ty = ty.trim();
    loop {
        let next = ["&", "mut ", "*const ", "*mut ", "dyn "]
            .iter()
            .find_map(|p| ty.strip_prefix(p));
        match next {
            Some(rest) => ty = rest.trim_start(),
            None => break,
        }
    }
    // `dyn A + B`'s principal trait.
    let ty = split_top(ty, " + ").into_iter().next().unwrap_or("");
    let path_like = ty.starts_with('<')
        || (ty.starts_with(|c: char| c.is_alphanumeric() || c == '_')
            && !ty.starts_with("fn(")
            && !ty.starts_with("unsafe "));
    if path_like {
        item_path(ty).0
    } else {
        vec![ty.to_string()]
    }
}

/// `s` split at each `sep` outside brackets and literals. A `>` after `-`
/// closes nothing (`fn() -> T`); a const generic's string or char literal
/// can hold any bracket.
fn split_top<'a>(s: &'a str, sep: &str) -> Vec<&'a str> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
            }
            // A char literal; a lifetime (`'a`) has no closing quote.
            b'\'' if b.get(i + 1) == Some(&b'\\') || s[i + 1..].chars().nth(1) == Some('\'') => {
                i += 1;
                while i < b.len() && b[i] != b'\'' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
            }
            b'<' | b'(' | b'[' | b'{' => depth += 1,
            b'>' if i > 0 && b[i - 1] == b'-' => {}
            b'>' | b')' | b']' | b'}' => depth -= 1,
            _ if depth == 0 && s[i..].starts_with(sep) => {
                out.push(&s[start..i]);
                i += sep.len();
                start = i;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    out.push(&s[start.min(s.len())..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(sym: &str) -> String {
        own_path(sym).join("::")
    }

    #[test]
    fn a_generic_types_method_is_its_items_path() {
        // `<core::option::Option<&core::cell::Cell<u8>>>::zip::<&core::cell::Cell<u8>>`
        assert_eq!(
            p("_RINvMNtCscxJeVp6gxq5_4core6optionINtB3_6OptionRINtNtB5_4cell4CellhEE3zipBI_ECs3xw9iEOG5PW_11miniz_oxide"),
            "core::option::Option::zip"
        );
    }

    #[test]
    fn a_trait_impls_method_is_its_types_and_its_traits() {
        let legacy =
            "_ZN47_$LT$std..fs..File$u20$as$u20$std..io..Read$GT$4read17h0123456789abcdefE";
        assert_eq!(
            trait_path(legacy).unwrap().join("::"),
            "std::io::Read::read"
        );
        let v0 = "_RNvXs_NtCs1_7example3apiNtB4_6CircleNtB4_5Shape4area";
        assert_eq!(p(v0), "example::api::Circle::area");
        assert_eq!(
            trait_path(v0).unwrap().join("::"),
            "example::api::Shape::area"
        );
        assert_eq!(trait_path("_RNvCs1_7example6render"), None);
    }

    #[test]
    fn a_closure_and_a_primitive_impl_keep_their_segments() {
        assert_eq!(
            p("_RNCNvNtCs1_7example3api6render0B5_"),
            "example::api::render::{closure#0}"
        );
        // `<str>::trim_start_matches::<&str>`
        assert_eq!(
            p("_RINvMNtCscxJeVp6gxq5_4core3stre18trim_start_matchesReECs6JQxrVel5kt_14rustc_demangle"),
            "str::trim_start_matches"
        );
    }

    #[test]
    fn the_split_ignores_separators_inside_brackets_and_literals() {
        assert_eq!(split_top("a::<b::c>::d", "::"), vec!["a", "<b::c>", "d"]);
        assert_eq!(
            split_top("f::<fn() -> u8>::g", "::"),
            vec!["f", "<fn() -> u8>", "g"]
        );
        assert_eq!(
            split_top(r#"f::<"::>">::g"#, "::"),
            vec!["f", r#"<"::>">"#, "g"]
        );
        assert_eq!(split_top("f::<'<'>::g", "::"), vec!["f", "<'<'>", "g"]);
        assert_eq!(
            split_top("f::<&'a u8>::g", "::"),
            vec!["f", "<&'a u8>", "g"]
        );
        assert_eq!(
            item_path("<&mut [u8] as std::io::Read>::read").0,
            vec!["[u8]", "read"]
        );
        assert_eq!(
            item_path("<dyn core::any::Any + Send>::is").0,
            vec!["core", "any", "Any", "is"]
        );
    }
}
