//! Where an indirect call's target comes from: did the member produce it?
//!
//! A claimed member that calls through a function pointer or a `dyn` value
//! it made itself (an address it took, a vtable of a type it built) calls
//! code the check already follows. One that calls through a value loaded
//! from a mutable static calls whatever someone put there: a logger
//! installed with `log::set_logger`, a `Box<dyn Fn>` behind a static, a hook,
//! a table of entry points. So does one that calls through what an external
//! function returned, or wrote through a pointer it was passed (a signal's
//! previous handler, say). The check cannot say what that is, so the call is
//! "cannot verify", naming the function that makes it and where the value
//! was loaded from.
//!
//! A static that holds one of a set of the program's own functions, every
//! store into it read ([`dispatch_tables`]), is not a slot: its functions are
//! what a call through it may call, and the check follows them.
//!
//! # The model
//!
//! Each value carries the *slots* (mutable statics, external functions) it
//! came from, and those a load through it yields: loading through the
//! address of a mutable static yields that static's slot, and everything a
//! value leads to keeps its slots. A function's own stack is followed field
//! by field (by constant byte offset), so a value kept in a local or a
//! struct on the stack keeps what it is, and a sibling field does not take
//! on its neighbour's slots.
//!
//! Calls are followed with summaries in terms of the callee's parameters, as
//! access paths (a parameter, then up to [`MAX_PATH`] loads at byte
//! offsets): what it returns, what it stores through each parameter, and
//! which of the paths an indirect call, its own or a callee's, calls
//! through. A caller instantiates them with what it passes. So a callee
//! shared by many callers (`fmt::write`, say) is reported only in the
//! contexts that hand it a value from a slot, and the finding hangs off the
//! function where it enters. Which functions make the calls a path stands
//! for is traced down from there, for that finding only.
//!
//! The analysis is bounded by construction, so its memory is linear in the
//! module: a value, and a summary's called paths, name at most [`WIDTH`]
//! paths per parameter and depth; a value names at most [`WIDTH`] stack
//! places and [`WIDTH`] slots, and a frame holds at most
//! [`WIDTH`] fields per stack slot. Past that, offsets widen to "any offset",
//! places to "anywhere in the frame" and slots to "one of many slots", which
//! name more, never less, so precision is lost toward "cannot verify". A loop
//! that steps a pointer through a buffer, which would otherwise name every
//! offset, settles in a few passes.
//!
//! It is a developer's map of callbacks installed elsewhere, not a proof over
//! every pointer: a value a member reaches through a handle's object or
//! another heap object it was handed is followed only if it came from a slot.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// An indirect call whose target may have been installed elsewhere.
pub(crate) struct Site {
    /// The function in whose context the value reaches the call.
    pub(crate) at: String,
    /// The function that makes the call.
    pub(crate) func: String,
    /// Where its target may come from, worded for a message.
    pub(crate) sources: Vec<String>,
}

/// Any offset.
const ANY: i64 = i64::MIN;

/// The stack slot that stands for every slot of the function's frame: with
/// [`ANY`], a place that may be anywhere in it.
const FRAME: u32 = u32::MAX;

/// The slot that stands for more slots than a value names one by one.
const MANY: u32 = 0;

/// The largest byte offset followed as a field of its own.
const MAX_FIELD: i64 = 256;

/// The deepest chain of loads a summary names; deeper is any offset.
const MAX_PATH: usize = 3;

fn shift(base: i64, by: i64) -> i64 {
    if base == ANY || by == ANY {
        return ANY;
    }
    let at = base + by;
    if (0..=MAX_FIELD).contains(&at) {
        at
    } else {
        ANY
    }
}

/// A value named by the enclosing function's caller: parameter `param`,
/// loaded through at each offset of `loads` in turn, then offset by `delta`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Path {
    param: u32,
    loads: Vec<i64>,
    delta: i64,
}

impl Path {
    fn shifted(&self, by: i64) -> Path {
        Path {
            param: self.param,
            loads: self.loads.clone(),
            delta: shift(self.delta, by),
        }
    }

    fn loaded(&self) -> Path {
        let mut loads = self.loads.clone();
        if loads.len() < MAX_PATH {
            loads.push(self.delta);
        } else if let Some(l) = loads.last_mut() {
            *l = ANY;
        }
        Path {
            param: self.param,
            loads,
            delta: 0,
        }
    }
}

#[derive(Clone, Default)]
struct Info {
    /// Slots the value came from.
    from: BTreeSet<u32>,
    /// Slots a load through it yields.
    through: BTreeSet<u32>,
    /// Stack locations it may point to: (slot, byte offset).
    stack: BTreeSet<(u32, i64)>,
    /// What the caller passes that it may be.
    sym: BTreeSet<Path>,
    /// The function's own parameters it is derived from, as a pointer.
    params: BTreeSet<u32>,
}

fn grow<T: Ord + Clone>(a: &mut BTreeSet<T>, b: &BTreeSet<T>) -> bool {
    let n = a.len();
    a.extend(b.iter().cloned());
    a.len() != n
}

/// Add `b`'s slots to `a`, answering whether `a` now names more. Past
/// [`WIDTH`] slots, `a` is [`MANY`], which stands for each of them.
fn add_slots(a: &mut BTreeSet<u32>, b: &BTreeSet<u32>) -> bool {
    if a.contains(&MANY) {
        return false;
    }
    let n = a.len();
    a.extend(b.iter().copied());
    if a.contains(&MANY) || a.len() > WIDTH {
        *a = BTreeSet::from([MANY]);
        return true;
    }
    a.len() != n
}

/// The most paths one value names from one parameter at one depth, and the
/// most offsets it names in one stack slot, before they are widened.
const WIDTH: usize = 16;

/// Does `a` name everything `b` does: the same parameter and depth, and each
/// offset equal or any?
fn covers(a: &Path, b: &Path) -> bool {
    let at = |x: i64, y: i64| x == ANY || x == y;
    a.param == b.param
        && a.loads.len() == b.loads.len()
        && at(a.delta, b.delta)
        && a.loads.iter().zip(&b.loads).all(|(x, y)| at(*x, *y))
}

/// Add `b`'s paths to `a`, answering whether `a` now names more.
///
/// The set is kept widened: a path another covers is dropped, and where one
/// parameter's paths at one depth number more than [`WIDTH`], the position
/// whose offsets differ most becomes any offset, until they fit. Widening
/// only ever names more, the offsets are finitely many, and a set that fits
/// never names every offset of a position, so each change is growth and a
/// value grows a bounded number of times.
fn add_paths(a: &mut BTreeSet<Path>, b: &BTreeSet<Path>) -> bool {
    let fresh: Vec<&Path> = b
        .iter()
        .filter(|p| !a.range(param_range(p.param)).any(|q| covers(q, p)))
        .collect();
    if fresh.is_empty() {
        return false;
    }
    let mut groups: BTreeSet<(u32, usize)> = BTreeSet::new();
    for p in fresh {
        groups.insert((p.param, p.loads.len()));
        a.insert(p.clone());
    }
    for (param, depth) in groups {
        let mut group: Vec<Path> = a
            .range(param_range(param))
            .filter(|p| p.loads.len() == depth)
            .cloned()
            .collect();
        for p in &group {
            a.remove(p);
        }
        let at = |p: &Path, i: usize| if i == depth { p.delta } else { p.loads[i] };
        group = antichain(group);
        while group.len() > WIDTH {
            // The position with the most distinct offsets: a load's, or the
            // delta's (index `depth`).
            let widest = (0..=depth)
                .max_by_key(|i| {
                    group
                        .iter()
                        .map(|p| at(p, *i))
                        .collect::<BTreeSet<i64>>()
                        .len()
                })
                .unwrap_or(depth);
            for p in group.iter_mut() {
                if widest == depth {
                    p.delta = ANY;
                } else {
                    p.loads[widest] = ANY;
                }
            }
            group = antichain(group);
        }
        a.extend(group);
    }
    true
}

/// The paths no other one covers.
fn antichain(mut group: Vec<Path>) -> Vec<Path> {
    group.sort();
    group.dedup();
    let keep: Vec<bool> = group
        .iter()
        .enumerate()
        .map(|(i, p)| {
            !group
                .iter()
                .enumerate()
                .any(|(j, q)| j != i && covers(q, p))
        })
        .collect();
    group
        .into_iter()
        .zip(keep)
        .filter(|(_, k)| *k)
        .map(|(p, _)| p)
        .collect()
}

/// Every path from `param`.
fn param_range(param: u32) -> std::ops::RangeInclusive<Path> {
    Path {
        param,
        loads: Vec::new(),
        delta: i64::MIN,
    }..=Path {
        param,
        loads: vec![i64::MAX; MAX_PATH + 1],
        delta: i64::MAX,
    }
}

