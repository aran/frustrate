//! Textual LLVM IR, read as a module of top-level entities.
//!
//! rustc's IR printer writes one top-level entity per line, except a function
//! definition, whose body runs from its `define … {` line to a line holding
//! only `}`. That is the whole grammar this module needs at the top level.
//! Below it, the only thing read is the token stream of an entity: global
//! names (`@name`, `@"quoted"`), metadata and attribute-group numbers, string
//! literals and comments — a lexer, not a pattern search, so an `@` inside a
//! string constant is never mistaken for a reference.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `define …`: a function with a body.
    Define,
    /// `declare …`: a function defined elsewhere.
    Declare,
    /// `@name = …`: a global variable, constant or alias.
    Global,
    /// `!…`: metadata.
    Metadata,
    /// `attributes #N = { … }`.
    Attributes,
    /// Everything else: `target triple`, `source_filename`, type definitions,
    /// comdats, `module asm`, comments, blank lines.
    Other,
}

#[derive(Debug, Clone)]
pub struct Entity<'a> {
    pub kind: Kind,
    /// The global's name as written after `@`, quotes removed but escapes not
    /// decoded; `None` for entities that name no global.
    pub name: Option<&'a str>,
    /// The entity's full text, without its trailing newline.
    pub text: &'a str,
}

pub struct Module<'a> {
    pub entities: Vec<Entity<'a>>,
    by_name: HashMap<&'a str, usize>,
    attribute_groups: HashMap<&'a str, &'a str>,
    /// Private and internal constants: see [`Module::is_content_addressed`].
    content_addressed: HashSet<&'a str>,
}

impl<'a> Module<'a> {
    pub fn parse(src: &'a str) -> Result<Module<'a>, String> {
        let mut entities = Vec::new();
        let mut by_name = HashMap::new();
        let mut attribute_groups = HashMap::new();
        let bytes = src.as_bytes();
        let mut pos = 0;
        while pos < bytes.len() {
            let end = line_end(bytes, pos);
            let line = &src[pos..end];
            let (kind, text, next) = if line.starts_with("define ") {
                // The body ends at the first line that is exactly `}`.
                let mut cursor = end + 1;
                loop {
                    if cursor >= bytes.len() {
                        return Err(format!(
                            "function body never closed: {}",
                            truncate(line, 120)
                        ));
                    }
                    let body_end = line_end(bytes, cursor);
                    if &src[cursor..body_end] == "}" {
                        break (Kind::Define, &src[pos..body_end], body_end + 1);
                    }
                    cursor = body_end + 1;
                }
            } else if line.starts_with("declare ") {
                (Kind::Declare, line, end + 1)
            } else if line.starts_with('@') {
                (Kind::Global, line, end + 1)
            } else if line.starts_with('!') {
                (Kind::Metadata, line, end + 1)
            } else if let Some(rest) = line.strip_prefix("attributes #") {
                if let Some((id, body)) = rest.split_once(" = ") {
                    attribute_groups.insert(id, body);
                }
                (Kind::Attributes, line, end + 1)
            } else {
                (Kind::Other, line, end + 1)
            };
            let name = match kind {
                Kind::Define | Kind::Declare => header_name(text),
                Kind::Global => global_name(text),
                _ => None,
            };
            if let Some(n) = name {
                by_name.insert(n, entities.len());
            }
            entities.push(Entity { kind, name, text });
            pos = next;
        }
        let content_addressed = entities
            .iter()
            .filter(|e| e.kind == Kind::Global && global_linkage(e.text).is_content_addressed())
            .filter_map(|e| e.name)
            .collect();
        Ok(Module {
            entities,
            by_name,
            attribute_groups,
            content_addressed,
        })
    }

    pub fn get(&self, name: &str) -> Option<&Entity<'a>> {
        self.by_name.get(name).map(|&i| &self.entities[i])
    }

