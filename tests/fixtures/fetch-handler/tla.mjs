// Top-level await settles before the export is inspected, so a handler assembled
// asynchronously is still found.
const greeting = await Promise.resolve("ready after await");

export default {
  fetch() {
    return new Response(greeting);
  },
};
