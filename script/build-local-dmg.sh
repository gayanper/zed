#!/bin/sh
# Fastest local release DMG for same-machine consumption.
# Uses `bundle-mac -l`: no LTO, no debug-symbol extraction/strip, no sentry
# upload, no remote_server build, cached git binary, fast ULFO DMG without a
# license agreement or notarization.
#
# The release channel is passed via ZED_RELEASE_CHANNEL (default: nightly)
# so crates/zed/RELEASE_CHANNEL is never rewritten: touching that file bumps
# its mtime and invalidates cargo's cache for the release_channel crate and
# everything downstream, even when the content is identical.
set -eu

if [ ! -d "./script" ]; then
  echo "This script must be run from the root of the repository."
  exit 1
fi

ZED_RELEASE_CHANNEL="${ZED_RELEASE_CHANNEL:-nightly}"
export ZED_RELEASE_CHANNEL

./script/bundle-mac -l "$@"
status=$?

echo; echo; echo "Complete"; echo; echo
exit $status
