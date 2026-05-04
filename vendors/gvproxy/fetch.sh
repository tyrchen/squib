#!/bin/bash
# Fetch the gvproxy binary and license file pinned in `MANIFEST.toml`,
# verify their SHA-256 against the manifest, and install into
# `vendors/gvproxy/bin/`. Idempotent — skips the network when the
# expected files already exist with the right hash.
#
# The Makefile target `vendor-gvproxy` wraps this script. The .pkg
# builder (`dist/pkg/build-pkg.sh`) and the Homebrew formula
# (`dist/homebrew/squib.rb`) both consume `vendors/gvproxy/bin/gvproxy`
# verbatim; if it isn't present at distribution-build time, the build
# emits a warning and ships without userspace networking (squib still
# works in `--network=shared` / `--network=host` / `--network=bridged`,
# just without the gvproxy fallback).
#
# Per `specs/70-security.md` § 10 (Supply chain), the SHA-256 pin is the
# only thing that prevents an upstream tag-move attack from sneaking a
# different binary into the bundled installer.

set -euo pipefail

cd "$(dirname "$0")"
HERE="$(pwd)"

# Hand-rolled TOML parser in awk (the Makefile already requires `jq`, but
# pulling a TOML parser is one more dependency than this script needs).
get() {
    local key="$1"
    awk -F' = ' "
        /^${key} = / {
            gsub(/\"/, \"\", \$2);
            print \$2;
            exit
        }
    " MANIFEST.toml
}

VERSION=$(get version)
URL=$(get url)
SHA=$(get sha256)
LICENSE_URL=$(get license_url)
LICENSE_SHA=$(get license_sha256)

if [[ -z "$VERSION" || -z "$URL" || -z "$SHA" ]]; then
    echo "vendors/gvproxy/MANIFEST.toml is malformed (missing version/url/sha256)" >&2
    exit 1
fi

if [[ "$SHA" == "PLACEHOLDER_PIN_AT_RELEASE_PREP_TIME" ]]; then
    echo "warning: gvproxy SHA-256 pin is the placeholder; vendor-gvproxy is a no-op" >&2
    echo "  → bump vendors/gvproxy/MANIFEST.toml with a real release pin." >&2
    exit 0
fi

mkdir -p bin

# Helper: compute SHA-256 portably (macOS shasum + Linux sha256sum).
sha256_of() {
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        sha256sum "$1" | awk '{print $1}'
    fi
}

# Skip the network if the binary is already present and matches the pin.
if [[ -f bin/gvproxy ]]; then
    have=$(sha256_of bin/gvproxy)
    if [[ "$have" == "$SHA" ]]; then
        echo "vendors/gvproxy/bin/gvproxy already pinned at $VERSION ($SHA)"
        exit 0
    fi
    echo "warning: existing bin/gvproxy hash $have ≠ pinned $SHA; refetching" >&2
    rm -f bin/gvproxy
fi

echo "fetching gvproxy v$VERSION from $URL"
curl --fail --silent --show-error --location --output bin/gvproxy.tmp "$URL"
got=$(sha256_of bin/gvproxy.tmp)
if [[ "$got" != "$SHA" ]]; then
    rm -f bin/gvproxy.tmp
    echo "FAILED: SHA-256 mismatch — pinned $SHA but got $got" >&2
    echo "  Either upstream moved the tag (supply-chain attack — refuse)" >&2
    echo "  or the manifest pin needs a deliberate bump." >&2
    exit 1
fi
chmod 0755 bin/gvproxy.tmp
mv bin/gvproxy.tmp bin/gvproxy

if [[ -n "$LICENSE_URL" ]]; then
    echo "fetching gvproxy LICENSE from $LICENSE_URL"
    curl --fail --silent --show-error --location --output bin/LICENSE.tmp "$LICENSE_URL"
    got=$(sha256_of bin/LICENSE.tmp)
    if [[ "$LICENSE_SHA" != "PLACEHOLDER_PIN_AT_RELEASE_PREP_TIME" && "$got" != "$LICENSE_SHA" ]]; then
        rm -f bin/LICENSE.tmp
        echo "FAILED: LICENSE SHA-256 mismatch — pinned $LICENSE_SHA but got $got" >&2
        exit 1
    fi
    mv bin/LICENSE.tmp bin/LICENSE
fi

echo "vendors/gvproxy/bin/gvproxy installed (sha256 $SHA) at $HERE/bin/gvproxy"
