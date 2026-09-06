#!/usr/bin/env bats
#
# Network-dependent compatibility checks for real frameworks that use aube's
# global virtual store. Gated so the default BATS suite remains hermetic.

setup() {
	load 'test_helper/common_setup'
	local node_bin_dir
	node_bin_dir="$(dirname "$(mise which node)")"
	_common_setup
	export PATH="$node_bin_dir:$PATH"
	if [ "${AUBE_NETWORK_TESTS:-}" != "1" ]; then
		skip "set AUBE_NETWORK_TESTS=1 to run network tests"
	fi
	printf 'registry=https://registry.npmjs.org/\n' >.npmrc
}

teardown() {
	_common_teardown
}

@test "Nuxt 3 and 4 prepare and build with the global virtual store" {
	for version in 3.21.11 4.5.2; do
		mkdir "nuxt-$version"
		(
			cd "nuxt-$version"
			cat >package.json <<JSON
{
  "name": "nuxt-gvs-$version",
  "version": "1.0.0",
  "scripts": {
    "build": "nuxt build",
    "postinstall": "nuxt prepare"
  },
  "dependencies": {
    "nuxt": "$version"
  }
}
JSON

			run aube install
			assert_success
			refute_output --partial "disableGlobalVirtualStoreForPackages"
			run find node_modules/.aube -mindepth 1 -maxdepth 1 -type l
			assert_success
			[ -n "$output" ]

			run aube run build
			assert_success
			assert_file_exists .output/server/index.mjs
		)
	done
}

@test "Parcel discovers .parcelrc and builds with the global virtual store" {
	mkdir -p parcel/src
	cd parcel
	# Strict layouts require configs referenced by the project to be direct.
	cat >package.json <<'JSON'
{
  "name": "parcel-gvs",
  "version": "1.0.0",
  "scripts": {
    "build": "parcel build src/index.html"
  },
  "devDependencies": {
    "@parcel/config-default": "2.16.4",
    "parcel": "2.16.4"
  }
}
JSON
	cat >.parcelrc <<'JSON'
{
  "extends": "@parcel/config-default"
}
JSON
	cat >src/index.html <<'HTML'
<!doctype html>
<title>Parcel GVS compatibility</title>
<script type="module" src="./index.js"></script>
HTML
	cat >src/index.js <<'JS'
document.body.textContent = "parcel works";
JS

	run aube install
	assert_success
	refute_output --partial "disableGlobalVirtualStoreForPackages"
	run realpath node_modules/parcel
	assert_success
	[[ "$output" == */aube/virtual-store/v1/* ]]

	run aube run build
	assert_success
	assert_file_exists dist/index.html
}
