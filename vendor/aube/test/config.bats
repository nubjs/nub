#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "config set writes aube-owned keys to user config.toml" {
	run aube config set autoInstallPeers false
	assert_success
	assert [ -f "$XDG_CONFIG_HOME/aube/config.toml" ]
	run cat "$XDG_CONFIG_HOME/aube/config.toml"
	assert_output --partial "autoInstallPeers = false"
	run cat "$HOME/.npmrc"
	refute_output --partial "autoInstallPeers"
}

@test "config get reads value from user .npmrc" {
	echo "autoInstallPeers=false" >"$HOME/.npmrc"
	run aube config get autoInstallPeers
	assert_success
	assert_output "false"
}

@test "config get resolves canonical name to .npmrc alias" {
	# Value written under the kebab-case alias; canonical lookup should
	# still find it because settings.toml declares both names.
	echo "auto-install-peers=true" >"$HOME/.npmrc"
	run aube config get autoInstallPeers
	assert_success
	assert_output "true"
}

@test "config get and list prefer user config.toml over user .npmrc" {
	# Aube's own user config wins over ~/.npmrc so values aube wrote
	# via `aube config set` are authoritative — they are not silently
	# shadowed by leftover entries in a shared .npmrc that other tools
	# (npm, pnpm, yarn) also read.
	mkdir -p "$XDG_CONFIG_HOME/aube"
	echo "autoInstallPeers = true" >"$XDG_CONFIG_HOME/aube/config.toml"
	echo "autoInstallPeers=false" >"$HOME/.npmrc"
	run aube config get autoInstallPeers
	assert_success
	assert_output "true"
	run aube config list --location user
	assert_success
	assert_line "auto-install-peers=true"
	refute_line "auto-install-peers=false"
}

@test "config get --location project only reads project .npmrc" {
	mkdir proj
	echo "autoInstallPeers=true" >"$HOME/.npmrc"
	echo "autoInstallPeers=false" >proj/.npmrc
	cd proj
	run aube config get autoInstallPeers --location project
	assert_success
	assert_output "false"
}

@test "config get --location user ignores project .npmrc" {
	mkdir proj
	echo "autoInstallPeers=false" >proj/.npmrc
	cd proj
	run aube config get autoInstallPeers --location user
	assert_success
	assert_output "undefined"
}

@test "config list collapses cross-alias duplicates to match get" {
	# User file writes the setting under the camelCase canonical name;
	# project file writes it under the kebab-case alias. `get` resolves
	# aliases and returns the project value; `list` must agree and show
	# exactly one row under the primary alias with that same value —
	# otherwise `list` and `get` could disagree on identical input.
	mkdir proj
	echo "autoInstallPeers=true" >"$HOME/.npmrc"
	echo "auto-install-peers=false" >proj/.npmrc
	cd proj
	run aube config get autoInstallPeers
	assert_success
	assert_output "false"
	run aube config list
	assert_success
	assert_line "auto-install-peers=false"
	refute_line "autoInstallPeers=true"
	refute_line "auto-install-peers=true"
}

@test "config list --all rejects non-merged location" {
	run aube config list --all --location project
	assert_failure
	assert_output --partial "--all is only supported with --location merged"
}

@test "config get prints undefined for missing key" {
	run aube config get autoInstallPeers
	assert_success
	assert_output "undefined"
}

@test "config set for an aube-owned key leaves user .npmrc untouched" {
	# Discussion #601: `aube config set <known-key>` writes to
	# `config.toml` and must not edit `~/.npmrc`, which is shared with
	# npm/pnpm/yarn. The new value still takes effect because
	# `config.toml` outranks `~/.npmrc` in the resolver.
	echo "auto-install-peers=false" >"$HOME/.npmrc"
	run aube config set autoInstallPeers true
	assert_success
	run cat "$HOME/.npmrc"
	assert_output "auto-install-peers=false"
	run cat "$XDG_CONFIG_HOME/aube/config.toml"
	assert_output --partial "autoInstallPeers = true"
	run aube config get autoInstallPeers
	assert_success
	assert_output "true"
}

