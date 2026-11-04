// A data: URL carrying `with { type: "text" }` must get Node's own text
// strategy on a native-import-text Node — not nub's unknown-data-URL trap.
// On a Node without the feature, report SKIP and exit 0.
let supported = false;
try {
  supported = process.allowedNodeEnvironmentFlags.has("--experimental-import-text") ||
    process.versions.node.split(".").map(Number)[0] >= 25;
} catch {}
if (!supported) {
  console.log("SKIP no import-text on this Node");
} else {
  const { default: text } = await import("data:text/plain,hello%20text", { with: { type: "text" } });
  console.log("TEXT " + text);
}
