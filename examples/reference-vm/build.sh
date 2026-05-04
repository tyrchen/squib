#!/usr/bin/env bash
# Build a minimal aarch64 reference VM (kernel + initramfs) for squib's
# end-to-end demos and live-HVF integration tests.
#
# Inputs:
#   - examples/reference-vm/init                 (the userspace init script)
# Outputs:
#   - examples/reference-vm/build/Image          (Linux 5.10 / 6.x aarch64 vmlinux Image)
#   - examples/reference-vm/build/initramfs.cpio.gz
#
# Strategy:
#   - Kernel:   download Firecracker's reference aarch64 kernel from the
#               canonical S3 bucket. It's ~30 MB, virtio-MMIO + ext4 +
#               net + rng + console + initramfs, well-trodden in
#               microvm CI everywhere. (We do not vendor a pre-built
#               binary in git.)
#   - Busybox:  download a static aarch64 build from the Docker Library
#               busybox release bucket, OR build with Docker if the
#               download fails. ~1 MB.
#   - Init:     copy `init` from this directory.
#   - Pack:     `cpio -o -H newc | gzip -9` into initramfs.cpio.gz.
#
# Squib's runner consumes the kernel via `--kernel` and the initramfs
# via `--initrd`; the boot args carry the demo configuration.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
BUILD="${ROOT}/build"
mkdir -p "$BUILD"

KERNEL_VERSION="${KERNEL_VERSION:-6.1.141}"
KERNEL_URL="${KERNEL_URL:-https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.13/aarch64/vmlinux-${KERNEL_VERSION}}"
# Source of the static aarch64 busybox: Alpine Linux's
# `alpine-minirootfs` tarball. ~3 MB, ships a pre-built busybox at
# `/bin/busybox` plus the symlink farm we want anyway. macOS-friendly
# (just `tar`), no Docker required.
ALPINE_VERSION="${ALPINE_VERSION:-3.20.5}"
ALPINE_MAJOR="${ALPINE_VERSION%.*}"
ALPINE_URL="${ALPINE_URL:-https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_MAJOR}/releases/aarch64/alpine-minirootfs-${ALPINE_VERSION}-aarch64.tar.gz}"

echo "[1/4] kernel: ${KERNEL_URL}"
if [ ! -f "${BUILD}/Image" ]; then
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL -o "${BUILD}/Image" "$KERNEL_URL"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "${BUILD}/Image" "$KERNEL_URL"
    else
        echo "ERROR: need curl or wget to fetch the kernel" >&2
        exit 1
    fi
    echo "      downloaded $(du -h "${BUILD}/Image" | cut -f1)"
else
    echo "      reusing existing $(du -h "${BUILD}/Image" | cut -f1)"
fi

echo "[2/4] busybox-aarch64 + musl loader from Alpine minirootfs"
ALPINE_EXTRACT="${BUILD}/alpine-extract"
ALPINE_TARBALL="${BUILD}/alpine-minirootfs.tar.gz"
BUSYBOX_BIN="${ALPINE_EXTRACT}/bin/busybox"
if [ ! -f "$BUSYBOX_BIN" ]; then
    if [ ! -f "$ALPINE_TARBALL" ]; then
        echo "      downloading ${ALPINE_URL}"
        if command -v curl >/dev/null 2>&1; then
            curl -fsSL -o "$ALPINE_TARBALL" "$ALPINE_URL"
        else
            wget -q -O "$ALPINE_TARBALL" "$ALPINE_URL"
        fi
    fi
    # Extract bin/busybox + lib/* (musl dynamic loader + libc) — busybox
    # in Alpine is dynamically linked to /lib/ld-musl-aarch64.so.1; we
    # need to ship that into the initramfs or the kernel's exec_binprm
    # fails with -ENOENT after decompressing the cpio.
    rm -rf "$ALPINE_EXTRACT"
    mkdir -p "$ALPINE_EXTRACT"
    tar -xzf "$ALPINE_TARBALL" -C "$ALPINE_EXTRACT" ./bin/busybox ./lib
    chmod +x "$BUSYBOX_BIN"
    echo "      extracted $(du -h "$BUSYBOX_BIN" | cut -f1) busybox + $(du -sh "${ALPINE_EXTRACT}/lib" | cut -f1) libs"
else
    echo "      reusing existing $(du -h "$BUSYBOX_BIN" | cut -f1) busybox"
fi

echo "[3/4] staging initramfs root"
STAGING="${BUILD}/initramfs-root"
rm -rf "$STAGING"
mkdir -p "${STAGING}"/{bin,sbin,proc,sys,dev,tmp,etc,lib}

# Copy the musl dynamic loader + libc from Alpine into /lib so busybox
# can resolve its interpreter (ld-musl-aarch64.so.1) at exec time.
cp -a "${ALPINE_EXTRACT}/lib/." "${STAGING}/lib/"

# Place busybox + applet symlinks.
cp "$BUSYBOX_BIN" "${STAGING}/bin/busybox"
chmod +x "${STAGING}/bin/busybox"
# A small set of applets covers everything `init` needs.
for applet in sh mount umount cat echo sleep seq sync ip wget poweroff halt reboot ls cp rm mkdir; do
    ln -sf busybox "${STAGING}/bin/$applet"
done

# Copy the squib reference init.
cp "${ROOT}/init" "${STAGING}/init"
chmod +x "${STAGING}/init"

# /etc/resolv.conf — link-local only, no DNS needed.
cat > "${STAGING}/etc/resolv.conf" <<'EOF'
# Reference VM has no upstream DNS; MMDS is reached by literal IP.
EOF

echo "[4/4] packing initramfs.cpio.gz"
(
    cd "$STAGING"
    # `cpio -o -H newc` writes the standard newc format the kernel
    # accepts as initramfs. find . -print0 | cpio --null is the
    # canonical recipe for shipping a directory tree.
    find . -print0 | cpio --null -o -H newc 2>/dev/null
) | gzip -9n > "${BUILD}/initramfs.cpio.gz"

echo "      packed $(du -h "${BUILD}/initramfs.cpio.gz" | cut -f1)"
echo
echo "Done."
echo "  Kernel    : ${BUILD}/Image"
echo "  Initramfs : ${BUILD}/initramfs.cpio.gz"
echo
echo "Run with:"
echo "  cargo run --bin squib -- \\"
echo "    --no-api --config-file examples/reference-vm/config.json"
