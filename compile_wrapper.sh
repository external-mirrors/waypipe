#!/bin/sh

# This script is run by meson to make cargo build in a specific folder and produce a specific output file,
# because --artifact-dir is still nightly-only
set -e

if [ $# -ne 5 ] ; then
    echo "Incorrect number of arguments: $#"
    exit 1
fi

# This is a workaround for Rust having no simple and stable compile
# time conditional string concatenation; and meson not properly
# handling newlines or backslashes in custom targets
version="$WAYPIPE_VERSION
features:
  lz4: $WAYPIPE_FEATURE_LZ4
  zstd: $WAYPIPE_FEATURE_ZSTD
  dmabuf: $WAYPIPE_FEATURE_DMABUF
  video: $WAYPIPE_FEATURE_VIDEO"

env WAYPIPE_VERSION="$version" cargo build --frozen -v --profile "$1" --manifest-path "$2" --no-default-features --target-dir "$3" --features "$4"
cp "$3/$1/waypipe" "$5"
