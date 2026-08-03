import pino from "pino";
const log=pino({level:"info"}); log.info("x");
console.log("ok:" + typeof log.info);
