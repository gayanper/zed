#!/bin/sh

if [ ! -d "./script" ]; then
  echo "This script must be run from the root of the repository."
  exit 1
fi

echo "nightly" > crates/zed/RELEASE_CHANNEL && \
  ./script/bundle-mac aarch64-apple-darwin && \
  git checkout crates/zed/RELEASE_CHANNEL && \
  echo; echo; echo "Complete"; echo; echo