/// Add `b`'s stack places to `a`, answering whether `a` now names more. A
/// slot named at more than [`WIDTH`] offsets is named at any offset, which
/// covers each of them. Past [`WIDTH`] places in all, each slot is named at
/// any offset, and past [`WIDTH`] slots, the place is anywhere in the frame.
fn add_places(a: &mut BTreeSet<(u32, i64)>, b: &BTreeSet<(u32, i64)>) -> bool {
    if a.contains(&(FRAME, ANY)) {
        return false;
    }
    let fresh: Vec<(u32, i64)> = b
        .iter()
        .copied()
        .filter(|(s, o)| !a.contains(&(*s, ANY)) && !a.contains(&(*s, *o)))
        .collect();
    if fresh.is_empty() {
        return false;
    }
    if fresh.iter().any(|(s, _)| *s == FRAME) {
        *a = BTreeSet::from([(FRAME, ANY)]);
        return true;
    }
    let slots: BTreeSet<u32> = fresh.iter().map(|(s, _)| *s).collect();
    a.extend(fresh);
    for s in slots {
        let named: Vec<(u32, i64)> = a.range((s, i64::MIN)..=(s, i64::MAX)).copied().collect();
        if named.len() > WIDTH || named.contains(&(s, ANY)) {
            for k in named {
                a.remove(&k);
            }
            a.insert((s, ANY));
        }
    }
    if a.len() > WIDTH {
        let slots: BTreeSet<u32> = a.iter().map(|(s, _)| *s).collect();
        *a = if slots.len() > WIDTH {
            BTreeSet::from([(FRAME, ANY)])
        } else {
            slots.into_iter().map(|s| (s, ANY)).collect()
        };
    }
    true
}

impl Info {
    fn join(&mut self, o: &Info) -> bool {
        let mut c = add_slots(&mut self.from, &o.from);
        c |= add_slots(&mut self.through, &o.through);
        c |= add_places(&mut self.stack, &o.stack);
        c |= add_paths(&mut self.sym, &o.sym);
        c |= grow(&mut self.params, &o.params);
        c
    }

    /// The value as stored: without the parameters it derives from.
    fn stored(&self) -> Info {
        Info {
            params: BTreeSet::new(),
            ..self.clone()
        }
    }

    fn shifted(&self, by: i64) -> Info {
        Info {
            from: self.from.clone(),
            through: self.through.clone(),
            stack: self
                .stack
                .iter()
                .map(|(s, o)| (*s, shift(*o, by)))
                .collect(),
            sym: self.sym.iter().map(|p| p.shifted(by)).collect(),
            params: self.params.clone(),
        }
    }
}

/// One function's stack: (slot, offset) → what is stored there. A slot holds
/// at most [`WIDTH`] fields and its any-offset entry; `(FRAME, ANY)` holds
/// what was stored anywhere in the frame, which every load may see.
type Stack = BTreeMap<(u32, i64), Info>;

/// What a load through `p` yields.
fn deref(p: &Info, stack: &Stack) -> Info {
    let mut r = Info::default();
    for (s, off) in &p.stack {
        if *s == FRAME {
            for v in stack.values() {
                r.join(&v.stored());
            }
            continue;
        }
        for ((_, o), v) in stack.range((*s, i64::MIN)..=(*s, i64::MAX)) {
            if *off == ANY || *o == ANY || *o == *off {
                r.join(&v.stored());
            }
        }
    }
    if !p.stack.is_empty() {
        if let Some(v) = stack.get(&(FRAME, ANY)) {
            r.join(&v.stored());
        }
    }
    add_slots(&mut r.from, &p.through);
    add_slots(&mut r.through, &p.through);
    for path in &p.sym {
        r.sym.insert(path.loaded());
    }
    r
}

/// Store `v` at each place of `at`. A store anywhere in a slot or the frame
/// joins every entry it may overwrite; a slot that already holds its
/// [`WIDTH`] fields, or an any-offset entry, takes a new field's store in
/// that entry, which every load from the slot sees.
fn store(stack: &mut Stack, at: &BTreeSet<(u32, i64)>, v: &Info) -> bool {
    let mut changed = false;
    for (s, off) in at {
        let (s, mut off) = (*s, *off);
        if s == FRAME {
            for e in stack.values_mut() {
                changed |= e.join(v);
            }
            off = ANY;
        } else if off == ANY {
            for (_, e) in stack.range_mut((s, i64::MIN)..=(s, i64::MAX)) {
                changed |= e.join(v);
            }
        } else if !stack.contains_key(&(s, off))
            && (stack.contains_key(&(s, ANY))
                || stack.range((s, i64::MIN)..=(s, i64::MAX)).count() >= WIDTH)
        {
            off = ANY;
        }
        changed |= stack.entry((s, off)).or_default().join(v);
    }
    changed
}

/// What the callee's `paths` are, given what the caller passes.
fn instantiate(paths: &BTreeSet<Path>, args: &[Info], stack: &Stack) -> Info {
    let mut r = Info::default();
    for p in paths {
        let Some(mut v) = args.get(p.param as usize).cloned() else {
            continue;
        };
        v.params.clear();
        for off in &p.loads {
            v = deref(&v.shifted(*off), stack);
        }
        r.join(&v.shifted(p.delta));
    }
    r
}

/// A summary value, given what the caller passes.
fn instantiate_info(v: &Info, args: &[Info], stack: &Stack) -> Info {
    let mut r = instantiate(&v.sym, args, stack);
    add_slots(&mut r.from, &v.from);
    add_slots(&mut r.through, &v.through);
    r
}

#[derive(Clone, Default)]
struct Summary {
    ret: Info,
    /// What it stores through each parameter.
    writes: Vec<Info>,
    /// What its caller passes that an indirect call, its own or a callee's,
    /// may call: one entry per parameter and access path, widened as a
    /// value's paths are, so its size is bounded by the parameters, not by
    /// the calls below. Which calls an entry stands for is worked out only
    /// for an entry a caller hands a slot's value ([`Analysis::explain`]).
    pending: BTreeSet<Path>,
}

/// An operand: the locals and globals it mentions.
#[derive(Default)]
struct Op {
    locals: Vec<u32>,
    globals: Vec<u32>,
}

enum Callee {
    Direct(u32),
    Indirect(u32),
}

enum Inst {
    Load(u32, Op),
    Store(Op, Op),
    Rmw(Option<u32>, Op, Op),
    Copy(u32, Op),
    Gep(u32, Op, i64),
    Ret(Op),
    /// Destination, callee, arguments, and whether it returns a pointer.
    Call(Option<u32>, Callee, Vec<Op>, bool),
}

struct Function {
    name: String,
    params: Vec<u32>,
    locals: u32,
    body: Vec<Inst>,
    /// (local, slot) for each `alloca`.
    slots: Vec<(u32, u32)>,
}

struct Globals {
    names: Vec<String>,
    index: HashMap<String, u32>,
    mutable: Vec<bool>,
    init: Vec<Vec<u32>>,
}

impl Globals {
    fn id(&mut self, name: &str) -> u32 {
        if let Some(i) = self.index.get(name) {
            return *i;
        }
        let i = self.names.len() as u32;
        self.names.push(name.to_string());
        self.index.insert(name.to_string(), i);
        self.mutable.push(false);
        self.init.push(Vec::new());
        i
    }
}

/// The byte offset of `getelementptr inbounds i8, ptr %p, i64 <c>`, if that
/// is what the instruction is.
fn byte_offset(parts: &[&str]) -> Option<i64> {
    if parts.len() != 3 {
        return None;
    }
    let ty = parts[0]
        .trim()
        .trim_start_matches("inbounds ")
        .trim_start_matches("nuw ")
        .trim();
    if ty != "i8" {
        return None;
    }
    parts[2].trim().strip_prefix("i64 ")?.trim().parse().ok()
}

/// Answer every indirect call in `analysed` whose target may come from a
/// slot, in the context where the value reaches it.
///
/// The storage `pthread_getspecific` hands back: where std keeps a
/// thread-local on a target whose thread-locals live in pthread keys
/// (Android). A call to it is read as taking this static's address, as
/// `llvm.threadlocal.address` takes a native thread-local's, so what is
/// loaded through it is that static's slot, as a native thread-local's
/// contents are. `pthread_setspecific` keeps the pointer it is passed and
/// writes nothing through it ([`INERT`]).
const KEY_STORAGE: &str = "thread-locals kept in pthread keys";

/// `known(path)` names the statics whose contents frustrate vouches for
/// ([`super::ir::KNOWN_SLOTS`]): a load from one is not a slot.
pub(crate) fn foreign_calls(
    text: &str,
    analysed: &HashSet<String>,
    known: &dyn Fn(&str) -> bool,
) -> Calls {
    let (sites, _, tables) = analyse(text, analysed, known);
    Calls { sites, tables }
}

