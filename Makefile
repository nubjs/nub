# Clamp make-driven cargo to utility QoS on darwin so fleet builds never starve
# interactive work — same contention control as scripts/rust-build.sh (which
# covers `make verify` already). NUB_BUILD_FG=1 opts out; no-op off macOS.
ifeq ($(NUB_BUILD_FG),1)
  QOS =
else
  QOS = $(shell command -v taskpolicy >/dev/null 2>&1 && echo taskpolicy -c utility)
endif
CARGO   ?= $(QOS) cargo
PROFILE ?= release
BIN_DIR ?= /usr/local/bin
RUST_BUILD = $(CURDIR)/scripts/rust-build.sh
TARGET   = target/$(PROFILE)/nub

# PROFILE names a cargo profile AND the target/<dir> the copy steps read from, so
# the two must agree. `debug` is the odd one out: cargo's profile is named `dev`
# but its output dir is `target/debug`, and it is the default, so it takes no
# flag. Any other named profile (`fast`) needs an explicit --profile or cargo
# would build into target/debug while the copy steps looked in target/<profile>.
ifeq ($(PROFILE),release)
  CARGO_FLAGS = --release
else ifeq ($(PROFILE),debug)
  CARGO_FLAGS =
else
  CARGO_FLAGS = --profile $(PROFILE)
endif

.PHONY: build addon addon-fast install-dev uninstall-dev qos-global test verify oxc-lockstep-check test-node-matrix bench clean version version-check npm-build npm-publish npm-publish-dry

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
# Builds through scripts/rust-build.sh and symlinks to the target dir IT picked.
# Hardcoding $(CURDIR)/target left nub-dev pointing at a stale binary: every
# documented rebuild path (the post-merge refresh, the dev-loop) goes through the
# wrapper, which normally writes to the SHARED dir, so nothing updated
# ./target/fast/nub and `nub-dev` silently served whatever was built last time
# make ran. `--print-target` re-resolves the same shared/isolated decision.
install-dev: addon-fast qos-global
	scripts/rust-build.sh build --profile fast
	@t=$$(scripts/rust-build.sh --print-target); \
	  ln -sf $$t/fast/nub $(BIN_DIR)/nub-dev; \
	  ln -sf $$t/fast/nub $(BIN_DIR)/nubx-dev; \
	  echo "Installed: $(BIN_DIR)/nub-dev -> $$t/fast/nub"; \
	  echo "Installed: $(BIN_DIR)/nubx-dev -> $$t/fast/nub"
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

# Machine-global rustc QoS clamp: every cargo invocation on this host — any
# worktree, clone, or direct `cargo` call — compiles at utility QoS, closing the
# gap the entry-point clamps above leave open. See scripts/rustc-qos.sh.
qos-global:
	@scripts/qos-global.sh

uninstall-dev:
	rm -f $(BIN_DIR)/nub-dev $(BIN_DIR)/nubx-dev
	@echo "Removed nub-dev and nubx-dev from $(BIN_DIR)"

test:
	$(CARGO) test

# oxc is pre-1.0, so a consumer left on an older minor compiles a SECOND full oxc
# stack rather than unifying. This asserts every consumer — the three Cargo
# manifests and the npm @oxc-project/runtime helper package — names one version,
# and that no Cargo.lock resolves two. Pure Node, no toolchain, runs in ~50ms;
# `make verify` and ci.yml's `oxc-lockstep` job both call it.
oxc-lockstep-check:
	@node scripts/check-oxc-lockstep.mjs