@test "config delete removes a key" {
	mkdir -p "$XDG_CONFIG_HOME/aube"
	echo "autoInstallPeers = false" >"$XDG_CONFIG_HOME/aube/config.toml"
	run aube config delete autoInstallPeers
	assert_success
	run cat "$XDG_CONFIG_HOME/aube/config.toml"
	refute_output --partial "autoInstallPeers"
}

@test "config delete errors when key is not set" {
	echo "registry=https://r.example.com/" >"$HOME/.npmrc"
	run aube config delete autoInstallPeers
	assert_failure
}

@test "config delete points at .npmrc when an aube-only key lives only there" {
	# `autoInstallPeers` is an aube-only setting (not npm-shared), so
	# aube doesn't touch `.npmrc` for it — the file is shared with
	# npm/pnpm/yarn. When the value only lives in `.npmrc`, delete
	# surfaces the location so the user knows what to clean up.
	echo "autoInstallPeers=false" >"$HOME/.npmrc"
	run aube config delete autoInstallPeers
	assert_failure
	assert_output --partial ".npmrc"
	assert_output --partial "an entry exists in"
	# Confirm the .npmrc line is preserved.
	run cat "$HOME/.npmrc"
	assert_output --partial "autoInstallPeers=false"
}

@test "config list prints merged entries" {
	# Project dir must be separate from HOME so user vs project .npmrc
	# don't alias to the same file.
	mkdir proj
	echo "registry=https://user.example.com/" >"$HOME/.npmrc"
	echo "autoInstallPeers=false" >proj/.npmrc
	cd proj
	run aube config list
	assert_success
	assert_output --partial "registry=https://user.example.com/"
	# `autoInstallPeers` canonicalizes to `auto-install-peers` in list
	# output so cross-alias duplicates collapse into one row.
	assert_output --partial "auto-install-peers=false"
}

@test "config with no subcommand lists merged entries" {
	mkdir proj
	echo "registry=https://user.example.com/" >"$HOME/.npmrc"
	echo "autoInstallPeers=false" >proj/.npmrc
	cd proj
	run aube config
	assert_success
	assert_output --partial "registry=https://user.example.com/"
	assert_output --partial "auto-install-peers=false"
}

@test "config with parent --all lists defaults" {
	run aube config --all
	assert_success
	assert_output --partial "auto-install-peers=true (default)"
}

@test "config list honors parent list flags" {
	run aube config --all list
	assert_success
	assert_output --partial "auto-install-peers=true (default)"
}

@test "config rejects parent list flags with non-list subcommands" {
	run aube config --all set registry https://registry.example.com/
	assert_failure
	assert_output --partial "list flags must be used with"
}

@test "config rejects parent list flags with tui subcommand" {
	run aube config --json tui
	assert_failure
	assert_output --partial "list flags must be used with"
}

@test "config list subcommand location overrides parent location" {
	echo "registry=https://user.example.com/" >"$HOME/.npmrc"
	mkdir proj
	echo "registry=https://project.example.com/" >proj/.npmrc
	cd proj
	run aube config --location project list --location user
	assert_success
	assert_output --partial "registry=https://user.example.com/"
	refute_output --partial "project.example.com"
}

@test "config list subcommand location overrides parent local shortcut" {
	echo "registry=https://user.example.com/" >"$HOME/.npmrc"
	mkdir proj
	echo "registry=https://project.example.com/" >proj/.npmrc
	cd proj
	run aube config --local list --location user
	assert_success
	assert_output --partial "registry=https://user.example.com/"
	refute_output --partial "project.example.com"
}

@test "config list --location project only reads project .npmrc" {
	mkdir proj
	echo "registry=https://user.example.com/" >"$HOME/.npmrc"
	echo "autoInstallPeers=false" >proj/.npmrc
	cd proj
	run aube config list --location project
	assert_success
	refute_output --partial "user.example.com"
	assert_output --partial "auto-install-peers=false"
}

