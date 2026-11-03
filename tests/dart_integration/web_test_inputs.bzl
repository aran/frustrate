"""The Bazel-built inputs of the browser suite, named for its driver."""

load("@rules_dart//dart:providers.bzl", "DartInfo")
load("@rules_dart//dart:utils.bzl", "dart_analyzable_info")

def _package_entry(dep):
    info = dep[DartInfo]
    language_version = ""
    for record in info.transitive_packages.to_list():
        if record.package_name == info.package_name:
            language_version = record.language_version
    return info.package_name, {
        "root": info.lib_root,
        "languageVersion": language_version,
    }, info.srcs

def _web_test_inputs_impl(ctx):
    packages = {}
    files = []
    for dep in ctx.attr.packages:
        name, entry, srcs = _package_entry(dep)
        packages[name] = entry
        files.extend(srcs)

    # One package per web compiler, same name each: the driver adds the one
    # for the compiler it runs, as `dart_js_binary` and `dart_wasm_binary` do
    # by building for a platform with `@rules_dart//dart/constraints:js` or
    # `:wasm`.
    compiler_packages = {}
    for compiler, dep in ctx.attr.compiler_packages.items():
        name, entry, srcs = _package_entry(dep)
        compiler_packages[compiler] = dict(entry, name = name)
        files.extend(srcs)
    fixtures = {}
    labels = {}
    for target, path in ctx.attr.fixtures.items():
        outputs = target.files.to_list()
        if len(outputs) != 1:
            fail("%s: fixture %s is %d files, not one" % (ctx.label, target.label, len(outputs)))
        fixtures[path] = outputs[0].short_path
        labels[path] = str(target.label)
        files.append(outputs[0])
    prefix = ctx.label.package + "/"
    tests = []
    for src in ctx.files.srcs:
        if not src.short_path.startswith(prefix):
            fail("%s: %s is not in this package" % (ctx.label, src.short_path))
        if src.basename.endswith("_test.dart"):
            tests.append(src.short_path[len(prefix):])
    manifest = ctx.actions.declare_file(ctx.label.name + ".json")
    ctx.actions.write(manifest, json.encode_indent({
        "packages": packages,
        "compilerPackages": compiler_packages,
        "fixtures": fixtures,
        "labels": labels,
        "threaded": ctx.attr.threaded,
        "facilities": ctx.attr.facilities,
        "release": ctx.attr.release,
        "tests": tests,
    }))
    return [
        DefaultInfo(
            files = depset([manifest]),
            runfiles = ctx.runfiles(files = [manifest] + files + ctx.files.srcs),
        ),
        # The `dart_analyze` aspect checks `srcs` against `packages` and
        # `deps`, the closure the browser run resolves them in.
        dart_analyzable_info(
            deps = ctx.attr.packages + ctx.attr.deps,
            srcs = ctx.files.srcs,
        ),
    ]

web_test_inputs = rule(
    implementation = _web_test_inputs_impl,
    doc = "The browser suite's sources, with a JSON manifest of the Dart " +
          "packages and fixture files they need, all of them in its " +
          "runfiles. Paths are runfiles-relative: `packages` maps a package " +
          "name to its root (the parent of `lib/`), `fixtures` maps the path " +
          "a test fetches a file by to the file, and `labels` the target each one " +
          "is built by. `tests` lists the test files " +
          "of `srcs`, relative to this package.",
    attrs = {
        "srcs": attr.label_list(
            allow_files = [".dart"],
            doc = "The suite's sources, tests and helpers, in this package.",
        ),
        "packages": attr.label_list(
            providers = [DartInfo],
            doc = "Libraries the tests import by their own package names " +
                  "that pub cannot resolve.",
        ),
        "deps": attr.label_list(
            providers = [DartInfo],
            doc = "The other libraries `srcs` import. Pub resolves these for " +
                  "the browser run; here they resolve `srcs` for analysis.",
        ),
        "compiler_packages": attr.string_keyed_label_dict(
            providers = [DartInfo],
            doc = "Per-compiler variants of a package, keyed by the " +
                  "compiler: `js` (dart2js) or `wasm` (dart2wasm). The " +
                  "driver adds the one for the compiler it runs under, in " +
                  "the place Bazel's binaries get theirs from " +
                  "their platform's `@rules_dart//dart/constraints:web_compiler`.",
        ),
        "fixtures": attr.label_keyed_string_dict(
            allow_files = True,
            doc = "Each single-file target, keyed to the path it is served at.",
        ),
        "threaded": attr.string_dict(
            doc = "Fixture paths whose file under the driver's `--threaded` " +
                  "is the named target's output instead. Labels, not " +
                  "dependencies: those targets need the locally built " +
                  "+atomics std, so the driver builds them only when asked.",
        ),
        "facilities": attr.string_dict(
            doc = "Like `threaded`, for the driver's `--facilities`: the " +
                  "targets built against the std whose stubs ask the host, " +
                  "which needs that std staged locally.",
        ),
        "release": attr.string_list(
            doc = "Fixture paths the driver's `--release` rebuilds with " +
                  "`-c opt`, from whichever target the other flags chose " +
                  "for the path. The rest keep the build `bazel run` made.",
        ),
    },
)
