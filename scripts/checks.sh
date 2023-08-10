#!/bin/bash
set -e

# This script is used for running all the checks
# needed to make sure CI passes.

# See below for the reasoning behind testing
# clippy for all feature combinations.
# (Note: clippy reports compile errors too)

# You can pass extra args to cargo commands
#   ./scripts/check.sh --release
# will run in release mode. You can use that
# if you already have release mode compiled, as
# it will be faster to use the version you already compiled

TESTNAME=""
cleanup() {
    exit_code=$?
    if [[ ${exit_code} -ne 0 ]]; then
        echo -e "\n\n##### Failed on \"$TESTNAME\"" 1>&2
    fi
    exit $exit_code
}
trap cleanup EXIT

TESTNAME="Running cargo check"
echo "$TESTNAME"
cargo check $@ -q --all-features

TESTNAME="Running cargo fmt check"
echo "$TESTNAME"
cargo fmt $@ -q --all -- --check

# Testing all the combinations of clippy.
# There were many instances where a certain struct
# differed based on liquid or not(liquid) etc.
# and the clippy fixes would break the other
# feature combination.
#
# Be prepared to use #[allow(clippy::___)] attributes
# to "fix" contradictions between feature sets.

TESTNAME="Running cargo clippy check no features"
echo "$TESTNAME"
cargo clippy $@ -q

TESTNAME="Running cargo clippy check electrum-discovery"
echo "$TESTNAME"
cargo clippy $@ -q -F electrum-discovery

TESTNAME="Running cargo clippy check liquid"
echo "$TESTNAME"
cargo clippy $@ -q -F liquid

TESTNAME="Running cargo clippy check electrum-discovery + liquid"
echo "$TESTNAME"
cargo clippy $@ -q -F electrum-discovery,liquid

TESTNAME="Running cargo test with all features"
echo "$TESTNAME"
cargo test $@ -q --package electrs --lib --all-features