/// What [`foreign_calls`] answers.
pub(crate) struct Calls {
    pub(crate) sites: Vec<Site>,
    /// Each static [`dispatch_tables`] resolves, with the functions stored in
    /// it: a call through what is loaded from it calls one of them.
    pub(crate) tables: BTreeMap<String, BTreeSet<String>>,
}

/// [`foreign_calls`], with each analysed function's summary.
#[allow(clippy::type_complexity)]
fn analyse(
    text: &str,
    analysed: &HashSet<String>,
    known: &dyn Fn(&str) -> bool,
) -> (Vec<Site>, Vec<Summary>, BTreeMap<String, BTreeSet<String>>) {
    let (funcs, globals_text, declared) = split_module(text);
    let tables = dispatch_tables(&funcs, &globals_text);
    let mut g = Globals {
        names: Vec::new(),
        index: HashMap::new(),
        mutable: Vec::new(),
        init: Vec::new(),
    };
    for gl in &globals_text {
        let i = g.id(&gl.name);
        g.mutable[i as usize] = gl.mutable && !tables.contains_key(&gl.name);
        let refs: Vec<u32> = gl.init.iter().map(|(_, r)| g.id(r)).collect();
        g.init[i as usize] = refs;
    }
    let key_storage = g.id(KEY_STORAGE);
    g.mutable[key_storage as usize] = true;
    let declared: HashSet<&str> = declared.iter().map(String::as_str).collect();
    let mut parsed: Vec<Function> = Vec::new();
    let mut slot_count: u32 = 0;
    for f in funcs.iter().filter(|f| analysed.contains(&f.name)) {
        let mut locals: HashMap<String, u32> = HashMap::new();
        let local = |name: &str, locals: &mut HashMap<String, u32>| -> u32 {
            let n = locals.len() as u32;
            *locals.entry(name.to_string()).or_insert(n)
        };
        let params: Vec<u32> = f.params.iter().map(|p| local(p, &mut locals)).collect();
        let op = |s: &str, locals: &mut HashMap<String, u32>, g: &mut Globals| -> Op {
            let mut o = Op::default();
            for (global, name) in refs(s) {
                if global {
                    if !name.starts_with("llvm.") {
                        o.globals.push(g.id(&name));
                    }
                } else {
                    o.locals.push(local(&name, locals));
                }
            }
            o
        };
        let mut body = Vec::new();
        let mut slots = Vec::new();
        for line in &f.body {
            let (lhs, rhs) = match line.find(" = ") {
                Some(eq) if line.starts_with('%') => (
                    refs(&line[..eq]).into_iter().next().map(|(_, n)| n),
                    &line[eq + 3..],
                ),
                _ => (None, line.as_str()),
            };
            let dst = lhs.map(|n| local(&n, &mut locals));
            let rhs = rhs
                .trim_start_matches("tail ")
                .trim_start_matches("musttail ")
                .trim_start_matches("notail ");
            let opname = rhs.split(' ').next().unwrap_or("");
            let rest = &rhs[opname.len()..];
            match opname {
                "alloca" => {
                    if let Some(d) = dst {
                        slots.push((d, slot_count));
                        slot_count += 1;
                    }
                }
                "load" => {
                    let parts = split_top(rest);
                    let addr = parts
                        .iter()
                        .find(|p| p.trim_start().starts_with("ptr "))
                        .copied()
                        .unwrap_or("");
                    if let Some(d) = dst {
                        body.push(Inst::Load(d, op(addr, &mut locals, &mut g)));
                    }
                }
                "store" => {
                    let parts = split_top(rest);
                    if parts.len() >= 2 {
                        body.push(Inst::Store(
                            op(parts[0], &mut locals, &mut g),
                            op(parts[1], &mut locals, &mut g),
                        ));
                    }
                }
                "cmpxchg" | "atomicrmw" => {
                    let parts = split_top(rest);
                    let a = op(parts.first().copied().unwrap_or(""), &mut locals, &mut g);
                    let v = op(parts.last().copied().unwrap_or(""), &mut locals, &mut g);
                    body.push(Inst::Rmw(dst, a, v));
                }
                "getelementptr" => {
                    let parts = split_top(rest);
                    if let Some(d) = dst {
                        let off = byte_offset(&parts).unwrap_or(ANY);
                        body.push(Inst::Gep(
                            d,
                            op(parts.get(1).copied().unwrap_or(""), &mut locals, &mut g),
                            off,
                        ));
                    }
                }
                "ret" => body.push(Inst::Ret(op(rest, &mut locals, &mut g))),
                "landingpad" => {}
                "call" | "invoke" => {
                    let Some(open) = callee_open(rhs) else {
                        continue;
                    };
                    let callee = refs(&rhs[..open]).into_iter().last();
                    let returns_ptr = rhs[..open].split_whitespace().any(|w| w == "ptr");
                    let args_text = group(rhs, open);
                    let args: Vec<Op> = if args_text.trim().is_empty() {
                        Vec::new()
                    } else {
                        split_top(args_text)
                            .iter()
                            .map(|a| op(a, &mut locals, &mut g))
                            .collect()
                    };
                    match callee {
                        Some((true, name)) => {
                            if name.starts_with("llvm.memcpy")
                                || name.starts_with("llvm.memmove")
                                || name == "memcpy"
                                || name == "memmove"
                            {
                                if args.len() >= 2 {
                                    let t = local(&format!("\u{0}copy{}", body.len()), &mut locals);
                                    let mut it = args.into_iter();
                                    let d = it.next().unwrap();
                                    let s = it.next().unwrap();
                                    body.push(Inst::Load(t, s));
                                    body.push(Inst::Store(
                                        Op {
                                            locals: vec![t],
                                            globals: vec![],
                                        },
                                        d,
                                    ));
                                }
                                continue;
                            }
                            if name == "pthread_getspecific" {
                                if let Some(d) = dst {
                                    body.push(Inst::Copy(
                                        d,
                                        Op {
                                            locals: vec![],
                                            globals: vec![key_storage],
                                        },
                                    ));
                                }
                                continue;
                            }
                            if name.starts_with("llvm.") {
                                if !(name.starts_with("llvm.memset")
                                    || name.starts_with("llvm.lifetime")
                                    || name.starts_with("llvm.dbg"))
                                {
                                    if let Some(d) = dst {
                                        let mut all = Op::default();
                                        for a in args {
                                            all.locals.extend(a.locals);
                                            all.globals.extend(a.globals);
                                        }
                                        body.push(Inst::Copy(d, all));
                                    }
                                }
                                continue;
                            }
                            let gid = g.id(&name);
                            body.push(Inst::Call(dst, Callee::Direct(gid), args, returns_ptr));
                        }
                        Some((false, name)) => {
                            let c = local(&name, &mut locals);
                            body.push(Inst::Call(dst, Callee::Indirect(c), args, returns_ptr));
                        }
                        None => {}
                    }
                }
                // A comparison's result carries no pointer.
                "icmp" | "fcmp" => {}
                "select" => {
                    // The condition chooses; only the two values flow.
                    if let Some(d) = dst {
                        let parts = split_top(rest);
                        let values = parts.get(1..).map(|p| p.join(",")).unwrap_or_default();
                        body.push(Inst::Copy(d, op(&values, &mut locals, &mut g)));
                    }
                }
                _ => {
                    if let Some(d) = dst {
                        let text = rest.split(" label ").next().unwrap_or(rest);
                        body.push(Inst::Copy(d, op(text, &mut locals, &mut g)));
                    }
                }
            }
        }
        parsed.push(Function {
            name: f.name.clone(),
            params,
            locals: locals.len() as u32,
            body,
            slots,
        });
    }

    let fn_of_global: HashMap<u32, u32> = parsed
        .iter()
        .enumerate()
        .filter_map(|(i, f)| g.index.get(&f.name).map(|gi| (*gi, i as u32)))
        .collect();
    // [`MANY`] is the first.
    let mut sources: Vec<String> = vec![format!(
        "loaded from or returned by more than {WIDTH} statics and external \
         functions, which the check does not name one by one"
    )];
    let mut source_index: HashMap<String, u32> = HashMap::new();
    let mut source = |s: String| -> u32 {
        if let Some(i) = source_index.get(&s) {
            return *i;
        }
        let i = sources.len() as u32;
        sources.push(s.clone());
        source_index.insert(s, i);
        i
    };
    let mut static_slot: HashMap<u32, u32> = HashMap::new();
    for gid in 0..g.names.len() as u32 {
        let name = &g.names[gid as usize];
        if g.mutable[gid as usize]
            && !fn_of_global.contains_key(&gid)
            && !declared.contains(name.as_str())
            && !known(&super::own_path(name).join("::"))
        {
            static_slot.insert(
                gid,
                source(format!("loaded from `{}`", super::readable(name))),
            );
        }
    }
    let mut ext_slot: HashMap<u32, u32> = HashMap::new();

    let mut values: Vec<Vec<Info>> = parsed
        .iter()
        .map(|f| vec![Info::default(); f.locals as usize])
        .collect();
    let mut stacks: Vec<Stack> = vec![Stack::new(); parsed.len()];
    let mut sums: Vec<Summary> = parsed
        .iter()
        .map(|f| Summary {
            writes: vec![Info::default(); f.params.len()],
            ..Summary::default()
        })
        .collect();

    // Each function's direct callers in the set, to revisit when its summary
    // grows.
    let mut callers: Vec<Vec<usize>> = vec![Vec::new(); parsed.len()];
    for (fi, f) in parsed.iter().enumerate() {
        for inst in &f.body {
            if let Inst::Call(_, Callee::Direct(gid), _, _) = inst {
                if let Some(t) = fn_of_global.get(gid) {
                    callers[*t as usize].push(fi);
                }
            }
        }
    }
    for c in callers.iter_mut() {
        c.sort_unstable();
        c.dedup();
    }
    let mut queue: std::collections::VecDeque<usize> = (0..parsed.len()).rev().collect();
    let mut queued = vec![true; parsed.len()];
    while let Some(fi) = queue.pop_front() {
        queued[fi] = false;
        let mut summary_grew = false;
        loop {
            let mut changed = false;
            let mut sum_changed = false;
            let f = &parsed[fi];
            for (pi, p) in f.params.iter().enumerate() {
                let mut i = Info::default();
                i.sym.insert(Path {
                    param: pi as u32,
                    loads: Vec::new(),
                    delta: 0,
                });
                i.params.insert(pi as u32);
                changed |= values[fi][*p as usize].join(&i);
            }
            for (l, s) in &f.slots {
                let mut i = Info::default();
                i.stack.insert((*s, 0));
                changed |= values[fi][*l as usize].join(&i);
            }
            let read = |o: &Op, vals: &Vec<Info>| read(o, vals, &static_slot, &g);
            for inst in &f.body {
                match inst {
                    Inst::Load(d, a) => {
                        let ai = read(a, &values[fi]);
                        let r = deref(&ai, &stacks[fi]);
                        changed |= values[fi][*d as usize].join(&r);
                    }
                    Inst::Store(v, a) | Inst::Rmw(_, a, v) => {
                        let vi = read(v, &values[fi]).stored();
                        let ai = read(a, &values[fi]);
                        changed |= store(&mut stacks[fi], &ai.stack, &vi);
                        for p in &ai.params {
                            sum_changed |= sums[fi].writes[*p as usize].join(&vi);
                        }
                        if let Inst::Rmw(Some(d), _, _) = inst {
                            let r = deref(&ai, &stacks[fi]);
                            changed |= values[fi][*d as usize].join(&r);
                        }
                    }
                    Inst::Copy(d, o) => {
                        let r = read(o, &values[fi]);
                        changed |= values[fi][*d as usize].join(&r);
                    }
                    Inst::Gep(d, o, off) => {
                        let r = read(o, &values[fi]).shifted(*off);
                        changed |= values[fi][*d as usize].join(&r);
                    }
                    Inst::Ret(o) => {
                        let r = read(o, &values[fi]).stored();
                        sum_changed |= sums[fi].ret.join(&r);
                    }
                    Inst::Call(d, callee, args, returns_ptr) => {
                        let arg_infos: Vec<Info> =
                            args.iter().map(|a| read(a, &values[fi])).collect();
                        let mut result = Info::default();
                        match callee {
                            Callee::Direct(gid) => {
                                let name = &g.names[*gid as usize];
                                if let Some(t) = fn_of_global.get(gid) {
                                    let t = *t as usize;
                                    let callee_sum = sums[t].clone();
                                    result =
                                        instantiate_info(&callee_sum.ret, &arg_infos, &stacks[fi]);
                                    for (pi, w) in callee_sum.writes.iter().enumerate() {
                                        let Some(a) = arg_infos.get(pi) else { continue };
                                        let wv =
                                            instantiate_info(w, &arg_infos, &stacks[fi]).stored();
                                        let at: BTreeSet<(u32, i64)> =
                                            a.stack.iter().map(|(s, _)| (*s, ANY)).collect();
                                        changed |= store(&mut stacks[fi], &at, &wv);
                                        for p in &a.params {
                                            sum_changed |= sums[fi].writes[*p as usize].join(&wv);
                                        }
                                    }
                                    let v =
                                        instantiate(&callee_sum.pending, &arg_infos, &stacks[fi]);
                                    sum_changed |= add_paths(&mut sums[fi].pending, &v.sym);
                                } else if declared.contains(name.as_str()) {
                                    let path = super::own_path(name);
                                    if returns_its_argument(name, &path) {
                                        if let Some(a) = arg_infos.first() {
                                            result = a.clone();
                                        }
                                    } else if !is_allocator(name, &path) && !is_inert_external(name)
                                    {
                                        let s = *ext_slot.entry(*gid).or_insert_with(|| {
                                            source(format!("returned by `{name}`"))
                                        });
                                        if *returns_ptr {
                                            result.from.insert(s);
                                            result.through.insert(s);
                                        }
                                        let w = Info {
                                            from: BTreeSet::from([s]),
                                            through: BTreeSet::from([s]),
                                            ..Info::default()
                                        };
                                        for a in &arg_infos {
                                            let at: BTreeSet<(u32, i64)> =
                                                a.stack.iter().map(|(s, _)| (*s, ANY)).collect();
                                            changed |= store(&mut stacks[fi], &at, &w);
                                            for p in &a.params {
                                                sum_changed |=
                                                    sums[fi].writes[*p as usize].join(&w);
                                            }
                                        }
                                    }
                                }
                            }
                            Callee::Indirect(c) => {
                                sum_changed |=
                                    add_paths(&mut sums[fi].pending, &values[fi][*c as usize].sym);
                            }
                        }
                        if let Some(d) = d {
                            changed |= values[fi][*d as usize].join(&result);
                        }
                    }
                }
            }
            summary_grew |= sum_changed;
            if !changed && !sum_changed {
                break;
            }
        }
        if summary_grew {
            for c in &callers[fi] {
                if !queued[*c] {
                    queued[*c] = true;
                    queue.push_back(*c);
                }
            }
        }
    }

    // With the values and summaries settled, each function's calls through a
    // slot's value: its own, and those below a callee that it hands one. Only
    // these are traced down to the functions that make the calls.
    let a = Analysis {
        funcs: &parsed,
        values: &values,
        stacks: &stacks,
        sums: &sums,
        fn_of_global: &fn_of_global,
    };
    let read = |o: &Op, vals: &Vec<Info>| read(o, vals, &static_slot, &g);
    let mut explained: HashMap<(usize, Path), BTreeSet<u32>> = HashMap::new();
    let mut out = Vec::new();
    for (fi, f) in parsed.iter().enumerate() {
        let mut found: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
        for inst in &f.body {
            match inst {
                Inst::Call(_, Callee::Indirect(c), _, _) => {
                    let ci = &values[fi][*c as usize];
                    if !ci.from.is_empty() {
                        add_slots(found.entry(fi as u32).or_default(), &ci.from);
                    }
                }
                Inst::Call(_, Callee::Direct(gid), args, _) => {
                    let Some(t) = fn_of_global.get(gid) else {
                        continue;
                    };
                    let t = *t as usize;
                    let arg_infos: Vec<Info> = args.iter().map(|a| read(a, &values[fi])).collect();
                    for e in &sums[t].pending {
                        let v = instantiate(&BTreeSet::from([e.clone()]), &arg_infos, &stacks[fi]);
                        if v.from.is_empty() {
                            continue;
                        }
                        let sites = explained
                            .entry((t, e.clone()))
                            .or_insert_with(|| a.explain(t, e, &read));
                        for site in sites.iter() {
                            add_slots(found.entry(*site).or_default(), &v.from);
                        }
                    }
                }
                _ => {}
            }
        }
        for (site, labels) in found {
            out.push(Site {
                at: f.name.clone(),
                func: parsed[site as usize].name.clone(),
                sources: labels
                    .iter()
                    .map(|i| sources[*i as usize].clone())
                    .collect(),
            });
        }
    }
    (out, sums, tables)
}