    pub fn target_triple(&self) -> Option<&'a str> {
        self.entities.iter().find_map(|e| {
            (e.kind == Kind::Other)
                .then(|| e.text.strip_prefix("target triple = \""))
                .flatten()
                .and_then(|t| t.strip_suffix('"'))
        })
    }

    /// Whether the module carries a debug-info compile unit.
    pub fn has_debug_info(&self) -> bool {
        self.entities
            .iter()
            .any(|e| e.kind == Kind::Metadata && e.text.starts_with("!llvm.dbg.cu = "))
    }

    /// Content hashes of every function definition and global, with the
    /// incidental parts of the text removed: metadata numbers, attribute-group
    /// numbers (replaced by the group's contents) and the names of private and
    /// internal constants (replaced by their own content hash). What is left
    /// changes only when the code or data does. Keys are entity names.
    pub fn hashes(&self) -> HashMap<&'a str, u64> {
        let constants = self.constant_hashes();
        let items: Vec<&Entity<'a>> = self
            .entities
            .iter()
            .filter(|e| matches!(e.kind, Kind::Define | Kind::Global) && e.name.is_some())
            .collect();
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(16);
        let chunk = items.len().div_ceil(threads).max(1);
        let mut out = HashMap::with_capacity(items.len());
        std::thread::scope(|scope| {
            let handles: Vec<_> = items
                .chunks(chunk)
                .map(|part| {
                    let constants = &constants;
                    scope.spawn(move || {
                        part.iter()
                            .map(|e| (e.name.unwrap(), self.normalized_hash(e.text, constants)))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            for h in handles {
                out.extend(h.join().expect("hashing thread panicked"));
            }
        });
        out
    }

    /// Names of the private and internal constants, which carry no link-time
    /// identity: rustc numbers or content-names them, so a reference to one is
    /// hashed as the constant's contents.
    pub fn is_content_addressed(&self, name: &str) -> bool {
        self.content_addressed.contains(name)
    }

    fn constant_hashes(&self) -> HashMap<&'a str, u64> {
        let mut memo: HashMap<&'a str, u64> = HashMap::new();
        let names: Vec<&'a str> = self.content_addressed.iter().copied().collect();
        for name in names {
            self.constant_hash(name, &mut memo, &mut HashSet::new());
        }
        memo
    }

    fn constant_hash(
        &self,
        name: &'a str,
        memo: &mut HashMap<&'a str, u64>,
        visiting: &mut HashSet<&'a str>,
    ) -> u64 {
        if let Some(&h) = memo.get(name) {
            return h;
        }
        let text = self.get(name).expect("constant exists").text;
        // The initializer only: the name before ` = ` is incidental.
        let body = text.split_once(" = ").map_or(text, |(_, b)| b);
        visiting.insert(name);
        let mut refs = Vec::new();
        for token in Tokens::new(body) {
            if let Token::Global(n) = token {
                if self.is_content_addressed(n) && !visiting.contains(n) {
                    refs.push(n);
                }
            }
        }
        let resolved: HashMap<&str, u64> = refs
            .into_iter()
            .map(|n| (n, self.constant_hash(n, memo, visiting)))
            .collect();
        visiting.remove(name);
        let h = self.hash_tokens(body, &|n| resolved.get(n).copied());
        memo.insert(name, h);
        h
    }

    fn normalized_hash(&self, text: &str, constants: &HashMap<&'a str, u64>) -> u64 {
        // A global's own name is not part of its content; a function's name is
        // part of its header and is the key anyway, so it is harmless there.
        let text = match text.split_once(" = ") {
            Some((lhs, rhs)) if lhs.starts_with('@') => rhs,
            _ => text,
        };
        self.hash_tokens(text, &|n| constants.get(n).copied())
    }

    fn hash_tokens(&self, text: &str, constant: &dyn Fn(&str) -> Option<u64>) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for token in Tokens::new(text) {
            match token {
                Token::Text(t) => t.hash(&mut h),
                Token::Global(n) => match constant(n) {
                    Some(c) => {
                        "@const".hash(&mut h);
                        c.hash(&mut h);
                    }
                    None => {
                        "@".hash(&mut h);
                        n.hash(&mut h);
                    }
                },
                Token::MetadataRef(_) => "!".hash(&mut h),
                Token::AttributeGroup(id) => match self.attribute_groups.get(id) {
                    Some(body) => body.hash(&mut h),
                    None => id.hash(&mut h),
                },
                Token::Comment(_) => {}
            }
        }
        h.finish()
    }

    /// The global names an entity's text refers to, looking through private and
    /// internal constants to what they refer to. The entity's own name is
    /// excluded.
    pub fn references(&self, entity: &Entity<'a>) -> HashSet<&'a str> {
        let mut out = HashSet::new();
        let body = match entity.kind {
            Kind::Global => entity.text.split_once(" = ").map_or(entity.text, |(_, b)| b),
            _ => entity.text,
        };
        let mut stack: Vec<&'a str> = Tokens::new(body)
            .filter_map(|t| match t {
                Token::Global(n) => Some(n),
                _ => None,
            })
            .collect();
        let mut seen = HashSet::new();
        while let Some(n) = stack.pop() {
            if !seen.insert(n) {
                continue;
            }
            if self.is_content_addressed(n) {
                let text = self.get(n).unwrap().text;
                let init = text.split_once(" = ").map_or(text, |(_, b)| b);
                stack.extend(Tokens::new(init).filter_map(|t| match t {
                    Token::Global(g) => Some(g),
                    _ => None,
                }));
            }
            if Some(n) != entity.name {
                out.insert(n);
            }
        }
        out
    }
}

/// How a global is linked, as far as a patch needs to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlobalLinkage {
    pub local: bool,
    pub constant: bool,
    pub declaration: bool,
    pub thread_local: bool,
}

