#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "npm package-lock transitive win32 optional is pruned before fetch on non-win32" {
	case "$(uname -s)" in
	MINGW* | MSYS* | CYGWIN* | Windows_NT)
		skip "win32 host should keep the win32 optional dependency"
		;;
	esac

	mkdir -p host
	cat >host/package.json <<-'JSON'
		{
		  "name": "host",
		  "version": "1.0.0",
		  "optionalDependencies": {
		    "native-win": "1.0.0"
		  }
		}
	JSON
	cat >package.json <<-'JSON'
		{
		  "name": "npm-lock-platform-root",
		  "version": "1.0.0",
		  "dependencies": {
		    "host": "file:host"
		  }
		}
	JSON
	cat >package-lock.json <<-'JSON'
		{
		  "name": "npm-lock-platform-root",
		  "version": "1.0.0",
		  "lockfileVersion": 3,
		  "requires": true,
		  "packages": {
		    "": {
		      "name": "npm-lock-platform-root",
		      "version": "1.0.0",
		      "dependencies": {
		        "host": "file:host"
		      }
		    },
		    "node_modules/host": {
		      "resolved": "host",
		      "link": true
		    },
		    "host": {
		      "name": "host",
		      "version": "1.0.0",
		      "optionalDependencies": {
		        "native-win": "1.0.0"
		      }
		    },
		    "node_modules/native-win": {
		      "version": "1.0.0",
		      "resolved": "http://127.0.0.1:9/native-win/-/native-win-1.0.0.tgz",
		      "integrity": "sha512-z4PhNX7vuL3xVChQ1m2AB9Yg5AULVxXcg/SpIdNs6c5H0NE8XYXysP+DGNKHfuwvY7kxvUdBeoGlODJ6+SfaPg==",
		      "optional": true,
		      "os": ["win32"]
		    }
		  }
		}
	JSON

	run aube ci --ignore-scripts
	assert_success
	assert_exists node_modules/host
	assert_not_exists node_modules/native-win
}