# Bounded host-local gate. Platform matrices, Docker jobs, and change-specific
# end-to-end tests remain separate parts of the pre-push verification loop.
verify:
	@test -f node_modules/@oxc-project/runtime/package.json || { \
		echo "make verify requires installed JS dependencies; run: pnpm install --frozen-lockfile" >&2; \
		exit 1; \
	}
	$(MAKE) --no-print-directory oxc-lockstep-check
	NUB_SHARED_TARGET="$(CURDIR)/target" "$(RUST_BUILD)" fmt --check
	(cd crates/nub-native && NUB_SHARED_TARGET="$(CURDIR)/target" "$(RUST_BUILD)" fmt --check)
	@# PROFILE=fast, matching both the dev loop and CI's check/clippy jobs, so the
	@# gates reuse the artifacts iteration already built instead of driving a
	@# second full dependency compile under `dev` (~26 GB of duplicated
	@# target/debug + target/fast, and one cold dependency build the first time a
	@# developer crossed from the dev loop into the gates). `fast` inherits `dev`,
	@# so debug-assertions, overflow checks and opt-level are identical — only
	@# debuginfo differs, and no lint reads it.
	$(MAKE) --no-print-directory PROFILE=fast CARGO="env NUB_SHARED_TARGET='$(CURDIR)/target' '$(RUST_BUILD)'" addon
	@test -s runtime/addons/nub-native.node
	NUB_SHARED_TARGET="$(CURDIR)/target" "$(RUST_BUILD)" clippy --all-targets --all-features --profile fast -- -D warnings
	(cd crates/nub-native && NUB_SHARED_TARGET="$(CURDIR)/target" "$(RUST_BUILD)" clippy --all-features --profile fast -- -D warnings)
	tests/brand-lint/check-env-reads.sh
	tests/brand-lint/check-path-literals.sh
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
# value would serve stale cached output after an upgrade. The file-stamping body
# lives in scripts/set-version.mjs so release.yml's canary stamp (which can't
# rely on `make` on the Windows runners) shares it; this target adds the
# Cargo.lock refresh a committed bump wants.
version:
	@test -n "$(V)" || (echo "Usage: make version V=0.0.3" && exit 1)
	@echo "Setting version to $(V) across all packages, Cargo.toml, and preload.mjs..."
	@node scripts/set-version.mjs "$(V)"
	@cargo update -p nub-cli -p nub-cache-key -p nub-core --precise $(V)
	@# nub-native is its own workspace (split for panic=abort vs unwind); its
	@# version + Cargo.lock entry live under crates/nub-native, updated separately.
	@cd crates/nub-native && cargo update -p nub-native --precise $(V)
	@echo "✓ All packages, Cargo.toml, both Cargo.lock files, and runtime/version.mjs set to $(V)"

# Verify version consistency across npm packages, Cargo.toml, version.mjs, and
# the version-pinned nub.jsonc schema snapshot under site/public/schema (which
# must equal latest.json modulo the schema-id field, so a published vX.Y.json is
# never stale relative to the schema the release actually ships).
# Canonical source is npm/nub/package.json. Non-zero exit on any mismatch — the
# pre-release gate (release.yml runs it before building/publishing). Guards the
# transpile-cache invariant (A12): NUB_VERSION is the sole cache key, valid only
# because oxc cannot float without a version bump — which is why this also runs
# the oxc lockstep check (the single implementation of that assertion).
version-check: oxc-lockstep-check
	@node -e " \
		const fs = require('fs'); \
		const { isDeepStrictEqual } = require('node:util'); \
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
		for (const f of ['crates/nub-core/Cargo.toml', 'crates/nub-native/Cargo.toml']) { \
			try { \
				const t = fs.readFileSync(f, 'utf8'); \
				const im = t.match(/^version = \x22([^\x22]*)\x22/m); \
				if (!im) errors.push(f + ': inlined version line not found'); \
				else if (im[1] !== v) errors.push(f + ' has ' + im[1] + ', expected ' + v); \
			} catch { errors.push('missing or unreadable ' + f); } \
		} \
		const version = fs.readFileSync('runtime/version.mjs', 'utf8'); \
		const pm = version.match(/export const NUB_VERSION = \x22([^\x22]*)\x22/); \
		if (!pm) errors.push('runtime/version.mjs: NUB_VERSION not found'); \
		else if (pm[1] !== v) errors.push('runtime/version.mjs NUB_VERSION is ' + pm[1] + ', expected ' + v); \
		const pinned = 'v' + v.split('.').slice(0, 2).join('.') + '.json'; \
		try { \
			const latest = JSON.parse(fs.readFileSync('site/public/schema/latest.json', 'utf8')); \
			const snapshotPath = 'site/public/schema/' + pinned; \
			const snapshot = JSON.parse(fs.readFileSync(snapshotPath, 'utf8')); \
			const expectedId = 'https://nubjs.com/schema/' + pinned; \
			if (snapshot.\$$id !== expectedId) errors.push(snapshotPath + ' has \$$id ' + JSON.stringify(snapshot.\$$id) + ', expected ' + JSON.stringify(expectedId)); \
			delete latest.\$$id; \
			delete snapshot.\$$id; \
			if (!isDeepStrictEqual(snapshot, latest)) errors.push(snapshotPath + ' does not equal latest.json modulo \$$id'); \
		} catch { errors.push('missing or unreadable schema snapshot for ' + pinned); } \
		if (errors.length) { console.error('Version mismatch:\\n  ' + errors.join('\\n  ')); process.exit(1); } \
		else { console.log('✓ All npm packages, Cargo.toml (incl. the inlined nub-core/nub-native manifests), runtime/version.mjs, and the schema snapshot at v' + v); }"

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