impl GlobalLinkage {
    pub fn is_content_addressed(self) -> bool {
        self.local && self.constant
    }
}

/// Read the linkage words between `=` and `global`/`constant`.
pub fn global_linkage(text: &str) -> GlobalLinkage {
    let rhs = text.split_once(" = ").map_or("", |(_, r)| r);
    let mut out = GlobalLinkage {
        local: false,
        constant: false,
        declaration: false,
        thread_local: false,
    };
    for word in rhs.split(' ') {
        match word {
            "private" | "internal" => out.local = true,
            "external" | "extern_weak" => out.declaration = true,
            w if w.starts_with("thread_local") => out.thread_local = true,
            "constant" => {
                out.constant = true;
                break;
            }
            "global" => break,
            _ => {}
        }
    }
    out
}

/// The value type of a global: the text after `global`/`constant` up to its
/// initializer. Used to tell whether a static's type changed.
pub fn global_value_type(text: &str) -> &str {
    let rhs = text.split_once(" = ").map_or("", |(_, r)| r);
    let after = rhs
        .find(" global ")
        .map(|i| &rhs[i + " global ".len()..])
        .or_else(|| rhs.find(" constant ").map(|i| &rhs[i + " constant ".len()..]))
        .or_else(|| rhs.strip_prefix("global "))
        .or_else(|| rhs.strip_prefix("constant "))
        .unwrap_or(rhs);
    leading_type(after)
}

/// The leading type of `<type> <initializer>…`: a balanced `<{…}>`, `{…}` or
/// `[…]`, or a bare word.
pub fn leading_type(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut quoted = false;
    for (i, &c) in bytes.iter().enumerate() {
        if c == b'"' {
            quoted = !quoted;
            continue;
        }
        if quoted {
            continue;
        }
        match c {
            b'<' | b'{' | b'[' | b'(' => depth += 1,
            b'>' | b'}' | b']' | b')' => {
                depth -= 1;
                if depth == 0 && (i + 1 == bytes.len() || matches!(bytes[i + 1], b' ' | b',')) {
                    return &s[..=i];
                }
            }
            b' ' | b',' if depth == 0 => return &s[..i],
            _ => {}
        }
    }
    s
}

fn line_end(bytes: &[u8], from: usize) -> usize {
    bytes[from..]
        .iter()
        .position(|&c| c == b'\n')
        .map_or(bytes.len(), |p| from + p)
}