@test "config set --location project writes aube-owned keys to project config.toml" {
	# Project-scope aube settings land in <cwd>/.config/aube/config.toml,
	# the same XDG layout used at user-scope. The project `.npmrc` is
	# left alone so it remains a shared file with npm/pnpm/yarn.
	run aube config set autoInstallPeers false --location project
	assert_success
	assert [ -f ".config/aube/config.toml" ]
	run cat ".config/aube/config.toml"
	assert_output --partial "autoInstallPeers = false"
	# If a project `.npmrc` exists (e.g. for the test registry pin), it
	# must not contain the aube-owned key.
	if [ -f "./.npmrc" ]; then
		run cat "./.npmrc"
		refute_output --partial "autoInstallPeers"
	fi
}

@test "config set --location project writes unknown keys to ./.npmrc" {
	# Registry/auth-style keys aren't aube-owned settings and continue
	# to land in project `.npmrc`.
	run aube config set "//registry.example.com/:_authToken" secret --location project
	assert_success
	assert [ -f "./.npmrc" ]
	run cat "./.npmrc"
	assert_output --partial "//registry.example.com/:_authToken=secret"
}

@test "config get prefers project config.toml over project .npmrc" {
	# Locality: project beats user; within project, config.toml beats
	# `.npmrc` for the same reason it does at user-scope.
	mkdir proj
	echo "autoInstallPeers=false" >proj/.npmrc
	mkdir -p "proj/.config/aube"
	echo "autoInstallPeers = true" >"proj/.config/aube/config.toml"
	cd proj
	run aube config get autoInstallPeers
	assert_success
	assert_output "true"
}

@test "config set --location project writes to existing workspace yaml when one is present" {
	# When a pnpm-workspace.yaml (or aube-workspace.yaml) already
	# lives in the project, project-scope aube settings land there
	# instead of creating a new `.config/aube/config.toml`. Keeps the
	# project's config story to a single file when possible.
	echo "packages:" >pnpm-workspace.yaml
	echo "  - 'apps/*'" >>pnpm-workspace.yaml
	run aube config set autoInstallPeers false --location project
	assert_success
	run cat pnpm-workspace.yaml
	assert_output --partial "autoInstallPeers: false"
	# Existing entries are preserved.
	assert_output --partial "packages:"
	# No new config.toml created.
	assert [ ! -f ".config/aube/config.toml" ]
	# Round-trip through `aube config get`.
	run aube config get autoInstallPeers
	assert_success
	assert_output "false"
}

@test "config set --location project falls back to config.toml for settings without workspace yaml support" {
	# `scriptShell` is not a workspace-yaml source per settings.toml,
	# so the project write lands in `<cwd>/.config/aube/config.toml`
	# even though a workspace yaml exists.
	echo "packages:" >pnpm-workspace.yaml
	echo "  - 'apps/*'" >>pnpm-workspace.yaml
	run aube config set scriptShell /bin/zsh --location project
	assert_success
	assert [ -f ".config/aube/config.toml" ]
	run cat ".config/aube/config.toml"
	assert_output --partial 'scriptShell = "/bin/zsh"'
	run cat pnpm-workspace.yaml
	refute_output --partial "scriptShell"
}

@test "config set --location project to workspace yaml beats user-scope settings" {
	# Project-scope writes routed to workspace yaml must not be
	# silently shadowed by anything in ~/.npmrc or
	# ~/.config/aube/config.toml. Scope locality: project beats user,
	# and `pnpm-workspace.yaml` is project-scope.
	#
	# `proj/` is separate from $HOME so user-scope and project-scope
	# config files don't collide.
	mkdir proj
	echo "autoInstallPeers=true" >"$HOME/.npmrc"
	mkdir -p "$XDG_CONFIG_HOME/aube"
	echo "autoInstallPeers = true" >"$XDG_CONFIG_HOME/aube/config.toml"
	echo "packages:" >proj/pnpm-workspace.yaml
	cd proj
	run aube config set autoInstallPeers false --location project
	assert_success
	run cat pnpm-workspace.yaml
	assert_output --partial "autoInstallPeers: false"
	# Round-trip: get returns the project value, not user defaults.
	run aube config get autoInstallPeers
	assert_success
	assert_output "false"
}

