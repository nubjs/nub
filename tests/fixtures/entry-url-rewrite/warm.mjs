// A preload that imports the entry ahead of the program, under a query of its own:
// a second module, evaluated first, whose handler is not the one to serve.
await import("./holds.mjs?warm");
