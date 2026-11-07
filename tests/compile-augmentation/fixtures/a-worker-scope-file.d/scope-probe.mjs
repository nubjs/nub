let imports;
try {
  importScripts();
  imports = "works";
} catch {
  imports = "throws";
}
const own = [typeof self, typeof postMessage, imports].join("/");
if (self.name === "nested") {
  postMessage(own);
} else {
  const nested = new Worker(new URL(import.meta.url), { name: "nested" });
  nested.onmessage = (event) => {
    nested.terminate();
    postMessage(`${own}+${event.data}`);
  };
  nested.onerror = (event) => postMessage(`${own}+error(${event.message})`);
}