@test "config set --location project stays in config.toml once it exists" {
	# If a project already adopted `.config/aube/config.toml`, later
	# `set` calls keep landing there even after a `pnpm-workspace.yaml`
	# is added. Writing to yaml would be silently shadowed by the
	# higher-precedence config.toml entry on read.
	run aube config set autoInstallPeers false --location project
	assert_success
	assert [ -f ".config/aube/config.toml" ]
	echo "packages:" >pnpm-workspace.yaml
	run aube config set autoInstallPeers true --location project
	assert_success
	run cat ".config/aube/config.toml"
	assert_output --partial "autoInstallPeers = true"
	run cat pnpm-workspace.yaml
	refute_output --partial "autoInstallPeers"
	# Effective value matches the latest set.
	run aube config get autoInstallPeers
	assert_success
	assert_output "true"
}

@test "config delete --location project sweeps both workspace yaml and config.toml" {
	# Regression for the silent-resurrection bug: a setting can end up
	# in both files (e.g. set into config.toml first, into yaml later
	# via a manual edit). Delete must clear both — otherwise the
	# config.toml copy silently reactivates after the yaml removal.
	mkdir -p ".config/aube"
	echo "autoInstallPeers = true" >".config/aube/config.toml"
	cat >pnpm-workspace.yaml <<EOF
packages:
  - 'apps/*'
autoInstallPeers: false
EOF
	run aube config delete autoInstallPeers --location project
	assert_success
	run cat ".config/aube/config.toml"
	refute_output --partial "autoInstallPeers"
	run cat pnpm-workspace.yaml
	refute_output --partial "autoInstallPeers"
}

@test "config delete --location project removes the key from workspace yaml" {
	# Symmetric with set: delete removes from the workspace yaml
	# when the value lives there.
	cat >pnpm-workspace.yaml <<EOF
packages:
  - 'apps/*'
autoInstallPeers: false
EOF
	run aube config delete autoInstallPeers --location project
	assert_success
	run cat pnpm-workspace.yaml
	refute_output --partial "autoInstallPeers"
	# Unrelated entries are preserved.
	assert_output --partial "packages:"
}

@test "config get prefers project npmrc over user config.toml" {
	# Scope locality: project `.npmrc` outranks user `config.toml`.
	mkdir proj
	mkdir -p "$XDG_CONFIG_HOME/aube"
	echo "autoInstallPeers = true" >"$XDG_CONFIG_HOME/aube/config.toml"
	echo "autoInstallPeers=false" >proj/.npmrc
	cd proj
	run aube config get autoInstallPeers
	assert_success
	assert_output "false"
}

@test "config preserves existing unrelated entries when setting a key" {
	echo "registry=https://r.example.com/" >"$HOME/.npmrc"
	run aube config set autoInstallPeers false
	assert_success
	run cat "$HOME/.npmrc"
	assert_output --partial "registry=https://r.example.com/"
	refute_output --partial "autoInstallPeers"
	run cat "$XDG_CONFIG_HOME/aube/config.toml"
	assert_output --partial "autoInstallPeers = false"
}

@test "config get returns literal \${VAR} references, not substituted values" {
	# Users inspecting their .npmrc should see exactly what's on disk.
	# Resolving ${NPM_TOKEN} here would both surprise users and risk
	# leaking secrets into shell history or logs. The single quotes
	# below are intentional: we want the literal `${...}` text written
	# to the file, not the expansion. Uses a non-protected key so the
	# value is actually printed (protected keys error — see below).
	export AUBE_TEST_PROXY=http://proxy.example.com
	# shellcheck disable=SC2016
	echo 'https-proxy=${AUBE_TEST_PROXY}' >"$HOME/.npmrc"
	run aube config get https-proxy
	assert_success
	# shellcheck disable=SC2016
	assert_output '${AUBE_TEST_PROXY}'
	# Same answer via --location user.
	run aube config get https-proxy --location user
	assert_success
	# shellcheck disable=SC2016
	assert_output '${AUBE_TEST_PROXY}'
	unset AUBE_TEST_PROXY
}

