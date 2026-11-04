type Greeting = { readonly name: string };

// `satisfies` and the enum are non-erasable syntax on purpose: a `.ts` entry has to
// be transpiled before its default export can be inspected at all.
enum Status {
  Ok = 200,
}

export default {
  fetch(request: Request): Response {
    const who = { name: new URL(request.url).pathname.slice(1) || "world" } satisfies Greeting;
    return new Response(`hello ${who.name}`, { status: Status.Ok });
  },
};
