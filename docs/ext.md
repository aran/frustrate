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

**A package the root hub lacks gets a dev-only hub for its tests.** tokio in
`frustrate_crates` would enter the lockfile of every triple, so `ext/tokio`'s
`label_flag` has no default that resolves: unset, it points at a target whose
failure says what to set. The library is `manual` so `//...` does not analyze
it; `tests/tokio_hook` runs it under a transition that sets the flag to the
dev hub's tokio.

**Ship a platform, not a transition.** A build flag the glue needs goes on a
`platform()` in the ext crate that the consumer opts into by name. Setting it
in `frustrate_wasm_module`'s transition would force it on builds that chose
differently: `getrandom_backend` is the first arm of getrandom's `cfg_if`, so
setting it everywhere would override the `wasm_js` backend a wasm-bindgen build
chose and the `wasi` backend wasip1 already provides.

**Test the join, not the third-party crate:** that the flag reaches only the
crate that needs it, and that the whole chain links into a real module.

## `ext/getrandom` on threaded web

On `@frustrate//bazel:wasm32_threads` a graph with getrandom 0.4.3 fails to
build with `E0433` on `js_sys`, inside getrandom. getrandom declares `js-sys`
under `cfg(target_feature = "atomics")`; crate_universe resolves dependencies
per triple, before `+atomics` exists, so it drops it. frustrate cannot add the
dependency for you: a crate annotation applies only to the module that declares
it. Two ways out.

**1. The ext platform.** Depend on `@frustrate//ext/getrandom:frustrate_getrandom`,
write `use frustrate_getrandom as _;` in the bridge crate, and build on
`@frustrate//ext/getrandom:wasm32_threads`:

```starlark
frustrate_wasm_module(
    name = "my_rust.wasm",
    library = ":my_rust",
    platform = "@frustrate//ext/getrandom:wasm32_threads",
)

frustrate_no_block_check(
    name = "no_block_check",
    bridge = ":codegen",
    library = ":my_rust",
    targets = ["web_threads"],
    web_threads_platform = "@frustrate//ext/getrandom:wasm32_threads",
)
```

getrandom takes its `custom` backend, so its `wasm_js` module, the one that
names `js-sys`, is never compiled. Entropy comes through frustrate's host import
instead of getrandom's own `wasm_js`; both end at `crypto.getRandomValues`. No
version appears in your build files, so a lockfile bump cannot turn it off.

It fails to compile when anything in the graph enables getrandom 0.4's `wasm_js`
feature (iroh does): 0.4.3's `error.rs` refers to `Error::WEB_CRYPTO` under that
feature alone, and only the `wasm_js` module defines it, so with `custom`
selected rustc reports `E0599` on `WEB_CRYPTO`. getrandom 0.3 gates the line on
the backend as well and is not affected. To find out whether your graph enables
the feature, run `cargo tree -e features -i getrandom@0.4.3` against the manifest
your crate hub reads, or look for `"wasm_js"` in `crate_features` in the BUILD file the hub
generates for getrandom 0.4.3.
If it is enabled, use route 2; if not, route 1 is the one that survives version
changes.

**2. A crate annotation** in your root `MODULE.bazel`, which keeps getrandom's
`wasm_js` backend:

```starlark
crate.annotation_select(
    crate = "getrandom",
    version = "0.4.3",
    deps = ["@your_crates//:js-sys"],
    repositories = ["your_crates"],
    triples = ["wasm32-unknown-unknown"],
)
```

It names getrandom's version. When the lockfile moves getrandom to another
version it stops matching, without an error, and the `E0433` returns.

`web_threads_platform` makes `frustrate_no_block_check` build under the platform
the app ships on, so route 1 is checked as shipped.