@test "config get refuses to print protected auth keys, matching npm" {
	# Security/npm parity: `config get` of a registry auth token must
	# error like `npm config get`, never echo the secret. npm:
	# "The <key> option is protected, and cannot be retrieved in this way".
	echo '//registry.example.com/:_authToken=npm_supersecrettoken' >"$HOME/.npmrc"
	run aube config get "//registry.example.com/:_authToken"
	assert_failure
	# miette wraps the diagnostic across lines with box-drawing prefixes,
	# so match a fragment that stays on one rendered line.
	assert_output --partial "option is protected"
	refute_output --partial "npm_supersecrettoken"
	# A non-auth key in the same file still reads back fine.
	echo 'registry=https://registry.example.com/' >>"$HOME/.npmrc"
	run aube config get registry
	assert_success
	assert_output "https://registry.example.com/"
}

@test "config list renders protected auth keys as (protected), matching npm" {
	echo '//registry.example.com/:_authToken=npm_supersecrettoken' >"$HOME/.npmrc"
	echo 'registry=https://registry.example.com/' >>"$HOME/.npmrc"
	run aube config list
	assert_success
	assert_output --partial "//registry.example.com/:_authToken=(protected)"
	refute_output --partial "npm_supersecrettoken"
}

@test "config set routes unknown keys to user config.toml, not .npmrc" {
	# Discussion #617 follow-up: aube's `.npmrc` writes are scoped to the
	# npm-shared surface (auth, registries, npm-standard scalars). Any
	# other key — known aube setting or genuinely unknown — lands in
	# aube's own config.toml so it doesn't pollute the file npm/yarn/pnpm
	# also read.
	run aube config set some-experimental-flag value
	assert_success
	assert [ -f "$XDG_CONFIG_HOME/aube/config.toml" ]
	run cat "$XDG_CONFIG_HOME/aube/config.toml"
	assert_output --partial 'some-experimental-flag = "value"'
	if [ -e "$HOME/.npmrc" ]; then
		run cat "$HOME/.npmrc"
		refute_output --partial "some-experimental-flag"
	fi
}

@test "config get reads free-form unknown keys back from config.toml" {
	# Round-trip: an unknown key written via `config set` must be
	# readable via `config get` without the user having to remember
	# which file it ended up in.
	run aube config set some-experimental-flag value
	assert_success
	run aube config get some-experimental-flag
	assert_success
	assert_output "value"
}

@test "config delete removes a free-form unknown key from config.toml" {
	run aube config set some-experimental-flag value
	assert_success
	run aube config delete some-experimental-flag
	assert_success
	run aube config get some-experimental-flag
	assert_success
	assert_output "undefined"
}

@test "config set routes pnpm-only knobs (dangerouslyAllowAllBuilds) to config.toml" {
	# `dangerouslyAllowAllBuilds` is a pnpm/aube-only knob. npm warns
	# about it in `.npmrc`. With the inverted routing it lands in
	# aube's own config alongside other aube-known settings.
	run aube config set dangerouslyAllowAllBuilds true
	assert_success
	assert [ -f "$XDG_CONFIG_HOME/aube/config.toml" ]
	run cat "$XDG_CONFIG_HOME/aube/config.toml"
	assert_output --partial "dangerouslyAllowAllBuilds = true"
	if [ -e "$HOME/.npmrc" ]; then
		run cat "$HOME/.npmrc"
		refute_output --partial "dangerouslyAllowAllBuilds"
	fi
}

@test "config set rejects bare aube map settings" {
	# Object-typed aube settings (`allowBuilds`, `overrides`,
	# `packageExtensions`, …) can't be serialized as a single scalar
	# via `config set`. The error must point at the right edit site.
	run aube config set allowBuilds 'maybe'
	assert_failure
	assert_output --partial "allowBuilds"
	assert_output --partial "map setting"
}

