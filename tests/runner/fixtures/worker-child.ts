import { parentPort } from "node:worker_threads";
import { greet, Color } from "./util.ts";
parentPort!.postMessage(greet("thread") + " " + Color.Blue);
