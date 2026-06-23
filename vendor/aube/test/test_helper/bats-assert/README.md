# bats-assert

[![License](https://img.shields.io/npm/l/bats-assert.svg)](https://github.com/bats-core/bats-assert/blob/master/LICENSE)
[![GitHub release](https://img.shields.io/github/release/bats-core/bats-assert.svg)](https://github.com/bats-core/bats-assert/releases/latest)
[![npm release](https://img.shields.io/npm/v/bats-assert.svg)](https://www.npmjs.com/package/bats-assert)
[![Tests](https://github.com/bats-core/bats-assert/actions/workflows/test.yml/badge.svg?branch=master)](https://github.com/bats-core/bats-assert/actions/workflows/test.yml)

`bats-assert` is a helper library providing common assertions for [Bats][bats].

- [Install](#install)
- [Usage](#usage)
- [Options](#options)
- [Full Assertion API](#full-assertion-api)

In the context of this project, an [assertion][wikipedia-assertions] is a function that perform a test and returns `1` on failure or `0` on success.
To make debugging easier, the assertion also outputs relevant information on failure.
The output is [formatted][bats-support-output] for readability.
To make assertions usable outside of `@test` blocks, the output is sent to [stderr][wikipedia-stderr].

The most recent invocation of Bats' `run` function is used for testing assertions on output and status code.

[wikipedia-assertions]: https://en.wikipedia.org/wiki/Assertion_(software_development)
[wikipedia-stderr]: https://en.wikipedia.org/wiki/Standard_streams#Standard_error_(stderr)

## Install

This project has one dependency, for output formatting: [`bats-support`][bats-support]

Read the [shared documentation][bats-docs] to learn how to install and load both libraries.

## Usage

This project provides the following functions:

 - [assert](#assert) / [refute](#refute) Assert a given expression evaluates to `true` or `false`.
 - [assert_equal](#assert_equal) Assert two parameters are equal.
 - [assert_not_equal](#assert_not_equal) Assert two parameters are not equal.
 - [assert_success](#assert_success) / [assert_failure](#assert_failure) Assert exit status is `0` or `1`.
 - [assert_output](#assert_output) / [refute_output](#refute_output) Assert output does (or does not) contain given content.
 - [assert_line](#assert_line) / [refute_line](#refute_line) Assert a specific line of output does (or does not) contain given content.
 - [assert_regex](#assert_regex) / [refute_regex](#refute_regex) Assert a parameter does (or does not) match given pattern.
 - [assert_stderr](#assert_stderr) / [refute_stderr](#refute_stderr) Assert stderr does (or does not) contain given content.
 - [assert_stderr_line](#assert_stderr_line) / [refute_stderr_line](#refute_stderr_line) Assert a specific line of stderr does (or does not) contain given content.

These commands are described in more detail below.

## Options

For functions that have options, `--` disables option parsing for the remaining arguments to allow using arguments identical to one of the allowed options.

```bash
assert_output -- '-p'
```

Specifying `--` as an argument is similarly simple.

```bash
refute_line -- '--'
```


## Full Assertion API

### `assert`

Fail if the given expression evaluates to false.

> _**Note**:
> The expression must be a simple command.
> [Compound commands][bash-comp-cmd], such as `[[`, can be used only when executed with `bash -c`._

```bash
@test 'assert()' {
  assert [ 1 -lt 0 ]
}
```

On failure, the failed expression is displayed.

```
-- assertion failed --
expression : [ 1 -lt 0 ]
--
```


### `refute`

Fail if the given expression evaluates to true.

> _**Note**
> The expression must be a simple command.
> [Compound commands][bash-comp-cmd], such as `[[`, can be used only when executed with `bash -c`._

```bash
@test 'refute()' {
  refute [ 1 -gt 0 ]
}
```

On failure, the successful expression is displayed.

```
-- assertion succeeded, but it was expected to fail --
expression : [ 1 -gt 0 ]
--
```


### `assert_equal`

Fail if the two parameters, actual and expected value respectively, do not equal.

```bash
@test 'assert_equal()' {
  assert_equal 'have' 'want'
}
```

On failure, the expected and actual values are displayed.

```
-- values do not equal --
expected : want
actual   : have
--
```

If either value is longer than one line both are displayed in *multi-line* format.


### `assert_not_equal`

Fail if the two parameters, actual and unexpected value respectively, are equal.

```bash
@test 'assert_not_equal()' {
  assert_not_equal 'foobar' 'foobar'
}
```

On failure, the expected and actual values are displayed.

```
-- values should not be equal --
unexpected : foobar
actual     : foobar
--
```

If either value is longer than one line both are displayed in *multi-line* format.


### `assert_success`

Fail if `$status` is not 0.

```bash
@test 'assert_success() status only' {
  run bash -c "echo 'Error!'; exit 1"
  assert_success
}
```

On failure, `$status` and `$output` are displayed.

```
-- command failed --
status : 1
output : Error!
--
```

If `$output` is longer than one line, it is displayed in *multi-line* format.


### `assert_failure`

Fail if `$status` is 0.

```bash
@test 'assert_failure() status only' {
  run echo 'Success!'
  assert_failure
}
```

On failure, `$output` is displayed.

```
-- command succeeded, but it was expected to fail --
output : Success!
--
```

If `$output` is longer than one line, it is displayed in *multi-line* format.

#### Expected status

When one parameter is specified, fail if `$status` does not equal the expected status specified by the parameter.

```bash
@test 'assert_failure() with expected status' {
  run bash -c "echo 'Error!'; exit 1"
  assert_failure 2
}
```

On failure, the expected and actual status, and `$output` are displayed.

```
-- command failed as expected, but status differs --
expected : 2
actual   : 1
output   : Error!
--
```

If `$output` is longer than one line, it is displayed in *multi-line* format.


### `assert_output`

This function helps to verify that a command or function produces the correct output by checking that the specified expected output matches the actual output.
Matching can be literal (default), partial or regular expression.
This function is the logical complement of `refute_output`.

#### Literal matching

By default, literal matching is performed.
The assertion fails if `$output` does not equal the expected output.

```bash
@test 'assert_output()' {
  run echo 'have'
  assert_output 'want'
}
```

On failure, the expected and actual output are displayed.

```
-- output differs --
expected : want
actual   : have
--
```

If either value is longer than one line both are displayed in *multi-line* format.

#### Existence

To assert that any (non-empty) output exists at all, simply omit the matching argument.

```bash
@test 'assert_output()' {
  run echo 'have'
  assert_output
}
```

On failure, an error message is displayed.

```
-- no output --
expected non-empty output, but output was empty
--
```

#### Partial matching

Partial matching can be enabled with the `--partial` option (`-p` for short).
When used, the assertion fails if the expected *substring* is not found in `$output`.

```bash
@test 'assert_output() partial matching' {
  run echo 'ERROR: no such file or directory'
  assert_output --partial 'SUCCESS'
}
```

On failure, the substring and the output are displayed.

```
-- output does not contain substring --
substring : SUCCESS
output    : ERROR: no such file or directory
--
```

This option and regular expression matching (`--regexp` or `-e`) are mutually exclusive.
An error is displayed when used simultaneously.

#### Regular expression matching

Regular expression matching can be enabled with the `--regexp` option (`-e` for short).
When used, the assertion fails if the *[extended regular expression]* does not match `$output`.

[extended regular expression]: https://en.wikibooks.org/wiki/Regular_Expressions/POSIX-Extended_Regular_Expressions

> [!IMPORTANT]
> Bash [doesn't support](https://stackoverflow.com/a/48898886/5432315) certain parts of regular expressions you may be used to:
>  * `\d` `\D` `\s` `\S` `\w` `\W` — these can be replaced with POSIX character class equivalents `[[:digit:]]`, `[^[:digit:]]`, `[[:space:]]`, `[^[:space:]]`, `[_[:alnum:]]`, and `[^_[:alnum:]]`, respectively.  (Notice the last case, where the `[:alnum:]` POSIX character class is augmented with underscore to be exactly equivalent to the Perl `\w` shorthand.)
>  * Non-greedy matching. You can sometimes replace `a.*?b` with something like `a[^ab]*b` to get a similar effect in practice, though the two are not exactly equivalent.
>  * Non-capturing parentheses `(?:...)`. In the trivial case, just use capturing parentheses `(...)` instead; though of course, if you use capture groups and/or backreferences, this will renumber your capture groups.
>  * Lookarounds like `(?<=before)` or `(?!after)`. (In fact anything with `(?` is a Perl extension.)  There is no simple general workaround for these, though you can sometimes rephrase your problem into one where lookarounds can be avoided.

> _**Note**:
> The anchors `^` and `$` bind to the beginning and the end of the entire output (not individual lines), respectively._

```bash
@test 'assert_output() regular expression matching' {
  run echo 'Foobar 0.1.0'
  assert_output --regexp '^Foobar v[0-9]+\.[0-9]+\.[0-9]$'
}
```

On failure, the regular expression and the output are displayed.

```
-- regular expression does not match output --
regexp : ^Foobar v[0-9]+\.[0-9]+\.[0-9]$
output : Foobar 0.1.0
--
```

An error is displayed if the specified extended regular expression is invalid.

This option and partial matching (`--partial` or `-p`) are mutually exclusive.
An error is displayed when used simultaneously.

#### Standard Input, HereDocs and HereStrings

The expected output can be specified via standard input (also heredoc/herestring) with the `-`/`--stdin` option.

```bash
@test 'assert_output() with pipe' {
  run echo 'hello'
  echo 'hello' | assert_output -
}

@test 'assert_output() with herestring' {
  run echo 'hello'
  assert_output - <<< hello
}
```


### `refute_output`

This function helps to verify that a command or function produces the correct output by checking that the specified unexpected output does not match the actual output.
Matching can be literal (default), partial or regular expression.
This function is the logical complement of `assert_output`.

#### Literal matching

By default, literal matching is performed.
The assertion fails if `$output` equals the unexpected output.

```bash
@test 'refute_output()' {
  run echo 'want'
  refute_output 'want'
}
```

On failure, the output is displayed.

```
-- output equals, but it was expected to differ --
output : want
--
```

If output is longer than one line it is displayed in *multi-line* format.

#### Existence

To assert that there is no output at all, simply omit the matching argument.

```bash
@test 'refute_output()' {
  run foo --silent
  refute_output
}
```

On failure, an error message is displayed.

```
-- unexpected output --
expected no output, but output was non-empty
--
```

#### Partial matching

Partial matching can be enabled with the `--partial` option (`-p` for short).
When used, the assertion fails if the unexpected *substring* is found in `$output`.

```bash
@test 'refute_output() partial matching' {
  run echo 'ERROR: no such file or directory'
  refute_output --partial 'ERROR'
}
```

On failure, the substring and the output are displayed.

```
-- output should not contain substring --
substring : ERROR
output    : ERROR: no such file or directory
--
```

This option and regular expression matching (`--regexp` or `-e`) are mutually exclusive.
An error is displayed when used simultaneously.

#### Regular expression matching

Regular expression matching can be enabled with the `--regexp` option (`-e` for short).
When used, the assertion fails if the *extended regular expression* matches `$output`.

> _**Note**:
> The anchors `^` and `$` bind to the beginning and the end of the entire output (not individual lines), respectively._

```bash
@test 'refute_output() regular expression matching' {
  run echo 'Foobar v0.1.0'
  refute_output --regexp '^Foobar v[0-9]+\.[0-9]+\.[0-9]$'
}
```

On failure, the regular expression and the output are displayed.

```
-- regular expression should not match output --
regexp : ^Foobar v[0-9]+\.[0-9]+\.[0-9]$
output : Foobar v0.1.0
--
```

An error is displayed if the specified extended regular expression is invalid.

This option and partial matching (`--partial` or `-p`) are mutually exclusive.
An error is displayed when used simultaneously.

#### Standard Input, HereDocs and HereStrings

The unexpected output can be specified via standard input (also heredoc/herestring) with the `-`/`--stdin` option.

```bash
@test 'refute_output() with pipe' {
  run echo 'hello'
  echo 'world' | refute_output -
}

@test 'refute_output() with herestring' {
  run echo 'hello'
  refute_output - <<< world
}
```


### `assert_line`

Similarly to `assert_output`, this function helps to verify that a command or function produces the correct output.
It checks that the expected line appears in the output (default) or in a specific line of it.
Matching can be literal (default), partial or regular expression.
This function is the logical complement of `refute_line`.

> _**Warning**:
> Due to a [bug in Bats][bats-93], empty lines are discarded from `${lines[@]}`,
> causing line indices to change and preventing testing for empty lines._

[bats-93]: https://github.com/sstephenson/bats/pull/93

#### Looking for a line in the output

By default, the entire output is searched for the expected line.
The assertion fails if the expected line is not found in `${lines[@]}`.

```bash
@test 'assert_line() looking for line' {
  run echo $'have-0\nhave-1\nhave-2'
  assert_line 'want'
}
```

On failure, the expected line and the output are displayed.

> _**Warning**:
> The output displayed does not contain empty lines.
> See the Warning above for more._

```
-- output does not contain line --
line : want
output (3 lines):
  have-0
  have-1
  have-2
--
```

If output is not longer than one line, it is displayed in *two-column* format.

#### Matching a specific line

When the `--index <idx>` option is used (`-n <idx>` for short), the expected line is matched only against the line identified by the given index.
The assertion fails if the expected line does not equal `${lines[<idx>]}`.

```bash
@test 'assert_line() specific line' {
  run echo $'have-0\nhave-1\nhave-2'
  assert_line --index 1 'want-1'
}
```

On failure, the index and the compared lines are displayed.

```
-- line differs --
index    : 1
expected : want-1
actual   : have-1
--
```

#### Partial matching

Partial matching can be enabled with the `--partial` option (`-p` for short).
When used, a match fails if the expected *substring* is not found in the matched line.

```bash
@test 'assert_line() partial matching' {
  run echo $'have 1\nhave 2\nhave 3'
  assert_line --partial 'want'
}
```

On failure, the same details are displayed as for literal matching, except that the substring replaces the expected line.

```
-- no output line contains substring --
substring : want
output (3 lines):
  have 1
  have 2
  have 3
--
```

This option and regular expression matching (`--regexp` or `-e`) are mutually exclusive.
An error is displayed when used simultaneously.

#### Regular expression matching

Regular expression matching can be enabled with the `--regexp` option (`-e` for short).
When used, a match fails if the *extended regular expression* does not match the line being tested.

> _**Note**:
> As expected, the anchors `^` and `$` bind to the beginning and the end of the matched line, respectively._

```bash
@test 'assert_line() regular expression matching' {
  run echo $'have-0\nhave-1\nhave-2'
  assert_line --index 1 --regexp '^want-[0-9]$'
}
```

On failure, the same details are displayed as for literal matching, except that the regular expression replaces the expected line.

```
-- regular expression does not match line --
index  : 1
regexp : ^want-[0-9]$
line   : have-1
--
```

An error is displayed if the specified extended regular expression is invalid.

This option and partial matching (`--partial` or `-p`) are mutually exclusive.
An error is displayed when used simultaneously.


### `refute_line`

Similarly to `refute_output`, this function helps to verify that a command or function produces the correct output.
It checks that the unexpected line does not appear in the output (default) or in a specific line of it.
Matching can be literal (default), partial or regular expression.
This function is the logical complement of `assert_line`.

> _**Warning**:
> Due to a [bug in Bats][bats-93], empty lines are discarded from `${lines[@]}`,
> causing line indices to change and preventing testing for empty lines._

[bats-93]: https://github.com/sstephenson/bats/pull/93

#### Looking for a line in the output

By default, the entire output is searched for the unexpected line.
The assertion fails if the unexpected line is found in `${lines[@]}`.

```bash
@test 'refute_line() looking for line' {
  run echo $'have-0\nwant\nhave-2'
  refute_line 'want'
}
```

On failure, the unexpected line, the index of its first match and the output with the matching line highlighted are displayed.

> _**Warning**:
> The output displayed does not contain empty lines.
> See the Warning above for more._

```
-- line should not be in output --
line  : want
index : 1
output (3 lines):
  have-0
> want
  have-2
--
```

If output is not longer than one line, it is displayed in *two-column* format.

#### Matching a specific line

When the `--index <idx>` option is used (`-n <idx>` for short), the unexpected line is matched only against the line identified by the given index.
The assertion fails if the unexpected line equals `${lines[<idx>]}`.

```bash
@test 'refute_line() specific line' {
  run echo $'have-0\nwant-1\nhave-2'
  refute_line --index 1 'want-1'
}
```

On failure, the index and the unexpected line are displayed.

```
-- line should differ --
index : 1
line  : want-1
--
```

#### Partial matching

Partial matching can be enabled with the `--partial` option (`-p` for short).
When used, a match fails if the unexpected *substring* is found in the matched line.

```bash
@test 'refute_line() partial matching' {
  run echo $'have 1\nwant 2\nhave 3'
  refute_line --partial 'want'
}
```

On failure, in addition to the details of literal matching, the substring is also displayed.
When used with `--index <idx>` the substring replaces the unexpected line.

```
-- no line should contain substring --
substring : want
index     : 1
output (3 lines):
  have 1
> want 2
  have 3
--
```

This option and regular expression matching (`--regexp` or `-e`) are mutually exclusive.
An error is displayed when used simultaneously.

#### Regular expression matching

Regular expression matching can be enabled with the `--regexp` option (`-e` for short).
When used, a match fails if the *extended regular expression* matches the line being tested.

> _**Note**:
> As expected, the anchors `^` and `$` bind to the beginning and the end of the matched line, respectively._

```bash
@test 'refute_line() regular expression matching' {
  run echo $'Foobar v0.1.0\nRelease date: 2015-11-29'
  refute_line --index 0 --regexp '^Foobar v[0-9]+\.[0-9]+\.[0-9]$'
}
```

On failure, in addition to the details of literal matching, the regular expression is also displayed.
When used with `--index <idx>` the regular expression replaces the unexpected line.

```
-- regular expression should not match line --
index  : 0
regexp : ^Foobar v[0-9]+\.[0-9]+\.[0-9]$
line   : Foobar v0.1.0
--
```

An error is displayed if the specified extended regular expression is invalid.

This option and partial matching (`--partial` or `-p`) are mutually exclusive.
An error is displayed when used simultaneously.

### `assert_regex`

This function is similar to `assert_equal` but uses pattern matching instead of
equality, by wrapping `[[ value =~ pattern ]]`.

Fail if the value (first parameter) does not match the pattern (second
parameter).

```bash
@test 'assert_regex()' {
  assert_regex 'what' 'x$'
}
```

On failure, the value and the pattern are displayed.

```
-- values does not match regular expression --
value    : what
pattern  : x$
--
```

If the value is longer than one line then it is displayed in *multi-line*
format.

An error is displayed if the specified extended regular expression is invalid.

For description of the matching behavior, refer to the documentation of the
`=~` operator in the [Bash manual][bash-conditional].

> _**Note**:
> the `BASH_REMATCH` array is available immediately after the assertion succeeds but is fragile;
> i.e. prone to being overwritten as a side effect of other actions._

### `refute_regex`

This function is similar to `refute_equal` but uses pattern matching instead of
equality, by wrapping `! [[ value =~ pattern ]]`.

Fail if the value (first parameter) matches the pattern (second parameter).

```bash
@test 'refute_regex()' {
  refute_regex 'WhatsApp' 'Threema'
}
```

On failure, the value, the pattern and the match are displayed.

```bash
@test 'refute_regex()' {
  refute_regex 'WhatsApp' 'What.'
}

-- value matches regular expression --
value    : WhatsApp
pattern  : What.
match    : Whats
case     : sensitive
--
```

If the value or pattern is longer than one line then it is displayed in
*multi-line* format.

An error is displayed if the specified extended regular expression is invalid.

For description of the matching behavior, refer to the documentation of the
`=~` operator in the [Bash manual][bash-conditional].

> _**Note**:
> the `BASH_REMATCH` array is available immediately after the assertion fails but is fragile;
> i.e. prone to being overwritten as a side effect of other actions like calling `run`.
> Thus, it's good practice to avoid using `BASH_REMATCH` in conjunction with `refute_regex()`.
> The valuable information the array contains is the matching part of the value which is printed in the failing test log, as mentioned above._

### `assert_stderr`

> _**Note**:
> `run` has to be called with `--separate-stderr` to separate stdout and stderr into `$output` and `$stderr`.
> If not, `$stderr` will be empty, causing `assert_stderr` to always fail.

Similarly to `assert_output`, this function verifies that a command or function produces the expected stderr.
The stderr matching can be literal (the default), partial or by regular expression.
The expected stderr can be specified either by positional argument or read from STDIN by passing the `-`/`--stdin` flag.

#### Literal matching

By default, literal matching is performed.
The assertion fails if `$stderr` does not equal the expected stderr.

  ```bash
  echo_err() {
    echo "$@" >&2
  }

  @test 'assert_stderr()' {
    run --separate-stderr echo_err 'have'
    assert_stderr 'want'
  }

  @test 'assert_stderr() with pipe' {
    run --separate-stderr echo_err 'hello'
    echo_err 'hello' | assert_stderr -
  }

  @test 'assert_stderr() with herestring' {
    run --separate-stderr echo_err 'hello'
    assert_stderr - <<< hello
  }
  ```

On failure, the expected and actual stderr are displayed.

  ```
  -- stderr differs --
  expected : want
  actual   : have
  --
  ```

#### Existence

To assert that any stderr exists at all, omit the `expected` argument.

  ```bash
  @test 'assert_stderr()' {
    run --separate-stderr echo_err 'have'
    assert_stderr
  }
  ```

On failure, an error message is displayed.

  ```
  -- no stderr --
  expected non-empty stderr, but stderr was empty
  --
  ```

#### Partial matching

Partial matching can be enabled with the `--partial` option (`-p` for short).
When used, the assertion fails if the expected _substring_ is not found in `$stderr`.

  ```bash
  @test 'assert_stderr() partial matching' {
    run --separate-stderr echo_err 'ERROR: no such file or directory'
    assert_stderr --partial 'SUCCESS'
  }
  ```

On failure, the substring and the stderr are displayed.

  ```
  -- stderr does not contain substring --
  substring : SUCCESS
  stderr    : ERROR: no such file or directory
  --
  ```

#### Regular expression matching

Regular expression matching can be enabled with the `--regexp` option (`-e` for short).
When used, the assertion fails if the *extended regular expression* does not match `$stderr`.

*Note: The anchors `^` and `$` bind to the beginning and the end (respectively) of the entire stderr; not individual lines.*

  ```bash
  @test 'assert_stderr() regular expression matching' {
    run --separate-stderr echo_err 'Foobar 0.1.0'
    assert_stderr --regexp '^Foobar v[0-9]+\.[0-9]+\.[0-9]$'
  }
  ```

On failure, the regular expression and the stderr are displayed.

  ```
  -- regular expression does not match stderr --
  regexp : ^Foobar v[0-9]+\.[0-9]+\.[0-9]$
  stderr : Foobar 0.1.0
  --
  ```

### `refute_stderr`

> _**Note**:
> `run` has to be called with `--separate-stderr` to separate stdout and stderr into `$output` and `$stderr`.
> If not, `$stderr` will be empty, causing `refute_stderr` to always pass.

Similar to `refute_output`, this function verifies that a command or function does not produce the unexpected stderr.
(It is the logical complement of `assert_stderr`.)
The stderr matching can be literal (the default), partial or by regular expression.
The unexpected stderr can be specified either by positional argument or read from STDIN by passing the `-`/`--stdin` flag.

### `assert_stderr_line`

> _**Note**:
> `run` has to be called with `--separate-stderr` to separate stdout and stderr into `$output` and `$stderr`.
> If not, `$stderr` will be empty, causing `assert_stderr_line` to always fail.

Similarly to `assert_stderr`, this function verifies that a command or function produces the expected stderr.
It checks that the expected line appears in the stderr (default) or at a specific line number.
Matching can be literal (default), partial or regular expression.
This function is the logical complement of `refute_stderr_line`.

#### Looking for a line in the stderr

By default, the entire stderr is searched for the expected line.
The assertion fails if the expected line is not found in `${stderr_lines[@]}`.

  ```bash
  echo_err() {
    echo "$@" >&2
  }

  @test 'assert_stderr_line() looking for line' {
    run --separate-stderr echo_err $'have-0\nhave-1\nhave-2'
    assert_stderr_line 'want'
  }
  ```

On failure, the expected line and the stderr are displayed.

  ```
  -- stderr does not contain line --
  line : want
  stderr (3 lines):
    have-0
    have-1
  have-2
  --
  ```

#### Matching a specific line

When the `--index <idx>` option is used (`-n <idx>` for short), the expected line is matched only against the line identified by the given index.
The assertion fails if the expected line does not equal `${stderr_lines[<idx>]}`.

  ```bash
  @test 'assert_stderr_line() specific line' {
    run --separate-stderr echo_err $'have-0\nhave-1\nhave-2'
    assert_stderr_line --index 1 'want-1'
  }
  ```

On failure, the index and the compared stderr_lines are displayed.

  ```
  -- line differs --
  index    : 1
  expected : want-1
  actual   : have-1
  --
  ```

#### Partial matching

Partial matching can be enabled with the `--partial` option (`-p` for short).
When used, a match fails if the expected *substring* is not found in the matched line.

  ```bash
  @test 'assert_stderr_line() partial matching' {
    run --separate-stderr echo_err $'have 1\nhave 2\nhave 3'
    assert_stderr_line --partial 'want'
  }
  ```

On failure, the same details are displayed as for literal matching, except that the substring replaces the expected line.

  ```
  -- no stderr line contains substring --
  substring : want
  stderr (3 lines):
    have 1
    have 2
    have 3
  --
  ```

#### Regular expression matching

Regular expression matching can be enabled with the `--regexp` option (`-e` for short).
When used, a match fails if the *extended regular expression* does not match the line being tested.

*Note: As expected, the anchors `^` and `$` bind to the beginning and the end (respectively) of the matched line.*

  ```bash
  @test 'assert_stderr_line() regular expression matching' {
    run --separate-stderr echo_err $'have-0\nhave-1\nhave-2'
    assert_stderr_line --index 1 --regexp '^want-[0-9]$'
  }
  ```

On failure, the same details are displayed as for literal matching, except that the regular expression replaces the expected line.

  ```
  -- regular expression does not match line --
  index  : 1
  regexp : ^want-[0-9]$
  line   : have-1
  --
  ```

### `refute_stderr_line`

> _**Note**:
> `run` has to be called with `--separate-stderr` to separate stdout and stderr into `$output` and `$stderr`.
> If not, `$stderr` will be empty, causing `refute_stderr_line` to always pass.

Similarly to `refute_stderr`, this function helps to verify that a command or function produces the correct stderr.
It checks that the unexpected line does not appear in the stderr (default) or in a specific line of it.
Matching can be literal (default), partial or regular expression.
This function is the logical complement of `assert_stderr_line`.

<!-- REFERENCES -->

[bats]: https://github.com/bats-core/bats-core
[bash-comp-cmd]: https://www.gnu.org/software/bash/manual/bash.html#Compound-Commands
[bash-conditional]: https://www.gnu.org/software/bash/manual/bash.html#Conditional-Constructs

[bats-docs]: https://bats-core.readthedocs.io/
[bats-support-output]: https://github.com/bats-core/bats-support#output-formatting
[bats-support]: https://github.com/bats-core/bats-support
