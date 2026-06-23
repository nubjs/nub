#!/usr/bin/env bats
# bats file_tags=serial

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

_setup_vlt_fixture() {
	local fixture="$1"
	cp -R "$PROJECT_ROOT/fixtures/vlt-benchmarks/$fixture/." .
	printf 'registry=https://registry.npmjs.org/\n' >>.npmrc
}

_clean_aube_cache() {
	rm -rf "$XDG_CACHE_HOME/aube" "$XDG_DATA_HOME/aube"
}

_clean_lockfiles() {
	rm -f aube-lock.yaml
}

_clean_node_modules() {
	rm -rf node_modules
}

_clean_package_manager_files() {
	rm -rf .aube
}

_clean_build_files() {
	rm -rf .cache .nx .turbo
}

_clean_all() {
	_clean_node_modules
	_clean_lockfiles
	_clean_package_manager_files
	_clean_aube_cache
	_clean_build_files
}

_aube_benchmark_install() {
	run aube install --silent
	assert_success
	assert_file_exists aube-lock.yaml
	assert_dir_exists node_modules
}

_prime_vlt_fixture() {
	_clean_all
	_aube_benchmark_install
}

_run_vlt_variation() {
	local variation="$1"

	case "$variation" in
	clean)
		_clean_all
		;;
	cache)
		_prime_vlt_fixture
		_clean_lockfiles
		_clean_node_modules
		_clean_package_manager_files
		;;
	lockfile)
		_prime_vlt_fixture
		_clean_aube_cache
		_clean_node_modules
		_clean_package_manager_files
		;;
	node_modules)
		_prime_vlt_fixture
		_clean_aube_cache
		_clean_lockfiles
		_clean_package_manager_files
		;;
	cache+lockfile)
		_prime_vlt_fixture
		_clean_node_modules
		_clean_package_manager_files
		;;
	cache+node_modules)
		_prime_vlt_fixture
		_clean_lockfiles
		_clean_package_manager_files
		;;
	lockfile+node_modules)
		_prime_vlt_fixture
		_clean_aube_cache
		_clean_package_manager_files
		;;
	cache+lockfile+node_modules)
		_prime_vlt_fixture
		_clean_package_manager_files
		_clean_build_files
		;;
	*)
		echo "unknown vlt variation: $variation" >&2
		return 1
		;;
	esac

	_aube_benchmark_install
}

_install_vlt_fixture_like_benchmark() {
	local variation
	for variation in \
		clean \
		cache \
		lockfile \
		node_modules \
		cache+lockfile \
		cache+node_modules \
		lockfile+node_modules \
		cache+lockfile+node_modules; do
		_run_vlt_variation "$variation"
	done
}

@test "vlt benchmark fixture: svelte install variations complete" {
	_setup_vlt_fixture svelte
	_install_vlt_fixture_like_benchmark
}
