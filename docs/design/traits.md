# Traits (trait objects across the bridge)

A bridged trait lets a function return `Box<dyn Store>` so the Dart caller works
with the trait and never learns the concrete type.

```rust
#[bridge(confined)]
pub trait Store: Send {
    fn get(&self, key: String) -> Option<String>;
    fn len(&self) -> usize { 0 }        // default bodies are fine
}

#[bridge] pub fn open_store(kind: String) -> anyhow::Result<Box<dyn Store>> { … }
#[bridge(sync)] pub fn store_report(store: &dyn Store) -> String { … }
```

## Why it costs almost nothing

A handle is the integer value of a thin pointer, and `*dyn Trait` is fat. But
`Box<dyn Trait>` is itself `Sized`, so a trait-object handle is an ordinary
opaque handle with `T = Box<dyn Trait>`. Every handle helper is already generic
over `T`; the runtime, codecs and wire are unchanged, and the cost is one extra
pointer chase on top of the vtable call the user asked for.

The **trait declaration is the bridged surface.** Concrete impls are not parsed
and need not be in a bridge file, and fn_ids are per trait method, not per impl.
Free functions are the factories, since a receiver-less associated fn is not
dyn-dispatchable — which is how the Rust is written anyway.

For a small closed set of implementations, a data enum is still the simpler
answer. It cannot carry a foreign or open-ended impl, and every variant pays
full serialization.

## Rules and their reasons

- **The trait declares its model's thread bound as a supertrait (FR0021).** A
  concrete opaque states the bound about itself; `dyn Store` erases the type,
  so the bound must come from the trait.
- **The pipeline checks dyn compatibility (FR0022)** rather than letting rustc
  fail inside generated code. A consuming member is written `self: Box<Self>`,
  because the glue holds a `Box<dyn Trait>`, which is not itself a `Trait`.
- **No `async fn` members, and no FR code for it.** rustc's `E0038` already
  rejects a non-dyn-compatible trait at the user's own line, soundly. Frozen and
  locked trait members are dispatched off the caller anyway; if a body must
  await, use a concrete opaque or a hand-written boxed-future method.
- **Every implementor shares the trait's model (FR0026)**, because a handle at a
  `&dyn Store` parameter is acquired under the trait's model; otherwise a
  Confined object could cross into Frozen pool semantics. Impl methods match the
  trait's exec and `on_contention` (FR0024) because the Dart class implements
  the trait's interface.
- **Actors take no part** (FR0023, FR0025): actor construction and calls go
  through the actor's own executor, which neither the trait-object spawn path
  nor trait dispatch can reach.

## Bridging impls

`#[bridge] impl Store for Sqlite` bridges the impl's methods onto the concrete
class, which then implements the trait's Dart interface with its own fn_ids.
This supplements the trait surface rather than replacing it: the polymorphic
return still needs the trait, and making impls primary would multiply the
generated surface by the number of impls.

A concrete handle passed where the trait is expected carries a one-byte impl
tag, derived identically by both emitters. A borrow unsizes to `&dyn Store`
(Locked takes the concrete lock), and a by-value parameter or consuming receiver
unwraps and re-boxes. Unknown tags and hand-written Dart implementors fail
loudly.

What is deliberately not offered is an **owned upcast**: turning a concrete
handle into a long-lived `Box<dyn Store>` handle while the concrete one keeps
working. Confined would lose its single owner, and Frozen/Locked would need
generated forwarding wrappers. Tagged positions cover the need.

This is safe because generated opaque classes are `final` with private
constructors, so no user type can subclass or instantiate a trait's Dart class,
and because both sides of the wire come from one codegen run, so the tag byte
has no cross-version contract.
