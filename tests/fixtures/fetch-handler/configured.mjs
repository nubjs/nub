// A file written for Bun, which reads `port` and `hostname` off the default export.
// Nub reads neither: the listener's address comes from `PORT` and `HOST` alone.
export default {
  port: 41999,
  hostname: "127.0.0.1",
  fetch() {
    return new Response("configured");
  },
};
