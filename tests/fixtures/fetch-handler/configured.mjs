// `port` and `hostname` on the export are the two option keys the handler honors.
// `PORT` and `HOST` in the environment outrank both.
export default {
  port: 41999,
  // 127.0.0.1 rather than localhost, so a `HOST` that says `localhost` is
  // distinguishable in the reported URL even though both name the same interface.
  hostname: "127.0.0.1",
  fetch() {
    return new Response("configured");
  },
};
