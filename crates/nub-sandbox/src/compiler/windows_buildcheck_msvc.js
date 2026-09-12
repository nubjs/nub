import Module from 'node:module';
import { readFileSync } from 'node:fs';
import { win32 as path } from 'node:path';
const policy = __NUB_BUILDCHECK_MSVC_JSON__;
const originalLoad = Module._load;

function buildcheckToolchain() {
  const arch = process.arch === 'ia32' ? 'x86' : process.arch;
  const major = Number.parseInt(policy.version, 10);
  const details = new Map([
    [16, { year: 2019, toolset: 'v142' }],
    [17, { year: 2022, toolset: 'v143' }],
    [18, { year: 2026, toolset: 'v145' }],
  ]).get(major);
  if (!details)
    return [];
  const vc = path.join(policy.vsRoot, 'VC');
  let toolsVersion;
  try {
    toolsVersion = readFileSync(path.join(vc, 'Auxiliary', 'Build', `Microsoft.VCToolsVersion.${details.toolset}.default.txt`), 'utf8').trim();
  } catch {
    toolsVersion = readFileSync(path.join(vc, 'Auxiliary', 'Build', 'Microsoft.VCToolsVersion.default.txt'), 'utf8').trim();
  }
  const msvc = path.join(vc, 'Tools', 'MSVC', toolsVersion);
  const sdkVersion = policy.sdkVersion.replace(/[\\/]+$/, '');
  return [{
    path: policy.vsRoot,
    version: { full: policy.version, major, minor: 0 },
    year: details.year,
    toolset: details.toolset,
    msbuild: path.join(policy.vsRoot, 'MSBuild', 'Current', 'Bin', arch === 'x64' ? 'amd64' : arch, 'MSBuild.exe'),
    cl: path.join(msvc, 'bin', `Host${arch}`, arch, 'cl.exe'),
    includePaths: [path.join(msvc, 'include')],
    libPaths: [path.join(msvc, 'lib', arch)],
    sdks: [{
      version: '10.0',
      fullVersion: sdkVersion,
      includePaths: ['shared', 'um', 'ucrt'].map((kind) => path.join(policy.sdkRoot, 'Include', sdkVersion, kind)),
      libPaths: ['um', 'ucrt'].map((kind) => path.join(policy.sdkRoot, 'Lib', sdkVersion, kind, arch)),
    }],
  }];
}

Module._load = function(request, parent, isMain) {
  if (request === './findvs.js'
      && parent
      && /(?:^|[\\/])node_modules[\\/]buildcheck[\\/]lib[\\/]index\.js$/i.test(parent.filename)) {
    return buildcheckToolchain;
  }
  return originalLoad.call(this, request, parent, isMain);
};
