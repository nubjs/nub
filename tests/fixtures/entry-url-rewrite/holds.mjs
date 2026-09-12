console.log(`holds:${new URL(import.meta.url).search}`);
setInterval(() => {}, 1000);
export default {
  fetch() {
    return new Response("held");
  },
};