/// The statics that hold one of a known set of functions: a runtime CPU
/// dispatch (memchr's), a lazily chosen implementation. A static is one when
/// only the program can name it (internal linkage, or a Rust symbol), its
/// type is `ptr`, its
/// initialiser is a function defined here, and every use of it in the module
/// is one this reads: a load from it, or a store, exchange or
/// compare-exchange into it of a value that is a function defined here,
/// directly or through a function it is passed to whose parameter is used
/// the same way. The value is followed through `phi`, `select`, a stack slot
/// of the function's own, and a parameter back to the call that passed it.
/// Any other use (its address stored, passed to an external, offset, named in
/// another initialiser), or a stored value this cannot follow to functions,
/// and the static is not one.
fn dispatch_tables(funcs: &[Func], globals: &[Global]) -> BTreeMap<String, BTreeSet<String>> {
    let func_index: HashMap<&str, usize> = funcs
        .iter()
        .enumerate()
        .map(|(i, f)| (f.name.as_str(), i))
        .collect();
    let mut candidates: HashMap<&str, BTreeSet<String>> = globals
        .iter()
        .filter(|gl| gl.internal && gl.mutable && gl.holds_pointer && gl.init.len() == 1)
        .filter(|gl| {
            gl.init
                .iter()
                .all(|(global, n)| *global && func_index.contains_key(n.as_str()))
        })
        .map(|gl| {
            (
                gl.name.as_str(),
                gl.init.iter().map(|(_, n)| n.clone()).collect(),
            )
        })
        .collect();
    if candidates.is_empty() {
        return BTreeMap::new();
    }
    // Named in another global's initialiser: its address is data.
    for gl in globals {
        for (_, n) in &gl.init {
            if n != &gl.name {
                candidates.remove(n.as_str());
            }
        }
    }
    let mut failed: HashSet<String> = HashSet::new();
    let reader = Reader {
        funcs,
        func_index: &func_index,
    };
    for (fi, f) in funcs.iter().enumerate() {
        for line in &f.body {
            if !line.contains('@') {
                continue;
            }
            let named: BTreeSet<String> = refs(line)
                .into_iter()
                .filter(|(global, n)| *global && candidates.contains_key(n.as_str()))
                .map(|(_, n)| n)
                .collect();
            for gname in named {
                if failed.contains(&gname) {
                    continue;
                }
                let mut ctx = Vec::new();
                match reader.use_of(fi, line, &Operand::Global(&gname), &mut ctx, 0) {
                    Some(stored) => candidates.get_mut(gname.as_str()).unwrap().extend(stored),
                    None => {
                        failed.insert(gname);
                    }
                }
            }
        }
    }
    candidates
        .into_iter()
        .filter(|(n, _)| !failed.contains(*n))
        .map(|(n, fs)| (n.to_string(), fs))
        .collect()
}

