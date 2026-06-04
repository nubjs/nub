CARGO   ?= cargo
PROFILE ?= release
BIN_DIR ?= /usr/local/bin
TARGET   = target/$(PROFILE)/nub

ifeq ($(PROFILE),release)
  CARGO_FLAGS = --release
else
  CARGO_FLAGS =
endif

.PHONY: build addon install-dev uninstall-dev test test-node-matrix clean npm-build npm-publish npm-publish-dry

build: addon
	$(CARGO) build $(CARGO_FLAGS)

addon:
	$(CARGO) build -p nub-native $(CARGO_FLAGS)
	@mkdir -p runtime/addons
	@cp target/$(PROFILE)/libnub_native.dylib runtime/addons/nub-native.node 2>/dev/null || \
	 cp target/$(PROFILE)/libnub_native.so runtime/addons/nub-native.node 2>/dev/null || \
	 cp target/$(PROFILE)/nub_native.dll runtime/addons/nub-native.node 2>/dev/null || \
	 echo "Warning: could not find nub-native library"
	@echo "Built: runtime/addons/nub-native.node"

install-dev: build
	ln -sf $(CURDIR)/$(TARGET) $(BIN_DIR)/nub-dev
	ln -sf $(CURDIR)/$(TARGET) $(BIN_DIR)/nubx-dev
	@echo "Installed: $(BIN_DIR)/nub-dev -> $(TARGET)"
	@echo "Installed: $(BIN_DIR)/nubx-dev -> $(TARGET)"
	@echo ""
	@nub-dev --version

uninstall-dev:
	rm -f $(BIN_DIR)/nub-dev $(BIN_DIR)/nubx-dev
	@echo "Removed nub-dev and nubx-dev from $(BIN_DIR)"

test:
	$(CARGO) test

# Run the integration suite across a Node version matrix (18.19 floor → 22.15
# fast-path floor) — the local mirror of ci.yml's `test` job. Locates or
# downloads each Node under ~/.cache/nub-test-node. See the script header.
test-node-matrix:
	@bash wiki/scripts/test-node-matrix.sh

clean:
	$(CARGO) clean

# ── npm packaging ───────────────────────────────────────────────────

# Set version across all npm packages + Cargo.toml + preload.mjs. Usage: make version V=0.0.3
# Portable (node-based, no macOS-only sed). preload.mjs NUB_VERSION must stay in
# lockstep with the binary version — it is the transpile-cache key, so a stale
# value would serve stale cached output after an upgrade.
version:
	@test -n "$(V)" || (echo "Usage: make version V=0.0.3" && exit 1)
	@echo "Setting version to $(V) across all packages, Cargo.toml, and preload.mjs..."
	@node -e " \
		const fs = require('fs'); \
		const v = '$(V)'; \
		const pkgs = ['npm/nub/package.json', \
			'npm/nub-darwin-arm64/package.json', 'npm/nub-darwin-x64/package.json', \
			'npm/nub-linux-x64/package.json', 'npm/nub-linux-x64-musl/package.json', \
			'npm/nub-linux-arm64/package.json', 'npm/nub-linux-arm64-musl/package.json', \
			'npm/nub-win32-x64/package.json', 'npm/nub-win32-arm64/package.json']; \
		for (const f of pkgs) { \
			const p = JSON.parse(fs.readFileSync(f, 'utf8')); \
			p.version = v; \
			if (p.optionalDependencies) { \
				for (const k of Object.keys(p.optionalDependencies)) p.optionalDependencies[k] = v; \
			} \
			fs.writeFileSync(f, JSON.stringify(p, null, 2) + '\n'); \
		} \
		const q = String.fromCharCode(34); \
		let cargo = fs.readFileSync('Cargo.toml', 'utf8'); \
		const cargoNext = cargo.replace(/^version = .*/m, 'version = ' + q + v + q); \
		if (cargoNext === cargo) { console.error('ERROR: workspace version line not found in Cargo.toml'); process.exit(1); } \
		fs.writeFileSync('Cargo.toml', cargoNext); \
		let version = fs.readFileSync('runtime/version.mjs', 'utf8'); \
		const versionNext = version.replace(/export const NUB_VERSION = .*/, 'export const NUB_VERSION = ' + q + v + q + ';'); \
		if (versionNext === version) { console.error('ERROR: NUB_VERSION not found in runtime/version.mjs'); process.exit(1); } \
		fs.writeFileSync('runtime/version.mjs', versionNext); \
		"
	@echo "✓ All packages, Cargo.toml, and runtime/version.mjs set to $(V)"

