import fs from 'node:fs';
const R = JSON.parse(fs.readFileSync('/tmp/regdiff/results.json', 'utf8'));

// For each cell, for each tool, find the "real" dep request (ignore version self-checks
// like /npm, /pnpm, /-/v1/, ping). Report the dep URL + auth + whether it hit A or B.
function depRequests(reqs) {
  return reqs.filter(r => {
    const u = r.url;
    // strip self-checks and ping
    if (/\/(npm|pnpm|yarn|bun)$/.test(u)) return false;
    if (u.includes('/-/ping')) return false;
    if (u.includes('/-/v1/')) return false;
    return true;
  });
}

for (const [cellId, cell] of Object.entries(R)) {
  console.log(`\n=== ${cellId} ===`);
  console.log(`    ${cell.desc}`);
  if (cell.expectPort) console.log(`    expect: port ${cell.expectPort} (project/env override should win)`);
  for (const [tool, data] of Object.entries(cell.tools)) {
    const deps = depRequests(data.requests);
    const depsB = depRequests(data.requestsB || []);
    const fmt = (rs, label) => rs.map(r => `${r.url}${r.auth ? ` [AUTH:${r.auth.slice(0,30)}]` : ''}`).join('  ') || '(none)';
    let line = `  ${tool.padEnd(5)} A(${data.portA}): ${fmt(deps)}`;
    if (data.portB) line += `\n        B(${data.portB}): ${fmt(depsB)}`;
    console.log(line);
  }
}