/// The deepest chain of calls and values [`Reader`] follows.
const MAX_FOLLOW: usize = 16;

/// What a [`Reader`] traces: a static, or a function's own local.
#[derive(Clone, Copy)]
enum Operand<'a> {
    Global(&'a str),
    Local(&'a str),
}

impl Operand<'_> {
    fn is(&self, (global, name): &(bool, String)) -> bool {
        match self {
            Operand::Global(n) => *global && name == n,
            Operand::Local(n) => !*global && name == n,
        }
    }
}

/// The calls a parameter was passed down through, innermost last: each the
/// calling function and its argument texts.
type Passed = Vec<(usize, Vec<String>)>;

/// Reads the uses of one pointer and the values stored through it.
struct Reader<'a> {
    funcs: &'a [Func],
    func_index: &'a HashMap<&'a str, usize>,
}

impl Reader<'_> {
    /// The functions `line` of function `fi` stores through `p`, if every way
    /// it uses `p` is one [`dispatch_tables`] reads: none for a load.
    fn use_of(
        &self,
        fi: usize,
        line: &str,
        p: &Operand,
        ctx: &mut Passed,
        depth: usize,
    ) -> Option<BTreeSet<String>> {
        if depth > MAX_FOLLOW {
            return None;
        }
        let rhs = match line.find(" = ") {
            Some(eq) if line.starts_with('%') => &line[eq + 3..],
            _ => line,
        };
        let rhs = rhs
            .trim_start_matches("tail ")
            .trim_start_matches("musttail ")
            .trim_start_matches("notail ");
        let opname = rhs.split(' ').next().unwrap_or("");
        let rest = &rhs[opname.len()..];
        let parts = split_top(rest);
        let only = |part: &str| is_pointer_operand(part, p);
        let mentions = |part: &str| refs(part).iter().any(|r| p.is(r));
        match opname {
            "load" => {
                let addr = parts
                    .iter()
                    .position(|x| x.trim_start().starts_with("ptr "))?;
                (only(parts[addr])
                    && !parts
                        .iter()
                        .enumerate()
                        .any(|(i, x)| i != addr && mentions(x)))
                .then(BTreeSet::new)
            }
            "store" => {
                if parts.len() < 2
                    || !only(parts[1])
                    || mentions(parts[0])
                    || parts[2..].iter().any(|x| mentions(x))
                {
                    return None;
                }
                self.functions(fi, parts[0], ctx, depth + 1)
            }
            "atomicrmw" => {
                let op = parts
                    .first()?
                    .split_whitespace()
                    .find(|w| *w != "volatile")?;
                if op != "xchg" || parts.len() < 2 || mentions(parts[1]) {
                    return None;
                }
                let addr = parts[0]
                    .trim_start()
                    .trim_start_matches("volatile ")
                    .trim_start()
                    .strip_prefix("xchg ")?;
                only(addr).then_some(())?;
                self.functions(fi, parts[1], ctx, depth + 1)
            }
            "cmpxchg" => {
                if parts.len() < 3 || mentions(parts[1]) || mentions(parts[2]) {
                    return None;
                }
                let addr = parts[0]
                    .trim_start()
                    .trim_start_matches("weak ")
                    .trim_start_matches("volatile ");
                only(addr).then_some(())?;
                self.functions(fi, parts[2], ctx, depth + 1)
            }
            "call" | "invoke" => {
                let open = callee_open(rhs)?;
                let (global, callee) = refs(&rhs[..open]).into_iter().last()?;
                if !global || refs(&rhs[..open]).iter().any(|r| p.is(r)) {
                    return None;
                }
                let &ci = self.func_index.get(callee.as_str())?;
                let args: Vec<String> = split_top(group(rhs, open))
                    .iter()
                    .map(|a| a.trim().to_string())
                    .collect();
                if rhs[open..].len() > group(rhs, open).len() + 2
                    && mentions(&rhs[open + group(rhs, open).len() + 2..])
                {
                    return None;
                }
                let mut out = BTreeSet::new();
                for (k, a) in args.iter().enumerate() {
                    if !mentions(a) {
                        continue;
                    }
                    if !only(a) {
                        return None;
                    }
                    let param = self.funcs[ci].params.get(k)?.clone();
                    ctx.push((fi, args.clone()));
                    let r = self.uses_of_param(ci, &param, ctx, depth + 1);
                    ctx.pop();
                    out.extend(r?);
                }
                Some(out)
            }
            _ => None,
        }
    }

    /// The functions stored through parameter `param` of function `fi`, if
    /// every use of it there is one [`dispatch_tables`] reads.
    fn uses_of_param(
        &self,
        fi: usize,
        param: &str,
        ctx: &mut Passed,
        depth: usize,
    ) -> Option<BTreeSet<String>> {
        let p = Operand::Local(param);
        let mut out = BTreeSet::new();
        for line in &self.funcs[fi].body {
            if refs(line).iter().any(|r| p.is(r)) {
                out.extend(self.use_of(fi, line, &p, ctx, depth)?);
            }
        }
        Some(out)
    }

    /// The functions defined here that the value of `operand` (a typed
    /// operand, `ptr %x`) in function `fi` may be, if this can follow it to
    /// nothing else.
    fn functions(
        &self,
        fi: usize,
        operand: &str,
        ctx: &mut Passed,
        depth: usize,
    ) -> Option<BTreeSet<String>> {
        if depth > MAX_FOLLOW {
            return None;
        }
        let operand = operand.trim();
        let words: Vec<&str> = operand.split_whitespace().collect();
        // A typed value: `ptr @f`, `ptr %x`, with an optional `atomic` or
        // `volatile` before it.
        let value = match words.as_slice() {
            [.., "ptr", v] => *v,
            _ => return None,
        };
        let r = refs(value);
        let [(global, name)] = r.as_slice() else {
            return None;
        };
        if *global {
            return self
                .func_index
                .contains_key(name.as_str())
                .then(|| BTreeSet::from([name.clone()]));
        }
        let f = &self.funcs[fi];
        if let Some(j) = f.params.iter().position(|q| q == name) {
            let (caller, args) = ctx.pop()?;
            let r = args
                .get(j)
                .cloned()
                .and_then(|a| self.functions(caller, &a, ctx, depth + 1));
            ctx.push((caller, args));
            return r;
        }
        let def = f.body.iter().find(|l| {
            l.starts_with('%')
                && refs(&l[..l.find(" = ").unwrap_or(0)])
                    .first()
                    .is_some_and(|(g, n)| !g && n == name)
        })?;
        let rhs = &def[def.find(" = ")? + 3..];
        let opname = rhs.split(' ').next().unwrap_or("");
        let rest = &rhs[opname.len()..];
        match opname {
            "phi" => {
                let rest = rest.trim_start().strip_prefix("ptr ")?;
                let mut out = BTreeSet::new();
                for incoming in split_top(rest) {
                    let inner = incoming.trim().strip_prefix('[')?.strip_suffix(']')?;
                    let v = split_top(inner).first()?.trim().to_string();
                    out.extend(self.functions(fi, &format!("ptr {v}"), ctx, depth + 1)?);
                }
                Some(out)
            }
            "select" => {
                let parts = split_top(rest);
                if parts.len() != 3 {
                    return None;
                }
                let mut out = self.functions(fi, parts[1], ctx, depth + 1)?;
                out.extend(self.functions(fi, parts[2], ctx, depth + 1)?);
                Some(out)
            }
            "load" => {
                let parts = split_top(rest);
                let addr = parts.iter().find(|x| x.trim_start().starts_with("ptr "))?;
                let named = refs(addr);
                let [(false, slot)] = named.as_slice() else {
                    return None;
                };
                let is_alloca = f.body.iter().any(|l| {
                    l.starts_with('%')
                        && l[l.find(" = ").map_or(l.len(), |e| e + 3)..].starts_with("alloca ")
                        && refs(&l[..l.find(" = ").unwrap_or(0)])
                            .first()
                            .is_some_and(|(g, n)| !g && n == slot)
                });
                if !is_alloca {
                    return None;
                }
                self.stack_slot(fi, slot, ctx, depth + 1)
            }
            _ => None,
        }
    }

    /// The functions stored into stack slot `slot` (an `alloca` of function
    /// `fi`), if the function only loads from it, stores into it, and marks
    /// its lifetime.
    fn stack_slot(
        &self,
        fi: usize,
        slot: &str,
        ctx: &mut Passed,
        depth: usize,
    ) -> Option<BTreeSet<String>> {
        let p = Operand::Local(slot);
        let mut out = BTreeSet::new();
        for line in &self.funcs[fi].body {
            if !refs(line).iter().any(|r| p.is(r)) {
                continue;
            }
            let rhs = match line.find(" = ") {
                Some(eq) if line.starts_with('%') => &line[eq + 3..],
                _ => line.as_str(),
            };
            if rhs.starts_with("alloca ") || rhs.contains("@llvm.lifetime.") {
                continue;
            }
            let opname = rhs.split(' ').next().unwrap_or("");
            let parts = split_top(&rhs[opname.len()..]);
            let only = |part: &str| is_pointer_operand(part, &p);
            let mentions = |part: &str| refs(part).iter().any(|r| p.is(r));
            match opname {
                "load" => {
                    let addr = parts
                        .iter()
                        .position(|x| x.trim_start().starts_with("ptr "))?;
                    if !only(parts[addr]) {
                        return None;
                    }
                }
                "store" => {
                    if parts.len() < 2 || !only(parts[1]) || mentions(parts[0]) {
                        return None;
                    }
                    out.extend(self.functions(fi, parts[0], ctx, depth + 1)?);
                }
                _ => return None,
            }
        }
        Some(out)
    }
}

