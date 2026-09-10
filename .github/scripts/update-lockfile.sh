#!/bin/sh
set -eu

# Run from the Cargo workspace root.  Keep the nightly confined to dependency resolution.
test -f Cargo.lock || {
    echo 'Cargo.lock not found' >&2
    exit 1
}

exec rustup run nightly-2026-09-10 cargo update \
    --config 'registry.global-min-publish-age="3 days"' \
    --config 'resolver.incompatible-publish-age="deny"'
