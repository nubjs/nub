CARGO   ?= cargo
PROFILE ?= release
BIN_DIR ?= /usr/local/bin
RUST_BUILD = $(CURDIR)/scripts/rust-build.sh
TARGET   = target/$(PROFILE)/nub

ifeq ($(PROFILE),release)
  CARGO_FLAGS = --release
else
  CARGO_FLAGS =
endif

.PHONY: build addon addon-fast install-dev uninstall-dev test verify test-node-matrix bench clean npm-build npm-publish npm-publish-dry

build: addon
	$(CARGO) build $(CARGO_FLAGS)

addon:
	@# nub-native is its OWN Cargo workspace (split out so the CLI can use
	@# `panic = "abort"` while the cdylib keeps `panic = "unwind"` — see the root
	@# Cargo.toml `exclude` comment). It is not reachable as `-p nub-native` from the
	@# main workspace, so build it from inside crates/nub-native. Its local
	@# .cargo/config.toml (honored because cwd is the crate) routes output into the
	@# shared root target/, so the copy paths below are unchanged; an explicit
	@# CARGO_TARGET_DIR env var still overrides it (CI cache / fast-iteration loop).
	cd crates/nub-native && $(CARGO) build $(CARGO_FLAGS)
	@mkdir -p runtime/addons
	@# rm before cp: overwriting the .node in place keeps the old inode, and on
	@# macOS the kernel's cached code-signing validation is keyed to that inode's
	@# original cs_mtime. A new dylib written to the same inode trips a cs_mtime
	@# mismatch -> tainted pages -> the loading process is SIGKILLed (exit 137).
	@# Removing first forces a fresh inode with a clean code-signing cache.
	@rm -f runtime/addons/nub-native.node
	@cp target/$(PROFILE)/libnub_native.dylib runtime/addons/nub-native.node 2>/dev/null || \
	 cp target/$(PROFILE)/libnub_native.so runtime/addons/nub-native.node 2>/dev/null || \
	 cp target/$(PROFILE)/nub_native.dll runtime/addons/nub-native.node 2>/dev/null || \
	 echo "Warning: could not find nub-native library"
	@echo "Built: runtime/addons/nub-native.node"

# Dev install uses the `fast` profile (no LTO, cgu=256), NOT release: nub-dev is
# the iteration binary, and the release profile's lto=thin + cgu=1 makes every
# rebuild re-LTO-codegen the whole binary (the ~300s nub-cli critical-path tail).
# The fast profile rebuilds it in a fraction of that. addon-fast builds the
# native addon under the same profile so a single `cargo build --profile fast`
# pass serves both.
install-dev: addon-fast
	$(CARGO) build --profile fast
	ln -sf $(CURDIR)/target/fast/nub $(BIN_DIR)/nub-dev
	ln -sf $(CURDIR)/target/fast/nub $(BIN_DIR)/nubx-dev
	@echo "Installed: $(BIN_DIR)/nub-dev -> target/fast/nub"
	@echo "Installed: $(BIN_DIR)/nubx-dev -> target/fast/nub"
	@echo ""
	@nub-dev --version

# Native addon built under the `fast` profile (mirrors `addon`, which is release).
# See `addon` for the rm-before-cp code-signing rationale.
addon-fast:
	cd crates/nub-native && $(CARGO) build --profile fast
	@mkdir -p runtime/addons
	@rm -f runtime/addons/nub-native.node
	@cp target/fast/libnub_native.dylib runtime/addons/nub-native.node 2>/dev/null || \
	 cp target/fast/libnub_native.so runtime/addons/nub-native.node 2>/dev/null || \
	 cp target/fast/nub_native.dll runtime/addons/nub-native.node 2>/dev/null || \
	 echo "Warning: could not find nub-native library"
	@echo "Built: runtime/addons/nub-native.node (fast profile)"

