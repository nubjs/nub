import pino from "pino";
// Stable metadata and synchronous output make the complete log comparable.
const log=pino({level:"info", timestamp:false, base:null}, pino.destination({dest:1, sync:true})); log.info("x");
console.log("ok:" + typeof log.info);
