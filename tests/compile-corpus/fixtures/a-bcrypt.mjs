import bcrypt from "bcrypt";
console.log("ok:" + (bcrypt.hashSync("pw",4).length > 10));