uninstall-dev:
	rm -f $(BIN_DIR)/nub-dev $(BIN_DIR)/nubx-dev
	@echo "Removed nub-dev and nubx-dev from $(BIN_DIR)"

test:
	$(CARGO) test

# Bounded host-local gate. Platform matrices, Docker jobs, and change-specific
# end-to-end tests remain separate parts of the pre-push verification loop.
verify:
	@test -f node_modules/@oxc-project/runtime/package.json || { \
		echo "make verify requires installed JS dependencies; run: pnpm install --frozen-lockfile" >&2; \
		exit 1; \
	}
	NUB_SHARED_TARGET="$(CURDIR)/target" "$(RUST_BUILD)" fmt --check
	(cd crates/nub-native && NUB_SHARED_TARGET="$(CURDIR)/target" "$(RUST_BUILD)" fmt --check)
	$(MAKE) --no-print-directory PROFILE=debug CARGO="env NUB_SHARED_TARGET='$(CURDIR)/target' '$(RUST_BUILD)'" addon
	@test -s runtime/addons/nub-native.node
	NUB_SHARED_TARGET="$(CURDIR)/target" "$(RUST_BUILD)" clippy --all-targets --all-features -- -D warnings
	(cd crates/nub-native && NUB_SHARED_TARGET="$(CURDIR)/target" "$(RUST_BUILD)" clippy --all-features -- -D warnings)
	tests/brand-lint/check-env-reads.sh
	NUB_SHARED_TARGET="$(CURDIR)/target" "$(RUST_BUILD)" test

# Run the integration suite across a Node version matrix (18.19 floor → 22.15
# fast-path floor) — the local mirror of ci.yml's `test` job. Locates or
# downloads each Node under ~/.cache/nub-test-node. See the script header.
test-node-matrix:
	@bash wiki/scripts/test-node-matrix.sh

# Warm-install benchmark table (nub vs bun/pnpm/npm). The script's staleness guard
# builds a current release binary if target/release/nub is missing or stale, so
# `make bench` is one command = build-if-needed + run. Pass args: make bench ARGS="--fixture t3 --runs 12"
bench:
	@bash tests/bench/install/run-4way.sh $(ARGS)

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
		const pkgs = ['npm/nub/package.json', 'npm/nub-types/package.json', \
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
		let nativeCargo = fs.readFileSync('crates/nub-native/Cargo.toml', 'utf8'); \
		const nativeCargoNext = nativeCargo.replace(/^version = .*/m, 'version = ' + q + v + q); \
		if (nativeCargoNext === nativeCargo) { console.error('ERROR: workspace version line not found in crates/nub-native/Cargo.toml'); process.exit(1); } \
		fs.writeFileSync('crates/nub-native/Cargo.toml', nativeCargoNext); \
		let version = fs.readFileSync('runtime/version.mjs', 'utf8'); \
		const versionNext = version.replace(/export const NUB_VERSION = .*/, 'export const NUB_VERSION = ' + q + v + q + ';'); \
		if (versionNext === version) { console.error('ERROR: NUB_VERSION not found in runtime/version.mjs'); process.exit(1); } \
		fs.writeFileSync('runtime/version.mjs', versionNext); \
		"
	@cargo update -p nub-cli -p nub-cache-key -p nub-core --precise $(V)
	@# nub-native is its own workspace (split for panic=abort vs unwind); its
	@# version + Cargo.lock entry live under crates/nub-native, updated separately.
	@cd crates/nub-native && cargo update -p nub-native --precise $(V)
	@echo "✓ All packages, Cargo.toml, both Cargo.lock files, and runtime/version.mjs set to $(V)"

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
		try { \
			const types = JSON.parse(fs.readFileSync('npm/nub-types/package.json', 'utf8')); \
			if (types.version !== v) errors.push('npm/nub-types/package.json has ' + types.version + ', expected ' + v); \
		} catch { errors.push('missing or unreadable npm/nub-types/package.json'); } \
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
