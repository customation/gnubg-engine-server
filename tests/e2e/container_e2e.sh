#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# Runs inside rust:1-trixie with the customation workspace at /work —
# the prebuilt libgnubgapi.so from the gnubgapi repo's runtimes dir was
# linked against glibc 2.38, so bookworm (2.36) cannot load it. Data
# (weights, bearoff DBs, MET) also comes from the gnubgapi repo.
set -eu

GNUBGAPI=/work/gnubgapi
SERVER=/work/gnubg-engine-server
LIB=$GNUBGAPI/runtimes/linux-x64/native/libgnubgapi.so

if [ ! -f "$LIB" ]; then
    echo "prebuilt libgnubgapi.so missing at $LIB — build it with gnubgapi/native/build.sh" >&2
    exit 2
fi

echo "== building gnubg-engine-server =="
cd "$SERVER"
export CARGO_TARGET_DIR=/work/gnubg-engine-server/target-linux
cargo build --release 2>&1 | tail -3

echo "== running E2E =="
python3 "$SERVER/tests/e2e/run_e2e.py" \
    "$CARGO_TARGET_DIR/release/gnubg-engine-server" \
    "$LIB" \
    "$GNUBGAPI/data"
