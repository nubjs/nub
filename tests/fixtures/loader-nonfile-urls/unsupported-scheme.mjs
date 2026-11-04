// Node rejects an unsupported ESM URL scheme with ERR_UNSUPPORTED_ESM_URL_SCHEME.
// `.js` is deliberate: it is what used to route the URL into nub's transpile branch.
try {
  await import("http://example.com/foo.js");
  console.log("CODE none");
} catch (err) {
  console.log("CODE", err.code);
}
