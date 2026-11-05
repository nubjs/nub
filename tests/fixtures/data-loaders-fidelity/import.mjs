import { describe } from "./describe.cjs";
import yaml from "./doc.yaml";
import toml from "./doc.toml";
import json5 from "./doc.json5";
console.log("yaml:" + describe(yaml));
console.log("toml:" + describe(toml));
console.log("json5:" + describe(json5));
