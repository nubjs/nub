import { debounce, throttle } from "node:util";

const debounced = debounce((value) => value, 1);
const throttled = throttle((value) => value, 1, 1);
console.log((await Promise.all([debounced("debounce"), throttled("throttle")])).join(","));
