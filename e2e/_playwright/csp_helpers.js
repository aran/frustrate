// @ts-check
//
// The CSP proof's positive half.
//
// Everything the gallery specs assert about CSP is negative: nothing violated
// the policy, and no blob: worker appeared beyond the engine's one. Negative
// assertions pass just as happily against a page carrying no policy at all —
// so before this file existed, deleting the <meta> would have left the whole
// suite green while the proof it stands for quietly evaporated. That is not
// hypothetical: each demo substitutes its policy into the bundle at build time,
// and leaves it out of a -c dbg build so the dev loop's page can start, and a
// substitution is exactly the kind of thing that can silently stop happening.
//
// So this pins the premise: the served bundle really does carry the strict
// policy, and the browser really is enforcing it.
//
// `expect` is the caller's: this file sits outside every demo's node_modules,
// so it cannot require @playwright/test itself.

/**
 * Assert the served page carries the strict policy and the browser enforces it.
 *
 * Give this its own test (hence its own page): the enforcement probe
 * deliberately trips a CSP violation, which in a gallery run would land in the
 * console and fail hasUnexpectedErrors().
 *
 * @param {import('@playwright/test').Page} page
 * @param {string} url
 * @param {import('@playwright/test').Expect} expect
 */
async function expectStrictCspEnforced(page, url, expect) {
  await page.goto(url);

  // 1. Present — and strict in the one way that carries the claim. The
  //    bridge's entire served-glue design exists to avoid needing
  //    'unsafe-inline'; a policy that granted it would still
  //    satisfy every other assertion in these specs.
  const policy = await page.evaluate(() => {
    const meta = document.querySelector('meta[http-equiv="Content-Security-Policy"]');
    return meta ? meta.getAttribute('content') : null;
  });
  expect(
    policy,
    'no CSP meta in the served bundle — was it built -c dbg, or did the substitution not run?',
  ).not.toBeNull();
  expect(policy).toContain("script-src 'self'");
  expect(policy).not.toContain('unsafe-inline');

  // 2. Enforced, not merely declared. page.evaluate runs through CDP and is
  //    itself exempt from CSP; a <script> element it inserts into the DOM is
  //    not. That asymmetry is what makes this a real check — it separates a
  //    live policy from a decorative one.
  const inlineRan = await page.evaluate(() => {
    const s = document.createElement('script');
    s.textContent = 'window.__frustrateInlineRan = true;';
    document.head.appendChild(s);
    return window.__frustrateInlineRan === true;
  });
  expect(
    inlineRan,
    'an inline <script> ran: the policy is declared but not in force',
  ).toBe(false);
}

module.exports = { expectStrictCspEnforced };
