// NEGATIVE CONTROL — proves `satisfies ExportedHandler` rejects the mistake it exists
// to catch. A misspelled `fetch` key makes `nub <file>` exit with no error and no port
// bound. Without this fixture the type could accept any object and still read as
// coverage, since the positive fixture only ever hands it a correct handler.
// Expected: tsc --noEmit exits NON-ZERO (TS2561, "Did you mean to write 'fetch'?").

import type { ExportedHandler } from "@nubjs/types";

export default {
  fecth(request: Request) {
    return new Response(request.url);
  },
} satisfies ExportedHandler;
