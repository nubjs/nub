import { copyFileSync, existsSync } from 'node:fs';
import { join } from 'node:path';

export function retainInputs(project, destination) {
  const names = ['package.json', 'nub.jsonc', '.npmrc', 'nub.lock', 'package-lock.json', 'pnpm-lock.yaml', 'bun.lock', 'yarn.lock'];
  const retained = names.filter(name => existsSync(join(project, name)));
  for (const name of retained) copyFileSync(join(project, name), join(destination, name));
  return retained;
}
