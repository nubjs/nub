import pino from "pino";
// Synchronous destination, deliberately: pino's default sonic-boom write to fd 1
// races console.log, so on a default destination the harness's `tail -1` sees the
// log line on some runs and the marker on others. Observed failing once in three
// runs. The transport path this fixture exists to exercise is unchanged.
const log=pino({level:"info"}, pino.destination({dest:1, sync:true})); log.info("x");
console.log("ok:" + typeof log.info);
