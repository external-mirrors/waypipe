#!/bin/sh

# This script is run by meson to make cargo build in a specific folder and produce a specific output file,
# because --artifact-dir is still nightly-only
set -e

if [ $# -ne 5 ] ; then
    echo "Incorrect number of arguments: $#"
    exit 1
fi
cargo build --frozen -v --profile "$1" --manifest-path "$2" --no-default-features --target-dir "$3" --features "$4"
cp "$3/$1/waypipe" "$5"
