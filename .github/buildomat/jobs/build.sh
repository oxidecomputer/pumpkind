#!/bin/bash
#:
#: name = "illumos"
#: variety = "basic"
#: target = "helios-2.0"
#: rust_toolchain = true
#: output_rules = [
#:   "/out/*",
#: ]
#
#: [[publish]]
#: series = "image"
#: name = "pumpkind-gz.tar"
#: from_output = "/out/pumpkind-gz.tar"
#:
#: [[publish]]
#: series = "image"
#: name = "pumpkind-gz.sha256.txt"
#: from_output = "/out/pumpkind-gz.sha256.txt"
#
#: [[publish]]
#: series = "image"
#: name = "pumpkind.tar.gz"
#: from_output = "/out/pumpkind.tar.gz"
#:
#: [[publish]]
#: series = "image"
#: name = "pumpkind.sha256.txt"
#: from_output = "/out/pumpkind.sha256.txt"
#:

set -o errexit
set -o pipefail
set -o xtrace

# Copy file from our local working directory into the /out directory, where
# buildomat can retrieve for archiving.
# usage: archive <source stem> <dest stem> <suffix>
function archive {
    mv out/$1$2 /out/$1$2
    digest -a sha256 /out/$1$2 > /out/$1.sha256.txt
}

pfexec mkdir -p /out
pfexec chown "$UID" /out

banner build
ptime -m cargo build --release --verbose

banner package
ptime -m cargo xtask dist --release --format switch
ptime -m cargo xtask dist --release --format global

banner archive
archive pumpkind-gz .tar
archive pumpkind .tar.gz