/// Whether a typed operand is exactly the pointer `p`: `ptr @G`, `ptr %p`,
/// with parameter attributes or an atomic ordering around it, and nothing
/// else naming a value.
fn is_pointer_operand(part: &str, p: &Operand) -> bool {
    let r = refs(part);
    let [one] = r.as_slice() else { return false };
    p.is(one) && part.trim_start().starts_with("ptr ") && !part.contains('(')
}

/// What an operand may be: its locals joined, and for each global, the
/// slots a load through it yields.
fn read(o: &Op, vals: &[Info], static_slot: &HashMap<u32, u32>, g: &Globals) -> Info {
    let mut i = Info::default();
    for l in &o.locals {
        i.join(&vals[*l as usize]);
    }
    for gid in &o.globals {
        if let Some(s) = static_slot.get(gid) {
            i.through.insert(*s);
        } else {
            for t in &g.init[*gid as usize] {
                if let Some(s) = static_slot.get(t) {
                    i.through.insert(*s);
                }
            }
        }
    }
    i
}

/// The settled analysis, for tracing a summary entry to its calls.
struct Analysis<'a> {
    funcs: &'a [Function],
    values: &'a [Vec<Info>],
    stacks: &'a [Stack],
    sums: &'a [Summary],
    fn_of_global: &'a HashMap<u32, u32>,
}

