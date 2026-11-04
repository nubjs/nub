process.on("message", (m) => { process.send({ echo: m.n * 2 }); process.exit(0); });
