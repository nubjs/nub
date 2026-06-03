import { exec } from "node:child_process";
let ok = 0;
let fail = 0;
let done = 0;
for (let i = 0; i < 50; i++) {
  exec('node -e "process.exit(0)"', { timeout: 10000 }, (err) => {
    if (err) fail++; else ok++;
    done++;
    if (done === 50) console.log("concurrent:" + ok + "/50,fail:" + fail);
  });
}
