import data from "./reuse.yaml";

const configs = data.configs.flat();
const regions = data.regions.flat();
// Report the expanded values, not just a success signal: a loader that handed
// back an empty shell would still exit 0.
console.log(
  JSON.stringify({
    configs: configs.length,
    regions: regions.length,
    last: configs.at(-1),
    lastRegion: regions.at(-1),
  }),
);