# Verify version consistency across npm packages, Cargo.toml, and version.mjs,
# AND that @oxc-project/runtime (the emit-helper runtime) is exact-pinned and
# matches the oxc version compiled into nub-native (Cargo.toml `oxc = "=X.Y.Z"`).
# The transpiler + parser are now native (crates/nub-native), so oxc-transform /
# oxc-parser are no longer npm deps; only the helper runtime is, and it must move
# in lockstep with the addon's oxc. Canonical source is npm/nub/package.json.
# Non-zero exit on any mismatch — the pre-release gate (release.yml runs it before
# building/publishing). Guards the transpile-cache invariant (A12): NUB_VERSION is
# the sole cache key, valid only because oxc cannot float without a version bump.
version-check:
	@node -e " \
		const fs = require('fs'); \
		const root = JSON.parse(fs.readFileSync('npm/nub/package.json', 'utf8')); \
		const v = root.version; \
		const errors = []; \
		for (const [dep, ver] of Object.entries(root.optionalDependencies || {})) { \
			if (ver !== v) errors.push(dep + ' optionalDependency pinned at ' + ver + ', expected ' + v); \
			const pkg = 'npm/' + dep.replace('@nubjs/', '') + '/package.json'; \
			try { \
				const p = JSON.parse(fs.readFileSync(pkg, 'utf8')); \
				if (p.version !== v) errors.push(pkg + ' has ' + p.version + ', expected ' + v); \
			} catch { errors.push('missing or unreadable ' + pkg); } \
		} \
		const cargo = fs.readFileSync('Cargo.toml', 'utf8'); \
		const cm = cargo.match(/^version = \x22([^\x22]*)\x22/m); \
		if (!cm) errors.push('Cargo.toml: workspace version line not found'); \
		else if (cm[1] !== v) errors.push('Cargo.toml has ' + cm[1] + ', expected ' + v); \
		const version = fs.readFileSync('runtime/version.mjs', 'utf8'); \
		const pm = version.match(/export const NUB_VERSION = \x22([^\x22]*)\x22/); \
		if (!pm) errors.push('runtime/version.mjs: NUB_VERSION not found'); \
		else if (pm[1] !== v) errors.push('runtime/version.mjs NUB_VERSION is ' + pm[1] + ', expected ' + v); \
		const dev = JSON.parse(fs.readFileSync('package.json', 'utf8')); \
		const deps = dev.dependencies || {}; \
		const rt = deps['@oxc-project/runtime']; \
		if (!rt) errors.push('package.json: @oxc-project/runtime missing from dependencies'); \
		else if (!/^[0-9]/.test(rt) || /[~^<>*]/.test(rt) || rt.includes(' ') || rt.includes('||')) errors.push('package.json: @oxc-project/runtime must be an EXACT version, not a range (got ' + rt + '): A12 transpile-cache-key proxy, must not float'); \
		const om = cargo.match(/^oxc = \\{ version = \x22=([^\x22]*)\x22/m); \
		if (!om) errors.push('Cargo.toml: oxc workspace dependency (=X.Y.Z pin) not found'); \
		else if (rt && rt !== om[1]) errors.push('package.json @oxc-project/runtime (' + rt + ') must match the oxc crate compiled into nub-native (Cargo.toml oxc =' + om[1] + ') — the emit helpers and the transformer are one oxc release'); \
		if (errors.length) { console.error('Version mismatch:\\n  ' + errors.join('\\n  ')); process.exit(1); } \
		else { console.log('✓ All npm packages, Cargo.toml, runtime/version.mjs at v' + v + '; @oxc-project/runtime matches nub-native oxc pin (' + (om ? om[1] : '?') + ')'); }"

npm-build: build
	./npm/build-local.sh

npm-publish:
	@echo "Publishing all @nubjs packages to npm (serially)..."
	@for pkg in nub-darwin-arm64 nub-darwin-x64 nub-linux-x64 nub-linux-x64-musl \
	            nub-linux-arm64 nub-linux-arm64-musl nub-win32-x64 nub-win32-arm64; do \
		echo "→ @nubjs/$$pkg"; \
		(cd npm/$$pkg && npm publish --access public) || exit 1; \
		echo ""; \
	done
	@echo "→ @nubjs/nub (root)"
	@(cd npm/nub && npm publish --access public)
	@echo ""
	@echo "✓ All packages published."

npm-publish-dry:
	@for pkg in nub-darwin-arm64 nub-darwin-x64 nub-linux-x64 nub-linux-x64-musl \
	            nub-linux-arm64 nub-linux-arm64-musl nub-win32-x64 nub-win32-arm64; do \
		echo "→ @nubjs/$$pkg"; \
		(cd npm/$$pkg && npm publish --access public --dry-run) || exit 1; \
		echo ""; \
	done
	@echo "→ @nubjs/nub (root)"
	@(cd npm/nub && npm publish --access public --dry-run)
