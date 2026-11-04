// tsconfig paths alias, extensionless import, and a YAML data import.
import { fromAlias } from "@u/alias";
import { greet } from "./util";
import cfg from "./conf.yaml";
console.log(fromAlias, greet("x"), cfg.name, cfg.items.length);
