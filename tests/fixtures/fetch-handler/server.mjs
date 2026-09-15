export default {
  fetch(request) {
    const url = new URL(request.url);
    if (url.pathname === "/echo") {
      return new Response(request.body, {
        status: 201,
        headers: { "x-method": request.method, "x-seen": request.headers.get("x-send") ?? "" },
      });
    }
    if (url.pathname === "/cookies") {
      const headers = new Headers();
      headers.append("set-cookie", "a=1; Expires=Wed, 21 Oct 2026 07:28:00 GMT");
      headers.append("set-cookie", "b=2");
      return new Response("cookies", { headers });
    }
    if (url.pathname === "/stream") {
      const stream = new ReadableStream({
        start(controller) {
          controller.enqueue(new TextEncoder().encode("chunk-one\n"));
          controller.enqueue(new TextEncoder().encode("chunk-two\n"));
          controller.close();
        },
      });
      return new Response(stream, { headers: { "content-type": "text/plain" } });
    }
    if (url.pathname === "/empty") return new Response(null, { status: 204 });
    if (url.pathname === "/teapot") return new Response("short and stout", { status: 418 });
    if (url.pathname === "/throw") throw new Error("handler blew up");
    if (url.pathname === "/not-a-response") return "just a string";
    return new Response(`hello from ${url.pathname}?${url.searchParams.get("q") ?? ""}`);
  },
};
