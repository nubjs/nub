import express from "express";
const app = express(); app.get("/", (_q,r)=>r.send("hi"));
console.log("ok:" + typeof app.listen);
