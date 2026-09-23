# wasi demo — a bridge crate that needs a working `std`

An event log. Each entry is a v4 UUID, a wall-clock timestamp and an elapsed
time — about the smallest thing you can write that needs entropy, both clocks
and `println!`, none of which plain `wasm32-unknown-unknown` has.

```
bazel build //:app_web && (cd playwright && npx playwright test)   # web
bazel run //:app                                                   # macOS
bazel test //...                                                   # both, plus the wasm module
```

## The one line this example is about

`BUILD.bazel`:

```python
frustrate_wasm_module(
    name = "wasi_rust.wasm",
    crate = "//bridge:wasi_rust",
    platform = "@frustrate//bazel:wasm32_wasi",   # <-- the whole choice
)
```

Everything else in the module is ordinary Flutter-under-Bazel.
