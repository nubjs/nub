// A resolve hook that hands the entry a URL nub's own hooks never track: a
// cache-busting query, the shape a hot-reload loader adds. Scoped to the entry so
// nothing else in the process is evaluated a second time under a fresh URL.
import { register } from "node:module";

register(
  "data:text/javascript," +
    encodeURIComponent(`
export async function resolve(specifier, context, next) {
  const resolved = await next(specifier, context);
  if (resolved.url.endsWith("/plain.mjs")) resolved.url += "?v=1";
  return resolved;
}
`),
);
