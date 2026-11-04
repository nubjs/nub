import type { LanguageRegistration } from '@shikijs/types';
import grammar from './env-spec.tmLanguage.json';

/* Syntax highlighting for `@env-spec` — the schema format Varlock reads out of a
   `.env.schema`. Shiki bundles no grammar for it, so a ```env-spec fence would
   otherwise render as unhighlighted plain text, which is a poor showing on the page
   whose whole subject is that format.

   The grammar is Varlock's own, vendored verbatim from their VSCode extension
   (`dmno-dev/varlock`, packages/vscode-plugin/language/env-spec.tmLanguage.json @
   6283b725, MIT © DMNO Inc.). Vendored rather than depended on because the npm
   package is a VSCode extension, not a library — installing it would pull an
   editor plugin in to read one JSON file. Re-copy that file to update.

   `scopeName` and `patterns` come from the grammar as published; `name` is what a
   fence's info string matches, and shiki requires it on a LanguageRegistration
   where a TextMate grammar does not carry one. */
export const envSpecLang = {
  ...grammar,
  name: 'env-spec',
  aliases: ['envspec'],
} as unknown as LanguageRegistration;
