#!/usr/bin/env bash
# Build the native module and deploy it where require('matchup_rs') finds it.
#
#   ./build.sh              # release build -> lua/matchup_rs.so (or .dll)
#   PROFILE=debug ./build.sh
#
# cargo names the cdylib lib<name>.so / lib<name>.dylib on unix and
# <name>.dll on windows; nvim's package.cpath loads it as lua/matchup_rs.so
# (unix) or lua/matchup_rs.dll (windows).
set -euo pipefail
cd "$(dirname "$0")"

PROFILE=${PROFILE:-release}
if [ "$PROFILE" = "release" ]; then
  cargo build --release
  target_dir=target/release
else
  cargo build
  target_dir=target/debug
fi

mkdir -p lua

case "$(uname -s)" in
  Linux)
    src="$target_dir/libmatchup_rs.so"
    dest="lua/matchup_rs.so"
    ;;
  Darwin)
    src="$target_dir/libmatchup_rs.dylib"
    dest="lua/matchup_rs.so"
    ;;
  MINGW* | MSYS* | CYGWIN*)
    src="$target_dir/matchup_rs.dll"
    dest="lua/matchup_rs.dll"
    ;;
  *)
    echo "build.sh: unsupported platform '$(uname -s)'" >&2
    exit 1
    ;;
esac

if [ ! -f "$src" ]; then
  echo "build.sh: expected artifact not found: $src" >&2
  exit 1
fi

cp -f "$src" "$dest"
echo "build.sh: $src -> $dest"
