# `ext/`: glue for third-party crates

Some third-party crates need a small piece of frustrate to work under
frustrate. `getrandom` is the example: on `wasm32-unknown-unknown` it has no
source of randomness, but it exposes a custom-backend hook, and frustrate has
the byte source that hook wants. getrandom will not depend on frustrate, and
frustrate will not make every consumer resolve `getrandom` for every target.
`ext/` holds that join: **one crate per third-party package, used by whoever
wants it and nobody else.**

## Rules

**Nothing in `runtime/rust` may depend on anything here.** That is the reason
the area exists.

**Off its target, an ext crate compiles to nothing** rather than being absent,
so a consumer can depend on it without a `cfg`, and it keeps out of the way on
targets where the third-party crate already works.

**Take the third-party dependency through a private `label_flag`.** The crate
links into the consumer's module next to the consumer's own copy of the
package, from the consumer's crate hub. Compiling against a different copy is
at best a duplicate and at worst an ABI mismatch. A consumer who has not
pointed the flag at their copy gets a rustc error, not wrong behaviour.

**Ship a platform, not a transition.** A build flag the glue needs goes on a
`platform()` in the ext crate that the consumer opts into by name. Setting it
in `frustrate_wasm_module`'s transition would force it on builds that chose
differently: `getrandom_backend` is the first arm of getrandom's `cfg_if`, so
setting it everywhere would override the `wasm_js` backend a wasm-bindgen build
chose and the `wasi` backend wasip1 already provides.

**Test the join, not the third-party crate:** that the flag reaches only the
crate that needs it, and that the whole chain links into a real module.
