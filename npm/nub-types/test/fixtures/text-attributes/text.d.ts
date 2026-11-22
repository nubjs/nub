// The declaration the loaders docs tell TypeScript 7.1 users to add. TypeScript
// recommends that packages not ship it, so it stays out of @nubjs/types.
declare module "*" with { type: "text" } {
  const text: string;
  export default text;
}