@test "config set keeps npm-shared keys in .npmrc" {
	# `registry`, scoped registries, and per-host auth/cert tokens are
	# part of the multi-tool npm contract and must keep landing in
	# `.npmrc` so npm/pnpm/yarn read the same values.
	run aube config set registry https://r.example.com/
	assert_success
	run aube config set @mycorp:registry https://npm.mycorp.internal/
	assert_success
	run aube config set "//r.example.com/:_authToken" secret
	assert_success
	run cat "$HOME/.npmrc"
	assert_output --partial "registry=https://r.example.com/"
	assert_output --partial "@mycorp:registry=https://npm.mycorp.internal/"
	assert_output --partial "//r.example.com/:_authToken=secret"
	# config.toml should not contain registry/auth keys
	if [ -e "$XDG_CONFIG_HOME/aube/config.toml" ]; then
		run cat "$XDG_CONFIG_HOME/aube/config.toml"
		refute_output --partial "registry"
		refute_output --partial "_authToken"
	fi
}

@test "config delete sweeps .npmrc for npm-shared aube settings" {
	# Settings like `engineStrict` are both npm-shared (so `set`
	# routes them to .npmrc) and known aube settings in
	# settings.toml. Delete must follow the same routing — otherwise
	# the value sits stuck in .npmrc after `set` and `delete` fails
	# with a misleading "stale entry" error.
	run aube config set engineStrict false
	assert_success
	run cat "$HOME/.npmrc"
	assert_output --partial "engineStrict=false"
	# Delete must succeed and actually remove the .npmrc line.
	run aube config delete engineStrict
	assert_success
	if [ -e "$HOME/.npmrc" ]; then
		run cat "$HOME/.npmrc"
		refute_output --partial "engineStrict"
		refute_output --partial "engine-strict"
	fi
	run aube config get engineStrict
	assert_success
	assert_output "undefined"
}

@test "config set on an npm-shared aube setting sweeps stale config.toml" {
	# Settings like `engineStrict` are in both the npm-shared allowlist
	# (so writes land in `.npmrc` for cross-tool visibility) and
	# settings.toml (so older aube versions may have written them to
	# `config.toml` instead). The user-aube-config source outranks
	# user `.npmrc` in the resolver, so a stale `config.toml` entry
	# would silently shadow the new `.npmrc` value if we didn't sweep
	# it on each write.
	mkdir -p "$XDG_CONFIG_HOME/aube"
	echo "engineStrict = true" >"$XDG_CONFIG_HOME/aube/config.toml"
	run aube config set engineStrict false
	assert_success
	# .npmrc has the new value (preferred_write_key preserves the
	# spelling the user typed when it's one of the known aliases).
	run cat "$HOME/.npmrc"
	assert_output --partial "engineStrict=false"
	# config.toml stale entry must be gone
	run cat "$XDG_CONFIG_HOME/aube/config.toml"
	refute_output --partial "engineStrict"
	refute_output --partial "engine-strict"
	# Round-trip: get returns the new value, not the stale one.
	run aube config get engineStrict
	assert_success
	assert_output "false"
}

@test "config set --local allowBuilds.<pkg> writes to project workspace yaml" {
	# Discussion #617: `aube config set allowBuilds.<pkg> true` should
	# be a valid input — at project scope it edits the same
	# `allowBuilds:` map `aube approve-builds` mutates. With no
	# workspace yaml present, the write lands in `package.json#aube.allowBuilds`
	# via `aube-manifest::edit_setting_map`.
	echo '{"name":"demo","version":"0.0.1"}' >package.json
	run aube config set --local "allowBuilds.@mongodb-js/zstd" true
	assert_success
	run cat package.json
	assert_output --partial '"allowBuilds"'
	assert_output --partial '"@mongodb-js/zstd": true'
	# .npmrc must stay clean.
	if [ -e ".npmrc" ]; then
		run cat .npmrc
		refute_output --partial "allowBuilds"
	fi
}

@test "config set --local allowBuilds.<pkg> appends to existing workspace yaml" {
	# When a workspace yaml already exists, the dotted write extends
	# its `allowBuilds:` map instead of touching `package.json`.
	cat >pnpm-workspace.yaml <<-YAML
		packages:
		  - 'apps/*'
		allowBuilds:
		  sharp: true
	YAML
	run aube config set --local "allowBuilds.@mongodb-js/zstd" true
	assert_success
	run cat pnpm-workspace.yaml
	assert_output --partial "sharp: true"
	assert_output --partial "'@mongodb-js/zstd': true"
}

