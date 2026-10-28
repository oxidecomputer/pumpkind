#!/bin/bash
#:
#: name = "test"
#: variety = "basic"
#: target = "helios-2.0"
#: rust_toolchain = true
#:

set -o errexit
set -o pipefail
set -o xtrace

banner "clippy"
cargo clippy -- --deny warnings

banner "fmt"
cargo fmt -- --check

banner "test"
cargo test
