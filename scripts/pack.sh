#!/usr/bin/env bash
# Builds the Windows client and, when UPX is available, also writes a packed
# copy that is roughly a third of the size.
#
#   scripts/pack.sh                 # uses `upx` from PATH
#   UPX_BIN=/path/to/upx.exe scripts/pack.sh
#
# The plain build is always written to release/tabssh.exe; the packed one to
# release/tabssh-packed.exe.  UPX-packed executables are far smaller but their
# UPX signature is sometimes flagged by antivirus heuristics, so the unpacked
# build is kept alongside.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

cargo build --release -p tabssh-client
mkdir -p release
cp -f target/release/tabssh.exe release/tabssh.exe

upx="${UPX_BIN:-upx}"
if command -v "$upx" >/dev/null 2>&1; then
    cp -f target/release/tabssh.exe release/tabssh-packed.exe
    # --best --lzma is the smallest setting UPX offers.
    "$upx" --best --lzma release/tabssh-packed.exe
    echo
    echo "built:"
    ls -l release/tabssh.exe release/tabssh-packed.exe
else
    echo "upx not found on PATH — wrote the uncompressed release/tabssh.exe only." >&2
    echo "install UPX (https://upx.github.io) or set UPX_BIN=/path/to/upx.exe to pack." >&2
    ls -l release/tabssh.exe
fi
