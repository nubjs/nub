// A handler whose module body keeps the event loop alive for good, so a detection
// pass that waited for the loop to drain would never run.
setInterval(() => {}, 1000);

export default {
  fetch() {
    return new Response("holds the loop");
  },
};