fn truncate(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// The name in a `define`/`declare` header: the first global token.
fn header_name(text: &str) -> Option<&str> {
    let header = text.split('\n').next().unwrap_or(text);
    Tokens::new(header).find_map(|t| match t {
        Token::Global(n) => Some(n),
        _ => None,
    })
}

fn global_name(text: &str) -> Option<&str> {
    let (lhs, _) = text.split_once(" = ")?;
    match Tokens::new(lhs).next()? {
        Token::Global(n) => Some(n),
        _ => None,
    }
}

/// Decode an IR name: `\XX` hex escapes and `\\`.
pub fn unescape(name: &str) -> String {
    if !name.contains('\\') {
        return name.to_string();
    }
    let bytes = name.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'\\' {
                out.push(b'\\');
                i += 2;
                continue;
            }
            if i + 2 < bytes.len() {
                if let Ok(v) = u8::from_str_radix(&name[i + 1..i + 3], 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Spell a symbol name as an IR global name, quoting when needed.
pub fn spell(name: &str) -> String {
    let plain = !name.is_empty()
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'$' | b'.' | b'_'))
        && !name.as_bytes()[0].is_ascii_digit();
    if plain {
        format!("@{name}")
    } else {
        let mut out = String::from("@\"");
        for b in name.bytes() {
            if b == b'"' || b == b'\\' || !(0x20..0x7f).contains(&b) {
                out.push_str(&format!("\\{b:02X}"));
            } else {
                out.push(b as char);
            }
        }
        out.push('"');
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Token<'a> {
    /// Anything not otherwise classified, passed through verbatim.
    Text(&'a str),
    /// A global name, quotes removed, escapes not decoded.
    Global(&'a str),
    /// `!123`: the digits.
    MetadataRef(&'a str),
    /// `#12`: the digits.
    AttributeGroup(&'a str),
    /// `; …` to end of line.
    Comment(&'a str),
}

pub struct Tokens<'a> {
    src: &'a str,
    pos: usize,
    run: usize,
    pending: Option<Token<'a>>,
}

impl<'a> Tokens<'a> {
    pub fn new(src: &'a str) -> Self {
        Tokens {
            src,
            pos: 0,
            run: 0,
            pending: None,
        }
    }

    fn flush(&mut self, upto: usize, next: Token<'a>) -> Token<'a> {
        if upto > self.run {
            self.pending = Some(next);
            Token::Text(&self.src[self.run..upto])
        } else {
            next
        }
    }
}

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'-' | b'$' | b'.' | b'_')
}

/// Position just past a string literal whose opening quote is at `start`.
fn string_end(bytes: &[u8], start: usize) -> usize {
    let mut j = start + 1;
    while j < bytes.len() && bytes[j] != b'"' {
        j += 1;
    }
    (j + 1).min(bytes.len())
}

impl<'a> Iterator for Tokens<'a> {
    type Item = Token<'a>;

    fn next(&mut self) -> Option<Token<'a>> {
        if let Some(t) = self.pending.take() {
            return Some(t);
        }
        let bytes = self.src.as_bytes();
        while self.pos < bytes.len() {
            let i = self.pos;
            match bytes[i] {
                b'"' => self.pos = string_end(bytes, i),
                b';' => {
                    let end = line_end(bytes, i);
                    self.pos = end;
                    let tok = self.flush(i, Token::Comment(&self.src[i..end]));
                    self.run = end;
                    return Some(tok);
                }
                b'@' if i + 1 < bytes.len() && bytes[i + 1] == b'"' => {
                    let end = string_end(bytes, i + 1);
                    self.pos = end;
                    let name = &self.src[i + 2..end.saturating_sub(1).max(i + 2)];
                    let tok = self.flush(i, Token::Global(name));
                    self.run = end;
                    return Some(tok);
                }
                b'@' if i + 1 < bytes.len() && is_ident(bytes[i + 1]) => {
                    let mut j = i + 1;
                    while j < bytes.len() && is_ident(bytes[j]) {
                        j += 1;
                    }
                    self.pos = j;
                    let tok = self.flush(i, Token::Global(&self.src[i + 1..j]));
                    self.run = j;
                    return Some(tok);
                }
                c @ (b'!' | b'#') if i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() => {
                    let mut j = i + 1;
                    while j < bytes.len() && bytes[j].is_ascii_digit() {
                        j += 1;
                    }
                    self.pos = j;
                    let digits = &self.src[i + 1..j];
                    let tok = if c == b'!' {
                        Token::MetadataRef(digits)
                    } else {
                        Token::AttributeGroup(digits)
                    };
                    let tok = self.flush(i, tok);
                    self.run = j;
                    return Some(tok);
                }
                _ => self.pos += 1,
            }
        }
        if self.run < bytes.len() {
            let t = Token::Text(&self.src[self.run..]);
            self.run = bytes.len();
            return Some(t);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"source_filename = "x"
target triple = "arm64-apple-macosx11.0.0"

@alloc_1 = private unnamed_addr constant [2 x i8] c"@a", align 1
@vtable.0 = private unnamed_addr constant <{ ptr, ptr }> <{ ptr @drop, ptr @method }>, align 8
@COUNTER = internal global <{ [8 x i8] }> zeroinitializer, align 8
@"weird name" = external global i32

; Function Attrs: nounwind
define internal i64 @method(ptr %self) unnamed_addr #0 !dbg !10 {
start:
  %x = load i64, ptr @COUNTER, align 8, !dbg !12
  ret i64 %x, !dbg !13
}

define void @drop(ptr %p) #1 {
start:
  %v = getelementptr inbounds i8, ptr @vtable.0, i64 0
  ret void
}

declare void @external_fn(ptr)

attributes #0 = { nounwind "target-cpu"="apple-m1" }
attributes #1 = { "target-cpu"="apple-m1" }
!llvm.dbg.cu = !{!0}
"#;

    #[test]
    fn parses_entities_and_names() {
        let m = Module::parse(SAMPLE).unwrap();
        assert_eq!(m.target_triple(), Some("arm64-apple-macosx11.0.0"));
        assert_eq!(m.get("method").unwrap().kind, Kind::Define);
        assert!(m.get("method").unwrap().text.ends_with("\n}"));
        assert_eq!(m.get("external_fn").unwrap().kind, Kind::Declare);
        assert_eq!(m.get("weird name").unwrap().kind, Kind::Global);
        assert!(m.has_debug_info());
    }

    #[test]
    fn at_sign_in_a_string_is_not_a_reference() {
        let m = Module::parse(SAMPLE).unwrap();
        let alloc = m.get("alloc_1").unwrap();
        assert!(m.references(alloc).is_empty());
    }

    #[test]
    fn references_look_through_private_constants() {
        let m = Module::parse(SAMPLE).unwrap();
        let refs = m.references(m.get("drop").unwrap());
        assert!(refs.contains("vtable.0"));
        assert!(refs.contains("method"), "{refs:?}");
        // Its own name is excluded even when reached through the vtable.
        assert!(!refs.contains("drop"));
    }

    #[test]
    fn debug_locations_and_numbering_do_not_change_hashes() {
        let a = Module::parse(SAMPLE).unwrap();
        let shifted = SAMPLE
            .replace("!dbg !12", "!dbg !99")
            .replace("@vtable.0", "@vtable.7")
            .replace("#0", "#5");
        let b = Module::parse(&shifted).unwrap();
        let ha = a.hashes();
        let hb = b.hashes();
        assert_eq!(ha["method"], hb["method"]);
        assert_eq!(ha["drop"], hb["drop"]);
    }

    #[test]
    fn a_body_edit_changes_the_hash_and_so_does_a_constant_it_uses() {
        let a = Module::parse(SAMPLE).unwrap();
        let edited = SAMPLE.replace("load i64, ptr @COUNTER", "load i32, ptr @COUNTER");
        let b = Module::parse(&edited).unwrap();
        assert_ne!(a.hashes()["method"], b.hashes()["method"]);

        let string = SAMPLE.replace("c\"@a\"", "c\"@b\"");
        let c = Module::parse(&string).unwrap();
        assert_eq!(a.hashes()["drop"], c.hashes()["drop"]);
        assert_ne!(a.hashes()["alloc_1"], c.hashes()["alloc_1"]);
    }

    #[test]
    fn linkage_and_types() {
        let m = Module::parse(SAMPLE).unwrap();
        let counter = global_linkage(m.get("COUNTER").unwrap().text);
        assert!(counter.local && !counter.constant);
        assert!(global_linkage(m.get("vtable.0").unwrap().text).is_content_addressed());
        assert!(global_linkage(m.get("weird name").unwrap().text).declaration);
        assert_eq!(global_value_type(m.get("COUNTER").unwrap().text), "<{ [8 x i8] }>");
        assert_eq!(global_value_type(m.get("weird name").unwrap().text), "i32");
    }

    #[test]
    fn spelling_round_trips() {
        assert_eq!(spell("_RNvCs1_3foo3bar"), "@_RNvCs1_3foo3bar");
        assert_eq!(spell("a b"), "@\"a b\"");
        assert_eq!(unescape("a\\22b\\\\"), "a\"b\\");
    }
}
