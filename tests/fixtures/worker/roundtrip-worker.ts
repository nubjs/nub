// Worker entry exercising the INBOUND path: receive via the web `self.onmessage`
// API (not Node's parentPort) and reply via `self.postMessage`. If the polyfill
// didn't wire parentPort → self message events, this handler would never fire.
self.onmessage = (e: MessageEvent) => {
  self.postMessage("echo:" + e.data);
};
