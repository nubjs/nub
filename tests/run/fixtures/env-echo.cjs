// Prints the manifest-derived npm lifecycle values a script is entitled to.
console.log([
  process.env.npm_package_name,
  process.env.npm_package_version,
  process.env.npm_package_json ? "json" : "no-json",
  process.env.npm_lifecycle_event,
].join(" "));
