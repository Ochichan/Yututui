#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

checker=$PWD/scripts/check-unsafe-inventory.sh
temp_root=$(mktemp -d "${TMPDIR:-/tmp}/unsafe-inventory-test.XXXXXX")
trap 'rm -rf "$temp_root"' EXIT

tests_run=0

fail() {
  echo "not ok - $1" >&2
  exit 1
}

new_fixture() {
  tests_run=$((tests_run + 1))
  fixture=$temp_root/case-$tests_run
  mkdir -p "$fixture/scripts" "$fixture/src" "$fixture/crates/ratatui-image/src"
  cp "$checker" "$fixture/scripts/check-unsafe-inventory.sh"
}

run_checker() {
  fixture_path=$1
  checker_path=$2
  output_path=$fixture_path/stdout
  error_path=$fixture_path/stderr

  if PATH="$checker_path" "$BASH" "$fixture_path/scripts/check-unsafe-inventory.sh" \
    >"$output_path" 2>"$error_path"; then
    actual_status=0
  else
    actual_status=$?
  fi
}

assert_status() {
  expected_status=$1
  test_name=$2
  if [ "$actual_status" -ne "$expected_status" ]; then
    echo "stdout:" >&2
    cat "$output_path" >&2
    echo "stderr:" >&2
    cat "$error_path" >&2
    fail "$test_name: expected status $expected_status, got $actual_status"
  fi
}

assert_contains() {
  file=$1
  expected=$2
  test_name=$3
  if ! grep -Fq "$expected" "$file"; then
    cat "$file" >&2
    fail "$test_name: expected '$expected'"
  fi
}

assert_not_contains() {
  file=$1
  unexpected_text=$2
  test_name=$3
  if grep -Fq "$unexpected_text" "$file"; then
    cat "$file" >&2
    fail "$test_name: did not expect '$unexpected_text'"
  fi
}

make_minimal_path() {
  path_dir=$1
  mkdir -p "$path_dir"
  for command_name in dirname mktemp rm cat; do
    command_path=$(command -v "$command_name")
    ln -s "$command_path" "$path_dir/$command_name"
  done
}

real_path=$PATH

new_fixture
mkdir -p "$fixture/src/util"
printf '%s\n' 'pub unsafe fn allowed() {}' > "$fixture/src/util/runtime.rs"
run_checker "$fixture" "$real_path"
assert_status 0 "allowed unsafe match"
assert_contains "$output_path" "unsafe inventory ok" "allowed unsafe match"
if [ -s "$error_path" ]; then
  cat "$error_path" >&2
  fail "allowed unsafe match: expected empty stderr"
fi
echo "ok - allowed unsafe match"

new_fixture
printf '%s\n' 'pub unsafe fn unexpected() {}' > "$fixture/src/unexpected.rs"
run_checker "$fixture" "$real_path"
assert_status 1 "unexpected unsafe match"
assert_contains "$error_path" \
  "error: unsafe appears outside the reviewed path allowlist:" \
  "unexpected unsafe match"
assert_contains "$error_path" \
  "src/unexpected.rs:1:pub unsafe fn unexpected() {}" \
  "unexpected unsafe match"
assert_not_contains "$output_path" "unsafe inventory ok" "unexpected unsafe match"
echo "ok - unexpected unsafe match"

new_fixture
printf '%s\n' 'pub fn safe() {}' > "$fixture/src/safe.rs"
run_checker "$fixture" "$real_path"
assert_status 0 "no unsafe matches"
assert_contains "$output_path" "unsafe inventory ok" "no unsafe matches"
if [ -s "$error_path" ]; then
  cat "$error_path" >&2
  fail "no unsafe matches: expected empty stderr"
fi
echo "ok - no unsafe matches"

new_fixture
printf '%s\n' 'pub fn safe() {}' > "$fixture/src/safe.rs"
missing_rg_path=$fixture/bin
make_minimal_path "$missing_rg_path"
run_checker "$fixture" "$missing_rg_path"
assert_status 127 "missing rg"
assert_contains "$error_path" \
  "error: unsafe inventory search failed (rg exit status 127)" \
  "missing rg"
assert_not_contains "$output_path" "unsafe inventory ok" "missing rg"
echo "ok - missing rg"

new_fixture
printf '%s\n' 'pub fn safe() {}' > "$fixture/src/safe.rs"
rm -rf "$fixture/crates/ratatui-image/src"
run_checker "$fixture" "$real_path"
assert_status 2 "rg search error"
assert_contains "$error_path" \
  "error: unsafe inventory search failed (rg exit status 2)" \
  "rg search error"
assert_not_contains "$output_path" "unsafe inventory ok" "rg search error"
echo "ok - rg search error"

echo "unsafe inventory regression tests passed ($tests_run cases)"
