import { addDays, format } from "date-fns";
console.log("ok:" + format(addDays(new Date(2020,0,1),1),"yyyy-MM-dd"));
