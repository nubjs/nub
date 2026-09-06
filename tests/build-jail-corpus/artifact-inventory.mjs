import { existsSync, lstatSync, readdirSync, readFileSync, realpathSync, statSync } from 'node:fs';
import { join, relative } from 'node:path';

export function installedArtifacts(packageRoot) {
  const files = new Map();
  const seen = new Set();
  function record(path) {
    files.set(relative(packageRoot, path).replaceAll('\\', '/'), statSync(path).size > 0);
  }
  function walk(dir) {
    const real = realpathSync(dir);
    if (seen.has(real)) return;
    seen.add(real);
    for (const name of readdirSync(dir)) {
      if (name === 'node_modules' || name === '.git') continue;
      const path = join(dir, name);
      const stat = lstatSync(path);
      if (stat.isDirectory()) walk(path);
      else if (stat.isFile() && /\.(node|exe|dll|so|dylib)$/.test(name)) record(path);
    }
  }
  walk(packageRoot);
  const { bin } = JSON.parse(readFileSync(join(packageRoot, 'package.json'), 'utf8'));
  // Declared commands include extensionless native executables and script launchers.
  for (const path of typeof bin === 'string' ? [bin] : Object.values(bin ?? {})) {
    const full = join(packageRoot, path);
    if (existsSync(full) && statSync(full).isFile()) record(full);
  }
  return [...files].map(([path, nonempty]) => ({
    // esy signs through an mkdtemp copy, then copies the binary back to its stable path.
    // Keep those artifacts and their multiplicity; only the generated directory id differs.
    path: path.replace(/^esy-npm-bigsur-workaround-[A-Za-z0-9]{6}\//, 'esy-npm-bigsur-workaround-<temp>/'),
    nonempty,
  })).sort((a, b) => a.path.localeCompare(b.path) || Number(a.nonempty) - Number(b.nonempty));
}
