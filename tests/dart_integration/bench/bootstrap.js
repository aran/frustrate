// Launches the compiled bench module. A separate file rather than an inline
// <script> because index.html's CSP has no 'unsafe-inline' — see the comment
// there; the strictness is what forces the runtime onto its served-glue worker
// path instead of blob:.
//
// Also the page's last-resort error channel. The driver has no CDP connection
// (no Playwright, no node — Dart drivers only), so anything that escapes the
// Dart side would otherwise be invisible and present as a bare timeout. These
// handlers turn it into a POST the driver can print.

const report = (what) => {
  try {
    fetch('/fatal', { method: 'POST', body: String(what) });
  } catch (_) {}
  const el = document.getElementById('status');
  if (el) el.textContent = 'FAILED: ' + what;
};

window.addEventListener('error', (e) =>
  report('window.onerror: ' + (e.message || e.error) +
         ' @ ' + e.filename + ':' + e.lineno));
window.addEventListener('unhandledrejection', (e) =>
  report('unhandledrejection: ' + (e.reason && e.reason.stack || e.reason)));

try {
  const { compileStreaming, instantiate, invoke } = await import('./web_bench.mjs');
  const compiled = await compileStreaming(fetch('web_bench.wasm'));
  invoke(await instantiate(compiled, {}));
} catch (e) {
  report('bootstrap: ' + (e && e.stack || e));
}