@test "config set --local overrides.<pkg> writes pure-digit versions as strings" {
	# `overrides.express 4` is a valid version spec ("any 4.x"). It
	# must serialize as a YAML *string*, not a YAML number — pnpm's
	# (and aube's) typed-`String` deserializer rejects integer values
	# without a custom visitor.
	cat >pnpm-workspace.yaml <<-YAML
		packages:
		  - 'apps/*'
	YAML
	run aube config set --local overrides.express 4
	assert_success
	run cat pnpm-workspace.yaml
	assert_output --partial "overrides:"
	assert_output --partial "express: '4'"
	refute_output --partial "express: 4
"
}

@test "config set --local overrides.<pkg> writes to project workspace yaml" {
	# Generic map-setting branch: dotted writes for any aube
	# object-typed setting (`overrides`, `packageExtensions`, …) follow
	# the same path as `allowBuilds`, without the approve-builds hint.
	cat >pnpm-workspace.yaml <<-YAML
		packages:
		  - 'apps/*'
	YAML
	run aube config set --local overrides.lodash 4.17.21
	assert_success
	run cat pnpm-workspace.yaml
	assert_output --partial "overrides:"
	assert_output --partial "lodash: 4.17.21"
}

@test "config set allowBuilds.<pkg> at user scope errors with --local hint" {
	# User-scope errors because aube only reads `allowBuilds` from the
	# project's workspace yaml / `package.json` today. The hint points
	# at `--local` rather than dropping the value where nothing reads
	# it.
	run aube config set "allowBuilds.@mongodb-js/zstd" true
	assert_failure
	assert_output --partial "allowBuilds"
	assert_output --partial "--local"
	# .npmrc must stay clean.
	if [ -e "$HOME/.npmrc" ]; then
		run cat "$HOME/.npmrc"
		refute_output --partial "allowBuilds"
	fi
}

@test "config set overrides.<pkg> at user scope errors with --local hint" {
	# Same user-scope rejection as `allowBuilds` — generic map-setting
	# branch, no per-setting special case.
	run aube config set overrides.lodash 4.17.21
	assert_failure
	assert_output --partial "overrides"
	assert_output --partial "--local"
}

@test "config delete --local allowBuilds.<pkg> round-trips with set" {
	# Symmetric to `config set --local allowBuilds.<pkg>`: the set
	# path writes to workspace yaml / `package.json#aube.allowBuilds`,
	# so delete must sweep the same place. Without the dotted-delete
	# path, the entry sits stuck after set and the CLI can't remove it.
	echo '{"name":"demo","version":"0.0.1"}' >package.json
	run aube config set --local "allowBuilds.@mongodb-js/zstd" true
	assert_success
	run aube config delete --local "allowBuilds.@mongodb-js/zstd"
	assert_success
	# allowBuilds map should be gone (empty submap is scrubbed).
	run cat package.json
	refute_output --partial "@mongodb-js/zstd"
}

@test "config delete --local allowBuilds.<pkg> sweeps existing workspace yaml" {
	cat >pnpm-workspace.yaml <<-YAML
		packages:
		  - 'apps/*'
		allowBuilds:
		  sharp: true
		  '@mongodb-js/zstd': true
	YAML
	run aube config delete --local "allowBuilds.@mongodb-js/zstd"
	assert_success
	run cat pnpm-workspace.yaml
	# the other allowBuilds entry must survive
	assert_output --partial "sharp: true"
	# our target entry must be gone
	refute_output --partial "@mongodb-js/zstd"
}

@test "config delete allowBuilds.<pkg> at user scope errors with --local hint" {
	run aube config delete "allowBuilds.@mongodb-js/zstd"
	assert_failure
	assert_output --partial "allowBuilds"
	assert_output --partial "--local"
}

@test "config delete --local overrides.<pkg> on missing entry errors cleanly" {
	cat >pnpm-workspace.yaml <<-YAML
		packages:
		  - 'apps/*'
	YAML
	run aube config delete --local overrides.lodash
	assert_failure
	assert_output --partial "not set"
}

