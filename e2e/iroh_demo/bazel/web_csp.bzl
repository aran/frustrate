"""The Content-Security-Policy Tin Can's web bundles are served under.

Both pages carry a `{{FRUSTRATE_CSP}}` placeholder where the policy's `<meta>`
goes, and `csp_web_defines()` fills it for `flutter_web_app`'s `web_defines`.
A `-c dbg` build gets a comment instead: that is what `flutter_bazel run`
builds and serves, and dwds starts the app there with an inline `<script>`,
which this policy (no `'unsafe-inline'`) would block before `main()` ran.

web/index.html says what each source expression is for. The Playwright specs
assert the policy is present and enforced on the default-mode bundles.
"""

FRUSTRATE_CSP = "script-src 'self' 'wasm-unsafe-eval'; worker-src 'self' blob:; connect-src 'self' https: wss:;"

def csp_web_defines():
    return select({
        Label("//bazel:dbg"): {
            "FRUSTRATE_CSP": "<!-- no Content-Security-Policy in a -c dbg build: bazel/web_csp.bzl -->",
        },
        "//conditions:default": {
            "FRUSTRATE_CSP": '<meta http-equiv="Content-Security-Policy" content="%s">' % FRUSTRATE_CSP,
        },
    })
