// Ambient types for the loader hook. Referenced from the project as
// `/// <reference types="@nubjs/plugin-yaml/register" />`, or listed under
// `compilerOptions.types`. On TypeScript >= 7.1 with the content mapper active,
// a relative import resolves to the mapped file and gets that file's real shape;
// these wildcards are the fallback for older compilers and for editors without
// the mapper.
//
// MUST stay a script file (no top-level import/export): wildcard module
// declarations are only project-wide from a script.
declare module "*.yaml" {
  const data: Record<string, unknown>;
  export default data;
}
declare module "*.yml" {
  const data: Record<string, unknown>;
  export default data;
}