@test "config set autoInstallPeers.foo errors: scalar settings have no nested namespace" {
	# Dotted writes against a *scalar* aube setting are still a
	# syntactic error — there's no nested namespace to write into.
	run aube config set autoInstallPeers.foo true
	assert_failure
	assert_output --partial "autoInstallPeers"
	assert_output --partial "scalar"
}

@test "config accepts unknown (literal) keys for auth-style writes" {
	# Auth token keys like `//registry/:_authToken` are not registered
	# in settings.toml. The command should still write them verbatim.
	run aube config set "//registry.example.com/:_authToken" secret123
	assert_success
	run cat "$HOME/.npmrc"
	assert_output --partial "//registry.example.com/:_authToken=secret123"
}

@test "config set @scope:registry does not clobber the user's registry entry" {
	# `registries.npmrc_keys` documents `@scope:registry` and
	# `//host/:_authToken` as pattern templates alongside the literal
	# `registry` key. The alias resolver must NOT treat those templates
	# as siblings of `registry`, otherwise `config set @scope:registry …`
	# would resolve to the registries group and the stale-alias removal
	# pass would silently delete the user's existing `registry` line.
	run aube config set registry https://registry.example.com/
	assert_success
	run aube config set @mycorp:registry https://npm.mycorp.internal/
	assert_success
	run aube config get registry
	assert_success
	assert_output "https://registry.example.com/"
	run aube config get @mycorp:registry
	assert_success
	assert_output "https://npm.mycorp.internal/"
}

@test "config get --json emits the value as a JSON string" {
	run aube config set registry https://registry.example.com/
	assert_success
	run aube config get --json registry
	assert_success
	assert_output '"https://registry.example.com/"'
}

@test "config get --json prints undefined for a missing key" {
	run aube config get --json nonexistent-key
	assert_success
	assert_output "undefined"
}

@test "config list --json emits a JSON object" {
	run aube config set registry https://registry.example.com/
	assert_success
	run aube config set auto-install-peers true
	assert_success
	run bash -c "aube config list --json | jq -r '.registry'"
	assert_success
	assert_output "https://registry.example.com/"
	run bash -c 'aube config list --json | jq -r ".[\"auto-install-peers\"]"'
	assert_success
	assert_output "true"
}

@test "config list --all --json marks default values" {
	# Nothing is set — every row in the output is a default, and the JSON
	# value should preserve the default-vs-explicit distinction.
	run bash -c 'aube config list --all --json | jq -r ".[\"auto-install-peers\"].value"'
	assert_success
	assert_output "true"
	run bash -c 'aube config list --all --json | jq -r ".[\"auto-install-peers\"].default"'
	assert_success
	assert_output "true"

	# The parallel text view should still annotate defaults, so the two
	# outputs stay distinguishable for humans vs. machines.
	run aube config list --all
	assert_success
	assert_output --partial "(default)"
}

@test "config find searches the generated settings reference" {
	run aube config find min package install time
	assert_success
	assert_line --partial "minimumReleaseAge (minimumReleaseAge) - Delay installation of newly published versions (minutes)."
}

@test "config explain prints sources for a known setting" {
	run aube config explain minimum-release-age
	assert_success
	assert_line "minimumReleaseAge"
	assert_line "  Default: 1440"
	assert_line "  Environment: npm_config_minimum_release_age, NPM_CONFIG_MINIMUM_RELEASE_AGE, AUBE_MINIMUM_RELEASE_AGE"
	assert_line "  .npmrc keys: minimumReleaseAge, minimum-release-age"
	assert_line "  Workspace YAML keys: minimumReleaseAge"
	assert_output --partial "Set to \`0\` to disable."
}

@test "config tui rejects non-interactive stdout" {
	run aube config tui
	assert_failure
	assert_output --partial "requires an interactive terminal"
}

# ── top-level get / set aliases ──────────────────────────────────────

@test "get delegates to config get" {
	echo "autoInstallPeers=false" >"$HOME/.npmrc"
	run aube get autoInstallPeers
	assert_success
	assert_output "false"
}

@test "set delegates to config set" {
	run aube set autoInstallPeers false
	assert_success
	run cat "$XDG_CONFIG_HOME/aube/config.toml"
	assert_output --partial "autoInstallPeers = false"
}
