// A resolve hook that hands the entry a URL nub's own hooks never track: a
// cache-busting query, the shape a hot-reload loader adds. Keyed on the entry
// having no parent — Node's own import of it is the only resolve with none — so a
// later import of the same file from anywhere else is left alone, which is what
// makes a second evaluation of the entry observable.
import { register } from "node:module";

register(
  "data:text/javascript," +
    encodeURIComponent(`
export async function resolve(specifier, context, next) {
  const resolved = await next(specifier, context);
  if (context.parentURL === undefined) resolved.url += "?v=1";
  return resolved;
}
`),
);
