#!/bin/bash
# Build a stapleable `.pkg` installer carrying signed `squib` and
# `squib-jail` binaries. Implements the soft-requirement S4 from
# `specs/00-prd.md` § 9.
#
# The .pkg installs:
#   /usr/local/bin/squib                       (signed, hardened runtime)
#   /usr/local/bin/squib-jail                  (signed, hardened runtime)
#   /usr/local/share/squib/squib.entitlements
#   /usr/local/share/squib/squib-jail.entitlements
#
# Inputs (env, with documented defaults):
#   SIGN_ID       Codesign identity for the final productbuild signature.
#                 Defaults to `-` (ad-hoc). For a notarizable .pkg this
#                 must be a valid `Developer ID Installer: ...` identity.
#   VERSION       Package version; defaults to the workspace version
#                 inferred via `cargo metadata`.
#   PKG_OUT       Output path for the final .pkg.
#                 Defaults to `target/aarch64-apple-darwin/release/squib-<VERSION>.pkg`.
#
# This script is invoked from `make pkg`. The Makefile target ensures the
# release binaries are signed first via `make sign-all`.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

CARGO=${CARGO:-cargo}
SIGN_ID=${SIGN_ID:--}
# `cargo metadata` is the source of truth for both the package version and the
# target directory. We pipe through `jq`, which the rest of the project's CI
# (Makefile `hvf-test`, `vmnet-test`) already requires — keeps the .pkg builder
# free of a Python dependency that some CI runners ship without.
VERSION=${VERSION:-$($CARGO metadata --format-version 1 --no-deps \
  | jq -r '.packages[0].version')}

TARGET_DIR=$($CARGO metadata --format-version 1 --no-deps \
  | jq -r '.target_directory')
RELEASE_DIR="${TARGET_DIR}/aarch64-apple-darwin/release"
PKG_OUT=${PKG_OUT:-"${RELEASE_DIR}/squib-${VERSION}.pkg"}

SQUIB_BIN="${RELEASE_DIR}/squib"
SQUIB_JAIL_BIN="${RELEASE_DIR}/squib-jail"

if [[ ! -f "$SQUIB_BIN" ]] || [[ ! -f "$SQUIB_JAIL_BIN" ]]; then
  echo "error: release binaries not found; run 'make sign-all' first" >&2
  exit 1
fi

# Use two separate temp dirs so the intermediate component .pkg never sits
# inside the staging tree (otherwise pkgbuild includes it in the payload —
# subtle bug that surfaces as "./squib-component.pkg" inside the final
# .pkg's `pkgutil --payload-files`).
WORK_DIR="$(mktemp -d)"
PKG_ROOT="${WORK_DIR}/root"
trap 'rm -rf "$WORK_DIR"' EXIT

mkdir -p "$PKG_ROOT/usr/local/bin"
mkdir -p "$PKG_ROOT/usr/local/share/squib"
mkdir -p "$PKG_ROOT/usr/local/libexec/squib"

cp "$SQUIB_BIN" "$PKG_ROOT/usr/local/bin/squib"
cp "$SQUIB_JAIL_BIN" "$PKG_ROOT/usr/local/bin/squib-jail"
cp apps/squib/squib.entitlements "$PKG_ROOT/usr/local/share/squib/"
cp apps/squib/squib-bridged.entitlements "$PKG_ROOT/usr/local/share/squib/"

# Stage the bundled gvproxy binary if it has been fetched + verified
# (`make vendor-gvproxy`). The path is prescribed by
# `specs/30-networking.md` § 4 (`<install-prefix>/libexec/squib/gvproxy`).
# Soft-fail on a missing binary so the installer still ships when the
# operator hasn't pinned a release yet — the warning surfaces in the
# operator-visible build log so it's never silent.
if [[ -f vendors/gvproxy/bin/gvproxy ]]; then
    cp vendors/gvproxy/bin/gvproxy "$PKG_ROOT/usr/local/libexec/squib/gvproxy"
    chmod 0755 "$PKG_ROOT/usr/local/libexec/squib/gvproxy"
    if [[ -f vendors/gvproxy/bin/LICENSE ]]; then
        mkdir -p "$PKG_ROOT/usr/local/share/squib/licenses"
        cp vendors/gvproxy/bin/LICENSE \
           "$PKG_ROOT/usr/local/share/squib/licenses/gvproxy.LICENSE"
    fi
else
    echo "warning: vendors/gvproxy/bin/gvproxy missing — .pkg will not include" >&2
    echo "         userspace networking. Run 'make vendor-gvproxy' first to" >&2
    echo "         pin a release per vendors/gvproxy/MANIFEST.toml." >&2
fi

# Reset perms — pkgbuild bakes mode bits into the .pkg payload.
chmod 0755 "$PKG_ROOT/usr/local/bin/squib"
chmod 0755 "$PKG_ROOT/usr/local/bin/squib-jail"
chmod 0644 "$PKG_ROOT/usr/local/share/squib/"*.entitlements

COMPONENT_PKG="${WORK_DIR}/squib-component.pkg"

pkgbuild \
  --root "$PKG_ROOT" \
  --identifier com.tyrchen.squib \
  --version "$VERSION" \
  --install-location / \
  "$COMPONENT_PKG"

# productbuild wraps the component .pkg into a distributable installer.
# Unlike codesign, productbuild does not accept the ad-hoc `-` identity:
# omitting `--sign` produces an unsigned .pkg that installs cleanly on
# `installer(8)` but cannot be notarized. Releases must override
# `SIGN_ID` to a real `Developer ID Installer:` identity.
if [[ "$SIGN_ID" == "-" ]]; then
  echo "warning: SIGN_ID=-; producing UNSIGNED .pkg (not notarizable)" >&2
  productbuild \
    --package "$COMPONENT_PKG" \
    --identifier com.tyrchen.squib.installer \
    --version "$VERSION" \
    "$PKG_OUT"
else
  productbuild \
    --package "$COMPONENT_PKG" \
    --identifier com.tyrchen.squib.installer \
    --version "$VERSION" \
    --sign "$SIGN_ID" \
    "$PKG_OUT"
fi

echo "Built ${PKG_OUT}"
