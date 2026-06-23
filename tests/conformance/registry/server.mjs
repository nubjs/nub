// Mock npm registry that logs every request (method, url, host header, authorization)
// then returns 404 so the PM stops. One server, capture file appended per request.
import http from 'node:http';
import fs from 'node:fs';
const PORT = process.argv[2] || 4999;
const LOG = process.argv[3] || '/tmp/regdiff/requests.log';
const srv = http.createServer((req, res) => {
  const entry = {
    t: Date.now(),
    method: req.method,
    url: req.url,
    host: req.headers['host'] || '',
    authorization: req.headers['authorization'] || '',
    'npm-auth-type': req.headers['npm-auth-type'] || '',
  };
  fs.appendFileSync(LOG, JSON.stringify(entry) + '\n');
  // Return 404 to halt resolution quickly
  res.statusCode = 404;
  res.setHeader('content-type', 'application/json');
  res.end(JSON.stringify({ error: 'Not found' }));
});
srv.listen(PORT, '127.0.0.1', () => {
  fs.appendFileSync(LOG, JSON.stringify({ t: Date.now(), _started: PORT }) + '\n');
});
