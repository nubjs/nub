// Fetch the platform-matching @endevco/aube-<os>-<arch> sub-package at
// install time and hardlink (or copy) its three binaries into ./bin so
// npm's `bin` wrapper resolves directly to the native executable. The root
// package's bin targets are stable `./bin/<name>` paths so npm/npx can create
// shims without reading a rewritten package.json. On Windows, npm's generated
// `.cmd` shim needs a shebang target it can execute, so `./bin/<name>` is a
// tiny text file whose interpreter is the native `./bin/<name>.exe`. This
// mirrors https://www.npmjs.com/package/@jdxcode/mise — the preinstall
// approach avoids the JS shim at runtime and keeps `package-lock.json` free
// of six optional-dependency entries that are mostly skipped.
//
// Must stay CommonJS and use only the Node.js stdlib — it runs *before*
// any dependency is installed, so nothing from node_modules is reachable.

var spawn = require('child_process').spawn;
var path = require('path');
var fs = require('fs');

function main() {
    var pjson = require('./package.json');
    var version = pjson.version;

    // Nested `npm install` must stay local; otherwise it'd try to write
    // into the global prefix when the user ran `npm i -g @endevco/aube`.
    process.env.npm_config_global = 'false';

    var platform = process.platform; // darwin | linux | win32
    var arch = process.arch;         // arm64 | x64
    // On Linux, `process.report` exposes `glibcVersionRuntime` when the
    // runtime linked against glibc; its absence means musl (Alpine,
    // distroless-static). Same heuristic the `detect-libc` package uses.
    var suffix = '';
    if (platform === 'linux') {
        var glibc = '';
        try { glibc = process.report.getReport().header.glibcVersionRuntime || ''; } catch (_) {}
        if (!glibc) suffix = '-musl';
    }
    var subpkgName = '@endevco/aube-' + platform + '-' + arch + suffix;

    var npmCmd = platform === 'win32' ? 'npm.cmd' : 'npm';
    // --ignore-scripts: platform packages are passive binary carriers;
    // a compromised mirror/registry must not get RCE via lifecycle hooks
    // when the user installs the trusted root @endevco/aube.
    var args = ['install', '--no-save', '--no-package-lock', '--ignore-scripts', subpkgName + '@' + version];

    var cp = spawn(npmCmd, args, { stdio: 'inherit', shell: true });
    cp.on('close', function(code, signal) {
        // `code` is null when the child was killed by a signal (e.g.
        // OOM). `process.exit(null)` coerces to 0, which would tell
        // npm the preinstall succeeded — surface it as failure.
        if (signal || code === null) {
            console.error('[@endevco/aube] preinstall: `npm install ' + subpkgName + '` killed by ' + (signal || 'signal'));
            process.exit(1);
            return;
        }
        if (code !== 0) {
            process.exit(code);
            return;
        }
        try {
            linkSubpkgBins(subpkgName, platform);
            process.exit(0);
        } catch (e) {
            console.error('[@endevco/aube] preinstall failed: ' + (e && e.message ? e.message : e));
            process.exit(1);
        }
    });
}

// Only these names are ever produced by aube's own build pipeline; ignore
// any other keys the platform package's bin map might carry so a hostile
// or malformed sub-package can't smuggle in extra files.
var ALLOWED_BINS = ['aube', 'aubr', 'aubx'];

function isContained(parent, child) {
    var rel = path.relative(parent, child);
    return rel !== '' && !rel.startsWith('..') && !path.isAbsolute(rel);
}

function linkSubpkgBins(subpkgName, platform) {
    var subpkgJsonPath = require.resolve(subpkgName + '/package.json');
    var subpkg = JSON.parse(fs.readFileSync(subpkgJsonPath, 'utf8'));
    var subpkgDir = path.dirname(subpkgJsonPath);

    var binDir = path.resolve(__dirname, 'bin');
    try { fs.mkdirSync(binDir); } catch (e) { if (e.code !== 'EEXIST') throw e; }

    // Realpath the platform package dir once so the containment compares
    // happen in symlink-resolved space (the package itself can be
    // pnpm-style symlinked, which is fine — what we care about is whether
    // its `bin/*` entries escape the realpath of that directory).
    var subpkgRealDir = fs.realpathSync(subpkgDir);

    var subpkgBin = subpkg.bin || {};
    ALLOWED_BINS.forEach(function(name) {
        var srcRel = subpkgBin[name];
        if (typeof srcRel !== 'string') return;

        var src = path.resolve(subpkgDir, srcRel);
        // String-only containment first: rejects `../` traversal before
        // we ever touch the filesystem.
        if (!isContained(subpkgDir, src)) {
            throw new Error('platform package bin "' + name + '" escapes its package directory');
        }
        // Then realpath the source so a symlink inside the package can't
        // smuggle in an arbitrary on-disk file (e.g. `bin/aube -> ~/.ssh/id_rsa`).
        // The subsequent hardlink/copy follows symlinks, so a bare string
        // check would let `fs.copyFileSync` read straight through.
        var srcReal;
        try { srcReal = fs.realpathSync(src); } catch (e) {
            throw new Error('platform package bin "' + name + '" cannot be resolved: ' + (e && e.message ? e.message : e));
        }
        if (!isContained(subpkgRealDir, srcReal)) {
            throw new Error('platform package bin "' + name + '" resolves outside its package directory');
        }

        var destBasename = platform === 'win32' ? name + '.exe' : name;
        var dest = path.resolve(binDir, destBasename);
        // destBasename comes from our static allowlist, but guard anyway so
        // a future change to ALLOWED_BINS can't silently regress.
        if (!isContained(binDir, dest)) {
            throw new Error('refusing to write outside bin dir: ' + destBasename);
        }

        try { fs.unlinkSync(dest); } catch (e) { if (e.code !== 'ENOENT') throw e; }
        try {
            // Hardlink is cheapest (same inode, no extra disk). On some
            // filesystems (cross-device, restricted sandboxes) hardlink
            // fails — fall through to a copy. Use the realpath so we
            // never link/copy through a symlink that bypassed the check.
            fs.linkSync(srcReal, dest);
        } catch (e) {
            fs.copyFileSync(srcReal, dest);
        }
        if (platform !== 'win32') {
            try { fs.chmodSync(dest, 0o755); } catch (_) {}
        } else {
            var shim = path.resolve(binDir, name);
            if (!isContained(binDir, shim)) {
                throw new Error('refusing to write outside bin dir: ' + name);
            }
            try { fs.unlinkSync(shim); } catch (e) { if (e.code !== 'ENOENT') throw e; }
            fs.writeFileSync(shim, '#!' + dest.replace(/\\/g, '/') + '\n');
        }
    });
}

main();
