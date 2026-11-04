import Keyv from "keyv";
const k=new Keyv(); await k.set("a",1);
console.log("ok:" + await k.get("a"));
