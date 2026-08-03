import Database from "better-sqlite3";
const db=new Database(":memory:"); db.exec("create table t(x)");
console.log("ok:" + db.prepare("select count(*) c from t").get().c);
