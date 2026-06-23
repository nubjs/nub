# bats-file

[![GitHub license](https://img.shields.io/badge/license-CC0-blue.svg)](https://raw.githubusercontent.com/bats-core/bats-file/master/LICENSE)
[![GitHub release](https://img.shields.io/github/release/bats-core/bats-file.svg)](https://github.com/bats-core/bats-file/releases/latest)
[![Tests](https://github.com/bats-core/bats-file/actions/workflows/tests.yml/badge.svg)](https://github.com/bats-core/bats-file/actions/workflows/tests.yml)

`bats-file` is a helper library providing common filesystem related
assertions and helpers for [Bats][bats].

Assertions are functions that perform a test and output relevant
information on failure to help debugging. They return 1 on failure and 0
otherwise. Output, [formatted][bats-support-output] for readability, is
sent to the standard error to make assertions usable outside of `@test`
blocks too.

Features:
- [assertions](#usage)
- [temporary directory handling](#working-with-temporary-directories)

Dependencies:
- [`bats-support`][bats-support] - output formatting, function call
  restriction

See the [shared documentation][bats-docs] to learn how to install and
load this library.

## **Index of all functions**

| Test File Types | Test File Attributes | Test File Content |
| ----------- | ----------- | ----------- |
| _Check if a **file or directory** exists!_ <br/> - [assert_exists](#assert_exists) <br/> - [assert_not_exists](#assert_not_exists) | _Check if file is **executable**!_ <br/> - [assert_file_executable](#assert_file_executable) <br/> - [assert_file_not_executable](#assert_file_not_executable) | _Check if file is **empty**!_ <br/> - [assert_file_empty](#assert_file_empty) <br/> - [assert_file_not_empty](#assert_file_not_empty) |
| _Check if a **file** exists!_ <br/> - [assert_file_exists](#assert_file_exists) <br/> - [assert_file_not_exists](#assert_file_not_exists) | _Check the **owner** of a file!_ <br/> - [assert_file_owner](#assert_file_owner) <br/> - [assert_file_not_owner](#assert_file_not_owner) | _Check if file **contains regex**!_ <br/>  - [assert_file_contains](#assert_file_contains) <br/> - [assert_file_not_contains](#assert_file_not_contains) |
| _Check if a **directory** exists!_ <br/> - [assert_dir_exists](#assert_dir_exists) <br/> - [assert_dir_not_exists](#assert_dir_not_exists) | _Check the **permission** of a file!_ <br/> - [assert_file_permission](#assert_file_permission) <br/> - [assert_not_file_permission](#assert_not_file_permission) | _Check if file is a **symlink to target**!_ <br/> - [assert_symlink_to](#assert_symlink_to) <br/> - [assert_not_symlink_to](#assert_not_symlink_to) |
| _Check if a **link** exists!_ <br/> - [assert_link_exists](#assert_link_exists) <br/> - [assert_link_not_exists](#assert_link_not_exists) | _Check the **size** of a file **by bytes**!_ <br/> - [assert_file_size_equals](#assert_file_size_equals) | _Check if file **contents are the same**!_ <br/> - [assert_files_equal](#assert_files_equal) |
| _Check if a **block special file** exists!_ <br/> - [assert_block_exists](#assert_block_exists) <br/> - [assert_block_not_exists](#assert_block_not_exists) | _Check if a file have **zero bytes**!_ <br/> - [assert_size_zero](#assert_size_zero) <br/> - [assert_size_not_zero](#assert_size_not_zero) |
| _Check if a **character special file** exists!_ <br/> - [assert_character_exists](#assert_character_exists) <br/> - [assert_character_not_exists](#assert_character_not_exists) | _Check the **groupID** of a file!_ <br/> - [assert_file_group_id_set](#assert_file_group_id_set) <br/> - [assert_file_not_group_id_set](#assert_file_not_group_id_set) |
| _Check if a **socket** exists!_ <br/> - [assert_socket_exists](#assert_socket_exists) <br/> - [assert_socket_not_exists](#assert_socket_not_exists) | _Check the **userID** of a file!_ <br/> - [assert_file_user_id_set](#assert_file_user_id_set) <br/> - [assert_file_not_user_id_set](#assert_file_not_user_id_set) |
| _Check if a **fifo special file** exists!_ <br/> - [assert_fifo_exists](#assert_fifo_exists) <br/> - [assert_fifo_not_exists](#assert_fifo_not_exists) | _Check if a **stickybit is set**!_ <br/> - [assert_sticky_bit](#assert_sticky_bit) <br/> - [assert_no_sticky_bit](#assert_no_sticky_bit) |


## **Usage**

## _Test File Types:_


### `assert_exists`

Fail if the given file or directory does not exist.

```bash
@test 'assert_exists()' {
  assert_exists /path/to/non-existent-file-or-dir
}
```

On failure, the path is displayed.

```
-- file or directory does not exist --
path : /path/to/non-existent-file-or-dir
--
```
[Back to index](#Index-of-all-functions)


### `assert_not_exists`

Fail if the given file or directory does exist.

```bash
@test 'assert_not_exists()' {
  assert_not_exists /path/to/existent-file-or-dir
}
```

On failure, the path is displayed.

```
-- file or directory exists, but it was expected to be absent --
path : /path/to/existent-file-or-dir
--
```
[Back to index](#Index-of-all-functions)

---


### `assert_file_exists`

Fail if the given file does not exist.

```bash
@test 'assert_file_exists()' {
  assert_file_exists /path/to/non-existent-file
}
```

On failure, the path is displayed.

```
-- file does not exist --
path : /path/to/non-existent-file
--
```
[Back to index](#Index-of-all-functions)


### `assert_file_not_exists`

Fail if the given file or directory exists.

```bash
@test 'assert_file_not_exists() {
  assert_file_not_exists /path/to/existing-file
}
```

On failure, the path is displayed.

```
-- file or directory exists, but it was expected to be absent --
path : /path/to/existing-file
--
```
[Back to index](#Index-of-all-functions)

---


### `assert_dir_exists`

Fail if the given directory does not exist.

```bash
@test 'assert_dir_exists()' {
  assert_dir_exists /path/to/non-existent-directory
}
```

On failure, the path is displayed.

```
-- directory does not exist --
path : /path/to/non-existent-directory
--
```
[Back to index](#Index-of-all-functions)


### `assert_dir_not_exists`

Fail if the given directory exists.

```bash
@test 'assert_dir_not_exists() {
  assert_dir_not_exists /path/to/existing-directory
}
```

On failure, the path is displayed.

```
-- directory exists, but it was expected to be absent --
path : /path/to/existing-directory
--
```
[Back to index](#Index-of-all-functions)

---


### `assert_link_exists`

Fail if the given symbolic link does not exist.

```bash
@test 'assert_link_exists()' {
  assert_link_exists /path/to/non-existent-link-file
}
```

On failure, the path is displayed.

```
-- symbolic link does not exist --
path : /path/to/non-existent-link-file
--
```
[Back to index](#Index-of-all-functions)


### `assert_link_not_exists`

Fail if the given symbolic link exists.

```bash
@test 'assert_link_not_exists() {
  assert_link_not_exists /path/to/existing-link-file
}
```

On failure, the path is displayed.

```
-- symbolic link exists, but it was expected to be absent --
path : /path/to/existing-link-file
--
```
[Back to index](#Index-of-all-functions)

---


### `assert_block_exists`

Fail if the given block special file does not exist.

```bash
@test 'assert_block_exists()' {
  assert_block_exists /path/to/non-existent-block-file
}
```

On failure, the path is displayed.

```
-- block special file does not exist --
path : /path/to/non-existent-block-file
--
```
[Back to index](#Index-of-all-functions)


### `assert_block_not_exists`

Fail if the given block special file exists.

```bash
@test 'assert_block_not_exists() {
  assert_block_not_exists /path/to/existing-block-file
}
```

On failure, the path is displayed.

```
-- block special file exists, but it was expected to be absent --
path : /path/to/existing-block-file
--
```
[Back to index](#Index-of-all-functions)

---


### `assert_character_exists`

Fail if the given character special file does not exist.

```bash
@test 'assert_character_exists()' {
  assert_character_exists /path/to/non-existent-character-file
}
```

On failure, the path is displayed.

```
-- character special file does not exist --
path : /path/to/non-existent-character-file
--
```
[Back to index](#Index-of-all-functions)


### `assert_character_not_exists`

Fail if the given character special file exists.

```bash
@test 'assert_character_not_exists() {
  assert_character_not_exists /path/to/existing-character-file
}
```

On failure, the path is displayed.

```
-- character special file exists, but it was expected to be absent --
path : /path/to/existing-character-file
--
```
[Back to index](#Index-of-all-functions)

---


### `assert_socket_exists`

Fail if the given socket does not exist.

```bash
@test 'assert_socket_exists()' {
  assert_socket_exists /path/to/non-existent-socket
}
```

On failure, the path is displayed.

```
-- socket does not exist --
path : /path/to/non-existent-socket
--
```
[Back to index](#Index-of-all-functions)


### `assert_socket_not_exists`

Fail if the given socket exists.

```bash
@test 'assert_socket_not_exists() {
  assert_socket_not_exists /path/to/existing-socket
}
```

On failure, the path is displayed.

```
-- socket exists, but it was expected to be absent --
path : /path/to/existing-socket
--
```
[Back to index](#Index-of-all-functions)

---


### `assert_fifo_exists`

Fail if the given named pipe does not exist.

```bash
@test 'assert_fifo_exists()' {
  assert_fifo_exists /path/to/non-existent-fifo-file
}
```

On failure, the path is displayed.

```
-- fifo does not exist --
path : /path/to/non-existent-fifo-file
--
```
[Back to index](#Index-of-all-functions)


### `assert_fifo_not_exists`

Fail if the given named pipe exists.

```bash
@test 'assert_fifo_not_exists()' {
  assert_fifo_not_exists /path/to/existing-fifo-file
}
```

On failure, the path is displayed.

```
-- named pipe exists, but it was expected to be absent --
path : /path/to/existing-fifo-file
--
```
[Back to index](#Index-of-all-functions)

---
---


## _Test File Attributes:_



### `assert_file_executable`

Fail if the given file is not executable.

```bash
@test 'assert_file_executable()' {
  assert_file_executable /path/to/executable-file
}
```

On failure, the path is displayed.

```
-- file is not executable --
path : /path/to/executable-file
--
```
[Back to index](#Index-of-all-functions)


### `assert_file_not_executable`

Fail if the given file is executable.

```bash
@test 'assert_file_not_executable()' {
  assert_file_not_executable /path/to/executable-file
}
```

On failure, the path is displayed.

```
-- file is executable, but it was expected to be not executable --
path : /path/to/executable-file
--
```
[Back to index](#Index-of-all-functions)

---


### `assert_file_owner`

Fail if given user is not the owner of the given file.

```bash
@test 'assert_file_owner() {
  assert_file_owner $owner /path/to/owner
}
```

On failure, the path is displayed.

```
-- user is not the owner of the file --
path : /path/to/notowner
owner : user
--
```
[Back to index](#Index-of-all-functions)


### `assert_not_file_owner`

Fail if given user is the owner of the given file.

```bash
@test 'assert_not_file_owner() {
  assert_not_file_owner $owner /path/to/notowner
}
```

On failure, the path is displayed.

```
-- Fail if given user is the owner, but it was expected not to be --
path : /path/to/owner
owner : $owner
--
```
[Back to index](#Index-of-all-functions)

---


### `assert_file_permission`

Fail if given file does not have given permission.

```bash
@test 'assert_file_permission() {
  assert_file_permission $permission /path/to/permission
}
```

On failure, the path is displayed.

```
-- given file does not have permissions 777 --
path : /path/to/nopermission
permission: $permission
--
```
[Back to index](#Index-of-all-functions)


### `assert_not_file_permission`

Fail if given file has given permission.

```bash
@test 'assert_not_file_permission() {
  assert_not_file_permission $permission /path/to/nopermission
}
```

On failure, the path is displayed.

```
-- given file has permissions 777, but it was expected not to have --
path : /path/to/permission
permission : $permission
--
```
[Back to index](#Index-of-all-functions)

---


### `assert_file_size_equals`
Fail if the given file size does not match the input.
```bash
@test 'assert_file_size_equals() {
    assert_file_size_equals /path/to/non-empty-file bytecount
}
```
On failure, the path and expected bytecount are displayed.

[Back to index](#Index-of-all-functions)

---


### `assert_size_zero`

Fail if file is not zero byte.

```bash
@test 'assert_size_zero() {
  assert_size_zero /path/to/zerobyte
}
```

On failure, the path is displayed.

```
-- file is not zero byte --
path : /path/to/notzerobyte
--
```
[Back to index](#Index-of-all-functions)


### `assert_size_not_zero`

Fail if file size is zero byte.

```bash
@test 'assert_size_not_zero() {
  assert_size_not_zero /path/to/notzerobyte
}
```

On failure, the path is displayed.

```
-- file is 0 byte, but it was expected not to be --
path : /path/to/zerobyte
--
```
[Back to index](#Index-of-all-functions)

---


### `assert_file_group_id_set`

Fail if group id is not set.

```bash
@test 'assert_file_group_id_set() {
  assert_file_group_id_set /path/to/groupidset
}
```

On failure, the path is displayed.

```
-- group id is not set --
path : /path/to/groupidnotset
--
```
[Back to index](#Index-of-all-functions)


### `assert_file_not_group_id_set`

Fail if group id is set.

```bash
@test 'assert_file_not_group_id_set() {
  assert_file_not_group_id_set /path/to/groupidnotset
}
```

On failure, the path is displayed.

```
-- group id is set, but it was expected not to be --
path : /path/to/groupdidset
--
```
[Back to index](#Index-of-all-functions)

---


### `assert_file_user_id_set`

Fail if user id is not set.

```bash
@test 'assert_file_user_id_set() {
  assert_file_user_id_set /path/to/useridset
}
```

On failure, the path is displayed.

```
-- user id is not set --
path : /path/to/useridnotset
--
```
[Back to index](#Index-of-all-functions)


### `assert_file_not_user_id_set`

Fail if user id is set.

```bash
@test 'assert_file_not_user_id_set() {
  assert_file_not_user_id_set /path/to/groupidnotset
}
```

On failure, the path is displayed.

```
-- user id is set, but it was expected not to be --
path : /path/to/userdidset
--
```
[Back to index](#Index-of-all-functions)

---


### `assert_sticky_bit`

Fail if stickybit is not set.

```bash
@test 'assert_sticky_bit() {
  assert_sticky_bit /path/to/stickybit
}
```

On failure, the path is displayed.

```
-- stickybit is not set --
path : /path/to/notstickybit
--
```
[Back to index](#Index-of-all-functions)


### `assert_not_sticky_bit`

Fail if stickybit is set.

```bash
@test 'assert_not_sticky_bit() {
  assert_not_sticky_bit /path/to/notstickybit
}
```

On failure, the path is displayed.

```
-- stickybit is set, but it was expected not to be --
path : /path/to/stickybit
--
```
[Back to index](#Index-of-all-functions)

---


## _Test File Content:_


### `assert_file_empty`
Fail if the given file or directory is not empty.
```bash
@test 'assert_file_empty()' {
  assert_file_empty /path/to/empty-file
}
```
On failure, the path and the content of the file is displayed.
```
-- file is not empty --
path : /path/to/empty-file
output (2 lines) : content-line-1
content-line-2
--
```
[Back to index](#Index-of-all-functions)


### `assert_file_not_empty`
Fail if the given file or directory empty.
```bash
@test 'assert_file_not_empty() {
  assert_file_not_empty /path/to/non-empty-file
}
```
On failure, the path is displayed.
```
-- file empty, but it was expected to contain something --
path : /path/to/non-empty-file
--
```
[Back to index](#Index-of-all-functions)

---


### `assert_file_contains`
Fail if the given file does not contain the regex.
```bash
@test 'assert_file_contains() {
    assert_file_contains /path/to/non-empty-file regex engine
}
```
`engine` is optional and can be one of `grep`, `egrep` or `pcregrep`. The specified engine must be available on the system running the tests.

On failure, the path and expected regex are displayed.

[Back to index](#Index-of-all-functions)

---

### `assert_file_not_contains`
Fail if the given file contains the regex or if the file does not exist.
```bash
@test 'assert_file_not_contains() {
    assert_file_not_contains /path/to/non-empty-file regex
}
```
On failure, the path and regex are displayed.

[Back to index](#Index-of-all-functions)

---

### `assert_symlink_to`
Fail if the given file is not a symbolic to a defined target.
```bash
@test 'assert_symlink_to() {
  assert_symlink_to /path/to/source-file /path/to/symlink
}
```
On failure, the path is displayed.
```
-- symbolic link does not have the correct target --
path : /path/to/symlink
--
```
[Back to index](#Index-of-all-functions)


### `assert_not_symlink_to`
Fail if the given file is a symbolic to a defined target.
```bash
@test 'assert_not_symlink_to() {
  assert_not_symlink_to /path/to/source-file /path/to/symlink
}
```
On failure, the path is displayed.
```
-- file is a symbolic link --
path : /path/to/symlink
--
-- symbolic link does have the correct target --
path : /path/to/symlink
--
```
[Back to index](#Index-of-all-functions)

### `assert_files_equal`
Fail if the given files are not the same.
```bash
@test 'assert_files_equal()' {
  assert_files_equal /path/to/file1 /path/to/file2
}
```
On failure, the paths are displayed.
```
-- files are not the same --
path1 : /path/to/file1
path2 : /path/to/file2
--
```
[Back to index](#Index-of-all-functions)

---
---


## Working with temporary directories

When testing code that manipulates the filesystem, it is good practice
to run tests in clean, throw-away environments to ensure correctness and
reproducibility. Therefore, this library includes convenient functions
to create and destroy temporary directories.


### `temp_make`

Create a temporary directory for the current test in `BATS_TMPDIR`. The
directory is guaranteed to be unique and its name is derived from the
test's filename and number for easy identification.

```
<test-filename>-<test-number>-<random-string>
```

This information is only available in `setup`, `@test` and `teardown`,
thus the function must be called from one of these locations.

The path of the directory is displayed on the standard output and is
meant to be captured into a variable.

```bash
setup() {
  TEST_TEMP_DIR="$(temp_make)"
}
```

For example, for the first test in `sample.bats`, this snippet creates a
directory named `sample.bats-1-XXXXXXXXXX`, where each trailing `X` is a
random alphanumeric character.

If the directory cannot be created, the function fails and displays an
error message on the standard error.

```
-- ERROR: temp_make --
mktemp: failed to create directory via template ‘/etc/sample.bats-1-XXXXXXXXXX’: Permission denied
--
```

#### Directory name prefix

The directory name can be prefixed with an arbitrary string using the `--prefix
<prefix>` option (`-p <prefix>` for short).

```bash
setup() {
  TEST_TEMP_DIR="$(temp_make --prefix 'myapp-')"
}
```

Following the previous example, this will create a directory named
`myapp-sample.bats-1-XXXXXXXXXX`. This can be used to group temporary
directories.

Generally speaking, the directory name is of the following form.

```
<prefix><test-filename>-<test-number>-<random-string>
```


### `temp_del`

Delete a temporary directory, typically created with `temp_make`.

```bash
teardown() {
  temp_del "$TEST_TEMP_DIR"
}
```

If the directory cannot be deleted, the function fails and displays an
error message on the standard error.

```
-- ERROR: temp_del --
rm: cannot remove '/etc/sample.bats-1-04RUVmBP7x': No such file or directory
--
```

_**Note:** Actually, this function can be used to delete any file or
directory. However, it is most useful in deleting temporary directories
created with `temp_make`, hence the naming._

#### Preserve directory

During development, it is useful to peak into temporary directories
post-mortem to see what the tested code has done.

When `BATSLIB_TEMP_PRESERVE` is set to 1, the function succeeds but the
directory is not deleted.

```bash
$ BATSLIB_TEMP_PRESERVE=1 bats sample.bats
```

#### Preserve directory on failure

During debugging, it is useful to preserve the temporary directories of
failing tests.

When `BATSLIB_TEMP_PRESERVE_ON_FAILURE` is set to 1, the function
succeeds but the directory is not deleted if the test has failed.

```bash
$ BATSLIB_TEMP_PRESERVE_ON_FAILURE=1 bats sample.bats
```

The outcome of a test is only known in `teardown`, therefore this
feature can be used only when `temp_del` is called from that location.
Otherwise and error is displayed on the standard error.


## Transforming displayed paths

Sometimes paths can be long and tiresome to parse to the human eye. To
help focus on the interesting bits, all functions support hiding part of
the displayed paths by replacing it with an arbitrary string.

A single [pattern substitution][bash-pe] is performed on the path before
displaying it.

```
${path/$BATSLIB_FILE_PATH_REM/$BATSLIB_FILE_PATH_ADD}
```

The longest match of the pattern `BATSLIB_FILE_PATH_REM` is replaced
with `BATSLIB_FILE_PATH_ADD`. To anchor the pattern to the beginning or
the end, prepend `#` or `%`, respectively.

For example, the following example hides the path of the temporary
directory where the test takes place.

```bash
setup {
  TEST_TEMP_DIR="$(temp_make)"
  BATSLIB_FILE_PATH_REM="#${TEST_TEMP_DIR}"
  BATSLIB_FILE_PATH_ADD='<temp>'
}

@test 'assert_file_exists()' {
  assert_file_exists "${TEST_TEMP_DIR}/path/to/non-existent-file"
}

teardown() {
  temp_del "$TEST_TEMP_DIR"
}
```

On failure, only the relevant part of the path is shown.

```
-- file does not exist --
path : <temp>/path/to/non-existent-file
--
```

## **Development**

No one would want to develop piece of bash dependent libraries on their laptops due to single mistake (globbing for instance) can cause a disaster. In order to prevent this there is a Vagrantfile that you can use.

In order to start development environment, you have to take two steps;

```shell
user@localhost:~/bats-file$ vagrant up
```

The line above spins up a brand new virtualbox image and provisions with prerequisites.

However, as the tests require not to be on a network share due to running commands eg: `mknod`, the files are shared into the VM by `rsync` module. Rsync in vagrant only runs initially and stops. During the active development, you regularly change files and might want to see the impact. To achieve that, you have to use auto rsync.

> `auto-rsync` is a long running command. It means that it has to run on terminal screen as long as the VM is up and running. So, you have to keep this in a dedicated terminal screen.

After bringing up the VM, you can simply run following command;

```shell
user@localhost:~/bats-file$ vagrant rsync-auto
```

**WARNING! WARNING! WARNING!:** There will be a small delay between file save and sync. When you save a file please keep eye on the terminal window/pane and sync is triggered and finished. Simply run your next test with 5s-10s delay.

The repo files can be found under `/home/vagrant/bats-file` and you can run tests with

```shell
user@localhost:~/bats-file$ bats test
# or
user@localhost:~/bats-file$ bats test/on-particular-test.bats
```

Once you're done with development, you can simply turn the VM off with

```shell
user@localhost:~/bats-file$ vagrant halt
```

### Why?

- Why Vagrant, not Docker
  - This replicates the 100% CI
- Why EoL Xenial image
  - This replicates the 100% CI. Also, one step at a time. This wasn't in place at all. However, this change will bring up other concerns. So, that problems should be solved at another time.

<!-- REFERENCES -->

[bats]: https://github.com/bats-core/bats-core
[bats-support-output]: https://github.com/bats-core/bats-support#output-formatting
[bats-support]: https://github.com/bats-core/bats-support
[bats-docs]: https://github.com/ztombol/bats-docs
[bash-pe]: https://www.gnu.org/software/bash/manual/html_node/Shell-Parameter-Expansion.html
