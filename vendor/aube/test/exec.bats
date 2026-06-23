#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube exec --parallel fans out across workspace packages" {
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - packages/*
EOF
	cat >package.json <<'EOF'
{"name":"root","version":"0.0.0","private":true}
EOF
	mkdir -p packages/a packages/b
	cat >packages/a/package.json <<'EOF'
{"name":"a","version":"0.0.0","dependencies":{"is-odd":"3.0.1"}}
EOF
	cat >packages/b/package.json <<'EOF'
{"name":"b","version":"0.0.0","dependencies":{"is-odd":"3.0.1"}}
EOF

	run aube install
	assert_success

	run aube -r exec --parallel --shell-mode node -- -e 'console.log(require("is-odd")(3) ? process.cwd().split("/").pop() : "no")'
	assert_success
	assert_output --partial "a"
	assert_output --partial "b"
}

@test "aube exec --parallel preflights missing binaries before spawning" {
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - packages/*
EOF
	cat >package.json <<'EOF'
{"name":"root","version":"0.0.0","private":true}
EOF
	mkdir -p packages/a/node_modules/.bin packages/b
	cat >packages/a/package.json <<'EOF'
{"name":"a","version":"0.0.0"}
EOF
	cat >packages/b/package.json <<'EOF'
{"name":"b","version":"0.0.0"}
EOF
	cat >packages/a/node_modules/.bin/sentinel <<'EOF'
#!/usr/bin/env bash
echo ran >../../sentinel-ran
EOF
	chmod +x packages/a/node_modules/.bin/sentinel

	run aube install
	assert_success
	mkdir -p packages/a/node_modules/.bin
	cat >packages/a/node_modules/.bin/sentinel <<'EOF'
#!/usr/bin/env bash
echo ran >../../sentinel-ran
EOF
	chmod +x packages/a/node_modules/.bin/sentinel

	run aube -r exec --parallel sentinel
	assert_failure
	assert_output --partial "binary not found in b: sentinel"
	assert_file_not_exist packages/sentinel-ran
}
