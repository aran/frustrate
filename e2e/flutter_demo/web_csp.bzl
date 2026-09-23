"""The strict Content-Security-Policy the demo's web bundles are served under.

The policy lives here rather than in `web/index.html`, and a `-c dbg` bundle
goes without it, because dwds — the hot-restart channel — starts the app with
an inline `<script>`. Any policy without `'unsafe-inline'` kills the development
loop before the engine boots. `flutter run -d chrome` serves `web/index.html`,
so that page carries only an inert `<!-- $FRUSTRATE_CSP -->` marker;
`flutter_bazel run` serves the bundle's own page from a `-c dbg` build, so the
marker stays inert there too. Every other mode gets the policy substituted in.
This is also the shape a real app ships: a response header at the CDN, not a
tag the developer trips over locally.

Injection is allowed to fail silently — a page that never got the substitution
is simply unprotected — so both Playwright specs assert the policy is present
*and enforced* (an inline script must be blocked). Deleting either the marker
or this substitution turns those specs red; nothing here is self-checking.

## What the policy says, and whose constraints they are

    script-src 'self' 'wasm-unsafe-eval'; worker-src 'self' blob:;

- `'wasm-unsafe-eval'` is the floor for WebAssembly: `WebAssembly.compile`
  from bytes needs it, for dart2wasm itself as much as for the bridge module.
- `worker-src blob:` is a **Flutter-engine constraint, not frustrate's**.
  flutter.js hardcodes a `URL.createObjectURL` bootstrap for skwasm's threaded
  render worker (the web SDK ships no served `.ww.js` at all). Frustrate is
  clean under `worker-src 'self'`: the integration suite instruments
  `createObjectURL` before init and asserts zero calls, and both specs pin the
  blob-worker count to the engine's single baseline worker.
- No `'unsafe-inline'`: that is the claim being proved. The bridge serves its
  glue as a real asset instead of injecting it.

`use_local_canvaskit = True` on the app target is required by `script-src
'self'` — without it the engine loads the renderer from the gstatic CDN. The
bundle ships the renderer either way; the attribute only decides which copy
flutter.js reaches for. That coupling is why the CDN is a non-issue for the
dev loop: no policy on the page, nothing to block the CDN fetch.
"""

load("@bazel_skylib//rules:expand_template.bzl", "expand_template")

# Keep the two halves adjacent: the marker is what the source page carries,
# the policy is what replaces it.
FRUSTRATE_CSP_MARKER = "<!-- $FRUSTRATE_CSP -->"

FRUSTRATE_STRICT_CSP = "script-src 'self' 'wasm-unsafe-eval'; worker-src 'self' blob:;"

def strict_csp_index_html(name, src = "web/index.html", base_href = "/", **kwargs):
    """Produces the bundled index.html: the source page with the CSP injected,
    except in a `-c dbg` build.

    Also substitutes `$FLUTTER_BASE_HREF`, which `flutter_web_app` would have
    done via flutter_web_index_html_subst — composing flutter_web_bundle by
    hand means doing it here instead.

    Args:
        name: Target name; the output is `<name>.html`.
        src: The source page carrying [FRUSTRATE_CSP_MARKER].
        base_href: Value substituted for `$FLUTTER_BASE_HREF`.
        **kwargs: Additional arguments (e.g. tags, visibility).
    """
    expand_template(
        name = name,
        out = name + ".html",
        template = src,
        substitutions = select({
            Label("//:dbg"): {"$FLUTTER_BASE_HREF": base_href},
            "//conditions:default": {
                FRUSTRATE_CSP_MARKER: '<meta http-equiv="Content-Security-Policy" content="%s">' % FRUSTRATE_STRICT_CSP,
                "$FLUTTER_BASE_HREF": base_href,
            },
        }),
        **kwargs
    )