impl Analysis<'_> {
    /// The functions whose indirect calls `t`'s pending entry `e` stands
    /// for: those in `t` through a path `e` covers, and those a callee's
    /// entry stands for where `t` passes that callee something `e` covers.
    /// Each (function, entry) is visited once, so the walk is bounded by the
    /// summaries' size.
    fn explain(&self, t: usize, e: &Path, read: &dyn Fn(&Op, &Vec<Info>) -> Info) -> BTreeSet<u32> {
        let mut sites = BTreeSet::new();
        let mut seen: HashSet<(usize, Path)> = HashSet::new();
        let mut work = vec![(t, e.clone())];
        while let Some((u, q)) = work.pop() {
            if !seen.insert((u, q.clone())) {
                continue;
            }
            let named = |v: &Info| v.sym.iter().any(|p| covers(&q, p));
            for inst in &self.funcs[u].body {
                match inst {
                    Inst::Call(_, Callee::Indirect(c), _, _) => {
                        if named(&self.values[u][*c as usize]) {
                            sites.insert(u as u32);
                        }
                    }
                    Inst::Call(_, Callee::Direct(gid), args, _) => {
                        let Some(w) = self.fn_of_global.get(gid) else {
                            continue;
                        };
                        let w = *w as usize;
                        let arg_infos: Vec<Info> =
                            args.iter().map(|a| read(a, &self.values[u])).collect();
                        for e2 in &self.sums[w].pending {
                            let v = instantiate(
                                &BTreeSet::from([e2.clone()]),
                                &arg_infos,
                                &self.stacks[u],
                            );
                            if named(&v) {
                                work.push((w, e2.clone()));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        sites
    }
}

/// Functions known to allocate fresh memory, by the demangled path's
/// leading segments, and the C allocator.
fn is_allocator(name: &str, path: &[String]) -> bool {
    let rust =
        |f: &str| path.len() >= 3 && path[0] == "alloc" && path[1] == "alloc" && path[2] == f;
    matches!(name, "malloc" | "calloc" | "aligned_alloc" | "valloc")
        || rust("alloc")
        || rust("alloc_zeroed")
        || rust("exchange_malloc")
}

/// Externals whose result points into their first argument, or at its
/// contents moved: what a load through it yields is what the argument held.
fn returns_its_argument(name: &str, path: &[String]) -> bool {
    matches!(name, "memchr" | "strchr" | "strrchr" | "realloc")
        || (path.len() >= 3 && path[0] == "alloc" && path[1] == "alloc" && path[2] == "realloc")
}

/// The externals a call to which hands the program nothing it could call:
/// what each returns, and what it writes through its arguments, is plain
/// data, an integer, bytes, or a pointer to memory that holds no function
/// pointer and no pointer that leads to one (errno's slot, a directory
/// stream, a pointer back into a buffer the caller passed, filled with such
/// data). A load through such a pointer yields nothing callable, so the call
/// moves nothing. Several keep a function pointer (a thread-local destructor,
/// a signal handler); that is not a concern here, as the OS calls what they
/// keep outside any call of the program's, and any external that hands a
/// kept pointer back is not on this list. Nor is one that hands back memory
/// holding code pointers, the library's or the program's: a thread's control
/// block or stack, a dispatch object. A trailing `*` matches a prefix.
///
/// The names are every C library's the native check knows
/// ([`super::libc`]): each does what its group says in each library that has
/// it.
///
/// Any other external is a slot: what it returns, and what it may write
/// through each pointer it is passed, may be anything.
const INERT: &[(&[&str], &str)] = &[
    (
        &[
            "memcmp",
            "bcmp",
            "memset",
            "bzero",
            "strlen",
            "strnlen",
            "strcmp",
            "strncmp",
            "strerror_r",
            "__xpg_strerror_r",
            "gai_strerror",
        ],
        "compare, measure or fill memory the caller passes, write a message's bytes \
         into it, or point at a fixed message",
    ),
    (
        &[
            "free",
            "malloc_size",
            "malloc_usable_size",
            "posix_memalign",
            "mmap",
            "mmap64",
            "munmap",
            "mprotect",
            "madvise",
        ],
        "free memory, size it, or hand back fresh memory, which holds nothing of the program's",
    ),
    (
        &[
            "pthread_mutex_unlock",
            "pthread_mutex_trylock",
            "pthread_mutex_init",
            "pthread_mutex_destroy",
            "pthread_mutexattr_*",
            "pthread_cond_signal",
            "pthread_cond_broadcast",
            "pthread_cond_init",
            "pthread_cond_destroy",
            "pthread_condattr_*",
            "pthread_rwlock_unlock",
            "pthread_rwlock_tryrdlock",
            "pthread_rwlock_trywrlock",
            "pthread_rwlock_init",
            "pthread_rwlock_destroy",
            "os_unfair_lock_unlock",
            "os_unfair_lock_trylock",
            "dispatch_semaphore_signal",
            "dispatch_release",
            "dispatch_retain",
            "dispatch_time",
            "__ulock_wake",
            "os_sync_wake_by_address_*",
        ],
        "change or read a lock's own state, or an integer setting",
    ),
    (
        &[
            "pthread_threadid_np",
            "pthread_get_stacksize_np",
            "pthread_attr_init",
            "pthread_attr_destroy",
            "pthread_attr_set*",
            "pthread_attr_getstacksize",
            "pthread_attr_getguardsize",
            "pthread_attr_getstack",
            "pthread_detach",
            "pthread_setname_np",
            "pthread_key_create",
            "pthread_key_delete",
            "pthread_setspecific",
            "_tlv_atexit",
            "gettid",
            "sched_getaffinity",
        ],
        "read a thread's integer state, set its attributes, keep a value for a \
         thread-local key, or register a destructor the OS runs at thread exit",
    ),
    (
        &[
            "clock_gettime",
            "clock_gettime_nsec_np",
            "mach_absolute_time",
            "mach_continuous_time",
            "mach_timebase_info",
            "gettimeofday",
            "time",
            "getpid",
            "getppid",
            "getuid",
            "geteuid",
            "getgid",
            "getegid",
            "sysconf",
            "_NSGetExecutablePath",
            "getauxval",
        ],
        "read a clock, an id or a process constant as integers or bytes",
    ),
    (
        &[
            "CCRandomGenerateBytes",
            "arc4random",
            "arc4random_buf",
            "arc4random_uniform",
            "getentropy",
            "getrandom",
        ],
        "write random bytes",
    ),
    (
        &[
            "_Unwind_GetIP",
            "_Unwind_GetIPInfo",
            "_Unwind_GetCFA",
            "_Unwind_GetRegionStart",
            "_Unwind_GetDataRelBase",
            "_Unwind_GetTextRelBase",
            "_Unwind_SetGR",
            "_Unwind_SetIP",
            "_Unwind_DeleteException",
            "_Unwind_RaiseException",
            "_Unwind_Resume",
            "_Unwind_GetLanguageSpecificData",
        ],
        "read addresses as integers from the frame being unwound, or its exception \
         table, whose entries are offsets and type descriptors; write to the frame; \
         or unwind",
    ),
    (
        &[
            "__error",
            "__errno",
            "__errno_location",
            "_NSGetArgc",
            "_NSGetArgv",
            "_NSGetEnviron",
        ],
        "point at the thread's errno, or at the argument count and the argument \
         and environment strings",
    ),
    (
        &[
            "opendir",
            "fdopendir",
            "readdir",
            "readdir_r",
            "readdir64",
            "readdir64_r",
            "closedir",
        ],
        "open, read or close a directory stream: the stream holds a descriptor, \
         a buffer and positions, and an entry holds a name and integers",
    ),
    (
        &[
            "stat",
            "fstat",
            "lstat",
            "fstatat",
            "stat64",
            "fstat64",
            "lstat64",
            "fstatat64",
            "statx",
        ],
        "fill the buffer the caller passes with a file's metadata, all integers",
    ),
    (
        &["sigemptyset", "sigaddset", "pthread_sigmask"],
        "build or set a signal mask",
    ),
    (&["abort", "_exit"], "end the process"),
];

fn is_inert_external(name: &str) -> bool {
    INERT
        .iter()
        .any(|(names, _)| super::libc::listed(names, name))
}

/// `%name` and `@name` tokens in an operand string, with quoted names.
/// Other quoted text (string constants, inline asm templates and
/// constraints) is skipped: a `%` or `@` there is data, not a value.
fn refs(s: &str) -> Vec<(bool, String)> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'"' {
            i = skip_quoted(b, i);
            continue;
        }
        if c == b'!' {
            // Metadata: `!foo !12`, never a value.
            i += 1;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || matches!(b[i], b'.' | b'_')) {
                i += 1;
            }
            continue;
        }
        if c == b'%' || c == b'@' {
            let global = c == b'@';
            i += 1;
            let name = if b.get(i) == Some(&b'"') {
                let start = i + 1;
                let mut j = start;
                while j < b.len() && b[j] != b'"' {
                    j += 1;
                }
                i = j + 1;
                s[start..j.min(b.len())].to_string()
            } else {
                let start = i;
                while i < b.len()
                    && (b[i].is_ascii_alphanumeric() || matches!(b[i], b'-' | b'$' | b'.' | b'_'))
                {
                    i += 1;
                }
                s[start..i].to_string()
            };
            if !name.is_empty() {
                out.push((global, name));
            }
            continue;
        }
        i += 1;
    }
    out
}

/// Split at top-level commas.
fn split_top(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut quote = false;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '"' => quote = !quote,
            '(' | '[' | '{' | '<' if !quote => depth += 1,
            ')' | ']' | '}' | '>' if !quote => depth -= 1,
            ',' if !quote && depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// The text inside the parentheses that open at `open`.
fn group(s: &str, open: usize) -> &str {
    let mut depth = 0;
    let mut quote = false;
    for (i, c) in s[open..].char_indices() {
        match c {
            '"' => quote = !quote,
            '(' if !quote => depth += 1,
            ')' if !quote => {
                depth -= 1;
                if depth == 0 {
                    return &s[open + 1..open + i];
                }
            }
            _ => {}
        }
    }
    &s[open + 1..]
}

struct Func {
    name: String,
    params: Vec<String>,
    body: Vec<String>,
}

/// A global, as [`split_module`] reads its definition.
struct Global {
    name: String,
    mutable: bool,
    /// Internal or private linkage: only this module can name it.
    internal: bool,
    /// Its type is `ptr`.
    holds_pointer: bool,
    /// The symbols its initialiser names.
    init: Vec<(bool, String)>,
}

/// The defined functions, with their parameter names and instruction lines
/// (a continuation line is appended to the instruction it continues), and
/// the globals with their initialisers' references.
#[allow(clippy::type_complexity)]
fn split_module(text: &str) -> (Vec<Func>, Vec<Global>, Vec<String>) {
    let mut funcs = Vec::new();
    let mut globals = Vec::new();
    let mut declared = Vec::new();
    let mut current: Option<Func> = None;
    for line in text.lines() {
        if let Some(f) = current.as_mut() {
            if line.starts_with('}') {
                funcs.push(current.take().unwrap());
                continue;
            }
            if line.starts_with("    ") || line.starts_with("\t\t") {
                if let Some(last) = f.body.last_mut() {
                    last.push(' ');
                    last.push_str(line.trim());
                    continue;
                }
            }
            if line.starts_with("  ") {
                f.body.push(line.trim().to_string());
            }
            continue;
        }
        if line.starts_with("define ") {
            let at = match line.find('@') {
                Some(a) => a,
                None => continue,
            };
            let rest = &line[at..];
            let Some((_, name)) = refs(rest).into_iter().next() else {
                continue;
            };
            let open = at + rest.find('(').unwrap_or(0);
            let params: Vec<String> = split_top(group(line, open))
                .iter()
                .filter_map(|p| refs(p).into_iter().find(|(g, _)| !g).map(|(_, n)| n))
                .collect();
            let mut f = Func {
                name,
                params,
                body: Vec::new(),
            };
            if line.trim_end().ends_with('{') {
                current = Some(f);
            } else {
                f.body.clear();
                funcs.push(f);
            }
        } else if line.starts_with("declare ") {
            if let Some(at) = line.find('@') {
                if let Some((_, n)) = refs(&line[at..]).into_iter().next() {
                    declared.push(n);
                }
            }
        } else if line.starts_with('@') {
            let rs = refs(line);
            if let Some((_, name)) = rs.first() {
                let init = rs[1..].iter().filter(|(g, _)| *g).cloned().collect();
                // `constant` before the type: never written.
                let head = line.split(['[', '{', '<', '"']).next().unwrap_or(line);
                let words: Vec<&str> = head.split_whitespace().collect();
                let mutable = !words.contains(&"constant");
                // Or a Rust symbol: the program is every crate's module
                // linked (modules.rs), so every use of one is here; only Rust
                // code names a mangled symbol, and a library exports only
                // `#[no_mangle]` names.
                let internal = words.iter().any(|w| matches!(*w, "internal" | "private"))
                    || name.starts_with("_R")
                    || name.starts_with("_ZN");
                // `global ptr @f`: a pointer, its initialiser one symbol.
                let holds_pointer = words.windows(2).any(|w| w == ["global", "ptr"]);
                globals.push(Global {
                    name: name.clone(),
                    mutable,
                    internal,
                    holds_pointer,
                    init,
                });
            }
        }
    }
    (funcs, globals, declared)
}

/// The index just past the quoted text that opens at `open`. IR spells a
/// quote inside quoted text as `\22`, so the next `"` always closes it.
fn skip_quoted(b: &[u8], open: usize) -> usize {
    let mut i = open + 1;
    while i < b.len() && b[i] != b'"' {
        i += 1;
    }
    i + 1
}

/// Where the callee's argument list opens: the `(` after the first `@name`
/// or `%name` that a `(` immediately follows, outside quoted text.
fn callee_open(rhs: &str) -> Option<usize> {
    let b = rhs.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'"' {
            i = skip_quoted(b, i);
            continue;
        }
        if b[i] == b'@' || b[i] == b'%' {
            let mut j = i + 1;
            if b.get(j) == Some(&b'"') {
                j += 1;
                while j < b.len() && b[j] != b'"' {
                    j += 1;
                }
                j += 1;
            } else {
                while j < b.len()
                    && (b[j].is_ascii_alphanumeric() || matches!(b[j], b'-' | b'$' | b'.' | b'_'))
                {
                    j += 1;
                }
            }
            if b.get(j) == Some(&b'(') {
                return Some(j);
            }
            i = j;
            continue;
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_text_names_no_value() {
        // An AT&T asm template's registers and a `;` or `@` in it are data.
        let line =
            r#"call void asm sideeffect "movq %rax, %rdi; call foo@PLT", "r,~{memory}"(ptr %p)"#;
        assert_eq!(refs(line), vec![(false, "p".to_string())]);
        let fake = r#"call void asm "jmp *%rax(", "r"(ptr %p)"#;
        assert_eq!(callee_open(fake), None);
        let named = r#"call void @"weird name"(ptr %p)"#;
        assert_eq!(
            &named[..callee_open(named).unwrap()],
            r#"call void @"weird name""#
        );
    }

    /// `depth` helpers, each calling through a field of what it is handed
    /// and handing it on, under a root that hands them a static's value.
    fn chain(depth: usize) -> String {
        let mut t = String::from(
            "@SLOT = internal global ptr null\n\
             define void @root() {\n  %p = load ptr, ptr @SLOT\n  call void @l0(ptr %p)\n  ret void\n}\n",
        );
        for i in 0..depth {
            let next = if i + 1 < depth {
                format!("  call void @l{}(ptr %p)\n", i + 1)
            } else {
                String::new()
            };
            t.push_str(&format!(
                "define internal void @l{i}(ptr %p) {{\n  %a = getelementptr inbounds i8, ptr %p, i64 {}\n  \
                 %f = load ptr, ptr %a\n  call void %f()\n{next}  ret void\n}}\n",
                8 * (i % 32)
            ));
        }
        t
    }

    /// A summary names what its caller passes, not the calls below it: its
    /// size stays flat however deep the chain, and every call is still
    /// reported where the static's value enters.
    #[test]
    fn a_deep_callback_chain_keeps_summaries_flat_and_reports_every_call() {
        let mut largest = Vec::new();
        for depth in [50, 400] {
            let text = chain(depth);
            let analysed: HashSet<String> = std::iter::once("root".to_string())
                .chain((0..depth).map(|i| format!("l{i}")))
                .collect();
            let (sites, sums, _) = analyse(&text, &analysed, &|_| false);
            let funcs: BTreeSet<&str> = sites.iter().map(|s| s.func.as_str()).collect();
            assert_eq!(funcs.len(), depth);
            assert!(sites
                .iter()
                .all(|s| s.at == "root" && s.sources == ["loaded from `SLOT`"]));
            largest.push(sums.iter().map(|s| s.pending.len()).max().unwrap_or(0));
        }
        assert_eq!(largest[0], largest[1], "{largest:?}");
        assert!(largest[1] <= WIDTH, "{largest:?}");
    }
    fn sites(text: &str, analysed: &[&str]) -> Vec<(String, String, Vec<String>)> {
        let analysed: HashSet<String> = analysed.iter().map(|s| s.to_string()).collect();
        let mut out: Vec<_> = analyse(text, &analysed, &|_| false)
            .0
            .into_iter()
            .map(|s| (s.at, s.func, s.sources))
            .collect();
        out.sort();
        out
    }

    /// `sigaction`'s third argument receives the handler installed before:
    /// calling it calls what someone else installed.
    #[test]
    fn a_handler_written_back_by_an_external_is_a_slot() {
        let text = "declare i32 @sigaction(i32, ptr, ptr)\n\
             define void @root() {\n  %new = alloca [16 x i8]\n  %old = alloca [16 x i8]\n  \
             %r = call i32 @sigaction(i32 2, ptr %new, ptr %old)\n  %h = load ptr, ptr %old\n  \
             call void %h(i32 2)\n  ret void\n}\n";
        assert_eq!(
            sites(text, &["root"]),
            [(
                "root".into(),
                "root".into(),
                vec!["returned by `sigaction`".into()]
            )]
        );
    }

    /// `signal` returns the handler installed before.
    #[test]
    fn a_handler_returned_by_an_external_is_a_slot() {
        let text =
            "declare ptr @signal(i32, ptr)\ndefine internal void @mine(i32 %s) {\n  ret void\n}\n\
             define void @root() {\n  %prev = call ptr @signal(i32 2, ptr @mine)\n  \
             call void %prev(i32 2)\n  ret void\n}\n";
        assert_eq!(
            sites(text, &["root", "mine"]),
            [(
                "root".into(),
                "root".into(),
                vec!["returned by `signal`".into()]
            )]
        );
    }

    /// An external writing through a pointer its caller was handed writes
    /// into the caller's caller's memory: the wrapper's summary carries it.
    #[test]
    fn an_external_writing_through_a_parameter_reaches_the_caller() {
        let text = "declare i32 @sigaction(i32, ptr, ptr)\n\
             define internal void @previous(ptr %old) {\n  \
             %r = call i32 @sigaction(i32 2, ptr null, ptr %old)\n  ret void\n}\n\
             define void @root() {\n  %old = alloca [16 x i8]\n  call void @previous(ptr %old)\n  \
             %h = load ptr, ptr %old\n  call void %h(i32 2)\n  ret void\n}\n";
        assert_eq!(
            sites(text, &["root", "previous"]),
            [(
                "root".into(),
                "root".into(),
                vec!["returned by `sigaction`".into()]
            )]
        );
    }

    /// `realloc` moves its argument's contents and `memchr` points into its
    /// argument: what a load through the result yields keeps its slots.
    #[test]
    fn a_result_into_the_argument_keeps_the_arguments_slots() {
        for (decl, call) in [
            (
                "declare ptr @realloc(ptr, i64)",
                "call ptr @realloc(ptr %p, i64 64)",
            ),
            (
                "declare ptr @memchr(ptr, i32, i64)",
                "call ptr @memchr(ptr %p, i32 0, i64 64)",
            ),
        ] {
            let text = format!(
                "@SLOT = internal global ptr null\n{decl}\n\
                 define void @root() {{\n  %p = load ptr, ptr @SLOT\n  %q = {call}\n  \
                 %f = load ptr, ptr %q\n  call void %f()\n  ret void\n}}\n"
            );
            assert_eq!(
                sites(&text, &["root"]),
                [(
                    "root".into(),
                    "root".into(),
                    vec!["loaded from `SLOT`".into()]
                )],
                "{decl}"
            );
        }
    }

    /// errno's slot and a directory entry are the C library's plain data:
    /// what a load through them yields is nothing the program could call.
    #[test]
    fn a_pointer_to_library_data_is_not_a_slot() {
        let text = "declare ptr @__error()\ndeclare i32 @readdir_r(ptr, ptr, ptr)\n\
             define void @root(ptr %dir) {\n  %e = call ptr @__error()\n  %f = load ptr, ptr %e\n  \
             call void %f()\n  %entry = alloca [1048 x i8]\n  %out = alloca ptr\n  \
             %r = call i32 @readdir_r(ptr %dir, ptr %entry, ptr %out)\n  %d = load ptr, ptr %out\n  \
             %g = load ptr, ptr %d\n  call void %g()\n  ret void\n}\n";
        assert_eq!(sites(text, &["root"]), []);
    }

    /// An inert external moves nothing: a callback the member put on its
    /// stack beside a buffer it hands one stays the member's own.
    #[test]
    fn an_inert_external_leaves_the_stack_alone() {
        let text =
            "declare i32 @clock_gettime(i32, ptr)\ndefine internal void @mine() {\n  ret void\n}\n\
             define void @root() {\n  %s = alloca [32 x i8]\n  store ptr @mine, ptr %s\n  \
             %ts = getelementptr inbounds i8, ptr %s, i64 16\n  \
             %r = call i32 @clock_gettime(i32 6, ptr %ts)\n  %f = load ptr, ptr %s\n  \
             call void %f()\n  ret void\n}\n";
        assert_eq!(sites(text, &["root", "mine"]), []);
    }
}
