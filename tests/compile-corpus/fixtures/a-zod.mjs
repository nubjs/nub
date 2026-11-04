import { z } from "zod";
console.log("ok:" + z.object({a:z.number()}).safeParse({a:1}).success);
