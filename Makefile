CARGO ?= cargo
ENTITLEMENTS := apps/squib/squib.entitlements
ENTITLEMENTS_BRIDGED := apps/squib/squib-bridged.entitlements
# `cargo metadata` → `jq` instead of `python3`; jq is already a hard requirement
# for `make hvf-test` / `make vmnet-test`, so we get one less interpreter on
# the critical path.
TARGET_DIR := $(shell $(CARGO) metadata --format-version 1 --no-deps | jq -r '.target_directory')
SQUIB_BIN := $(TARGET_DIR)/aarch64-apple-darwin/release/squib
SQUIB_JAIL_BIN := $(TARGET_DIR)/aarch64-apple-darwin/release/squib-jail

# Codesign identity. Default ad-hoc (`-`) is fine for local development and
# CI smoke; releases override `SIGN_ID` to a Developer ID hash. Both signed
# variants run with hardened runtime (`--options runtime`) per
# specs/70-security.md § 9 / R11.
SIGN_ID ?= -

build:
	@$(CARGO) build --workspace --all-targets

build-release:
	@$(CARGO) build --release --bin squib --bin squib-jail

test:
	@$(CARGO) test --workspace --all-features

lint:
	@$(CARGO) clippy --workspace --all-targets --all-features -- -D warnings

fmt:
	@$(CARGO) +nightly fmt --all

fmt-check:
	@$(CARGO) +nightly fmt --all -- --check

audit:
	@$(CARGO) audit

deny:
	@$(CARGO) deny check

doc:
	@$(CARGO) doc --workspace --no-deps

run:
	@$(CARGO) run --bin squib --

# Codesign the release binary with the default entitlements (hypervisor only).
# Per D17 / 70-security.md §9, only the bridged-mode binary carries
# `com.apple.vm.networking`; the default build sticks to the self-claimable
# `com.apple.security.hypervisor`. Ad-hoc identity (`-`) is fine for local dev;
# CI uses a Developer ID via `SIGN_ID=<hash> make sign`.
sign: build-release
	codesign --entitlements $(ENTITLEMENTS) \
	         --options runtime \
	         --force \
	         --sign $(SIGN_ID) \
	         $(SQUIB_BIN)

# Codesign with the bridged entitlements (adds `com.apple.vm.networking`).
# Used for the separately-signed build that enables `--network=bridged`. Requires
# the restricted form of the entitlement; ad-hoc signing here is for development
# only — releases of this variant must be signed with a Developer ID that holds
# the restricted entitlement.
sign-bridged: build-release
	codesign --entitlements $(ENTITLEMENTS_BRIDGED) \
	         --options runtime \
	         --force \
	         --sign $(SIGN_ID) \
	         $(SQUIB_BIN)

# Codesign squib-jail with hardened runtime but no entitlements. The jailer
# is libc-only (chroot+setuid+execv) and needs no Apple entitlement; the
# `--entitlements` flag is omitted deliberately so `codesign -dvvv` reports
# no entitlements bound — older codesign toolchains reject `<dict></dict>`
# plists with "invalid or unsupported format for entitlements", and the
# absence of the flag is the cleaner contract anyway.
sign-jail: build-release
	codesign --options runtime \
	         --force \
	         --sign $(SIGN_ID) \
	         $(SQUIB_JAIL_BIN)

# Sign both the squib and squib-jail release binaries with the default
# entitlements. Use as the entry-point for distribution builds (the .pkg and
# Homebrew bottle pipelines below depend on this).
sign-all: sign sign-jail

# `verify` covers both release binaries. The umbrella keeps `make sign-all
# && make verify` symmetric — a release pipeline that signs two binaries
# must verify two. Individual targets remain available for finer-grained
# CI steps.
verify: verify-squib verify-jail

verify-squib:
	codesign --display --entitlements - $(SQUIB_BIN)
	codesign --verify --strict --verbose=2 $(SQUIB_BIN)

# Verify squib-jail's signature. `--entitlements -` prints the embedded
# plist to stdout; for the jailer the body is empty (no entitlements bound)
# which is the contract the launcher integrity checks rely on.
verify-jail:
	codesign --display --entitlements - $(SQUIB_JAIL_BIN)
	codesign --verify --strict --verbose=2 $(SQUIB_JAIL_BIN)

# I-JAIL-3 boundary check: copying the squib binary into the chroot
# preserves its codesignature byte-for-byte. Implements the
# "codesign -dvvv check in CI after squib-jail runs" requirement from
# `specs/40-jailer.md` § 5.
verify-jail-preserves-entitlements: sign
	@tmpdir=$$(mktemp -d) && \
	  cp $(SQUIB_BIN) $$tmpdir/squib && \
	  codesign --display --entitlements - $(SQUIB_BIN) 2>&1 | grep -v '^Executable=' > $$tmpdir/before.txt && \
	  codesign --display --entitlements - $$tmpdir/squib 2>&1 | grep -v '^Executable=' > $$tmpdir/after.txt && \
	  echo "--- before copy ---" && cat $$tmpdir/before.txt && \
	  echo "--- after copy ---" && cat $$tmpdir/after.txt && \
	  diff -u $$tmpdir/before.txt $$tmpdir/after.txt && \
	  codesign --verify --strict --verbose=2 $$tmpdir/squib && \
	  rm -rf $$tmpdir && \
	  echo "I-JAIL-3 holds: codesign survived fs::copy"

# Build, ad-hoc sign, and run the squib-hv live HVF integration tests.
#
# `cargo test` does not codesign test binaries, but HVF refuses to initialise without
# `com.apple.security.hypervisor`. The pattern below mirrors what the applevisor crate
# does in its own Makefile: build with `--no-run`, codesign the produced test binaries,
# then re-invoke `cargo test` (which re-uses the signed binaries because nothing
# changed). Requires `jq`.
hvf-test:
	@$(CARGO) test -p squib-hv -p squib-vmm --tests --no-run --quiet
	@for bin in $$($(CARGO) test -p squib-hv -p squib-vmm --tests --no-run --message-format=json 2>/dev/null \
	                | jq -r 'select(.profile.test == true) | .filenames[]'); do \
	    echo "signing $$bin"; \
	    codesign --sign - --entitlements $(ENTITLEMENTS) --deep --force $$bin; \
	done
	@$(CARGO) test -p squib-hv -p squib-vmm --tests -- --nocapture --include-ignored

# Build, ad-hoc sign, and run the squib-net live FFI tests against
# vmnet.framework. Same pattern as hvf-test: cargo test does not codesign
# test binaries, but vmnet won't even invoke our callback without the
# `com.apple.security.hypervisor` entitlement on the binary. We sign post-
# build then re-invoke `cargo test`. Requires `jq`.
vmnet-test:
	@$(CARGO) test -p squib-net --tests --no-run --quiet
	@for bin in $$($(CARGO) test -p squib-net --tests --no-run --message-format=json 2>/dev/null \
	                | jq -r 'select(.profile.test == true) | .filenames[]'); do \
	    echo "signing $$bin"; \
	    codesign --sign - --entitlements $(ENTITLEMENTS) --deep --force $$bin; \
	done
	@$(CARGO) test -p squib-net --tests -- --nocapture --include-ignored

# End-to-end smoke for the Phase 5 snapshot subsystem.
# Live cross-filesystem rejection test for I-SNAP-3. Mounts a `hdiutil`
# RAM disk and asserts that `save` rejects when the destination crosses
# filesystems. The test is `#[ignore]`'d in plain `cargo test` because it
# needs hdiutil + an ephemeral mount; this target opts in.
snapshot-cross-fs-test:
	@$(CARGO) test -p squib-snapshot --test integration -- --ignored \
	  cross_filesystem_save_rejects_when_dest_is_on_a_separate_ramdisk

# Generates a real <id>.snap + <id>.mem pair under /tmp via the live save
# pipeline (bitcode envelope, CRC64 trailer, atomic temp-file + fsync + rename),
# then exercises the squib binary's `--describe-snapshot` flag against it.
# A second pass corrupts the trailing CRC byte and asserts that describe still
# prints the metadata, reports `crc_ok: NO`, and exits with code 2.
snapshot-smoke:
	@rm -f /tmp/squib_smoke.snap /tmp/squib_smoke.mem /tmp/squib_smoke_corrupt.snap
	@$(CARGO) run --quiet --example produce_demo_pair --package squib-snapshot -- /tmp/squib_smoke
	@echo "--- describe (clean) ---"
	@$(CARGO) run --quiet -p squib -- --describe-snapshot /tmp/squib_smoke.snap
	@cp /tmp/squib_smoke.snap /tmp/squib_smoke_corrupt.snap
	@python3 -c "p='/tmp/squib_smoke_corrupt.snap'; b=bytearray(open(p,'rb').read()); b[-1]^=0x01; open(p,'wb').write(bytes(b))"
	@echo "--- describe (corrupt CRC; expect exit=2) ---"
	@$(CARGO) run --quiet -p squib -- --describe-snapshot /tmp/squib_smoke_corrupt.snap; \
	    rc=$$?; if [ $$rc -ne 2 ]; then echo "FAIL: corrupt describe returned $$rc, expected 2"; exit 1; fi
	@echo "--- snapshot-version ---"
	@$(CARGO) run --quiet -p squib -- --snapshot-version
	@echo "snapshot-smoke: ok"

# Build a stapleable installer .pkg carrying signed squib + squib-jail.
# Drives `dist/pkg/build-pkg.sh`; sign-all gates the binaries first.
# To produce a notarizable .pkg, set SIGN_ID to a Developer ID Installer
# identity hash before invoking; the default ad-hoc identity makes the
# .pkg installable but not notarizable.
PKG_OUT := $(TARGET_DIR)/aarch64-apple-darwin/release/squib-$(shell $(CARGO) metadata --format-version 1 --no-deps | jq -r '.packages[0].version').pkg

pkg: sign-all
	@SIGN_ID=$(SIGN_ID) ./dist/pkg/build-pkg.sh

# Notarize the signed installer .pkg. Implements the Phase 6 exit criterion
# from `specs/91-impl-plan.md` § 9: "make notarize produces a stapleable
# .pkg that installs and runs on a fresh Apple Silicon Mac."
#
# Sequence:
#   1. Build, sign, and assemble the .pkg via `make pkg`.
#   2. Submit to Apple's notary service via xcrun notarytool. The --wait
#      flag blocks until the notarytool verdict is in (or until the CI
#      job-level timeout fires, whichever comes first — the impl-plan
#      risk row "Notarytool stalls release" is mitigated by running this
#      target on a post-merge async lane decoupled from tag creation;
#      see .github/workflows/notarize.yml).
#   3. Staple the notarization ticket onto the .pkg so it's installable
#      offline (Gatekeeper check works without re-contacting Apple).
#
# Requires APPLE_ID, APPLE_TEAM_ID, and APPLE_NOTARY_PASSWORD (an
# app-specific password) in the environment. Releases must also override
# SIGN_ID to a Developer ID Installer identity; the default ad-hoc `-`
# produces an unsigned .pkg that notarytool will reject.
notarize: pkg
	xcrun notarytool submit --wait \
	  --apple-id $(APPLE_ID) \
	  --team-id $(APPLE_TEAM_ID) \
	  --password $(APPLE_NOTARY_PASSWORD) \
	  $(PKG_OUT)
	xcrun stapler staple $(PKG_OUT)
	xcrun stapler validate $(PKG_OUT)

# Print the path of the Homebrew formula. The formula is HEAD-only until
# the first tagged release; after that this target should be extended to
# update the `url` + `sha256` fields automatically.
homebrew-formula:
	@echo "Homebrew formula at: dist/homebrew/squib.rb"
	@echo "Install with: brew install --HEAD --build-from-source ./dist/homebrew/squib.rb"

release:
	@$(CARGO) release tag --execute
	@git cliff -o CHANGELOG.md
	@git commit -a -n -m "Update CHANGELOG.md" || true
	@git push origin master
	@$(CARGO) release push --execute

update-submodule:
	@git submodule update --init --recursive --remote

# Build the reference VM (kernel + initramfs) one-time. Downloads the
# Firecracker reference aarch64 vmlinux + a static busybox; produces
# examples/reference-vm/build/{Image,initramfs.cpio.gz}. Skip if the
# artifacts already exist.
build-reference-vm:
	@./examples/reference-vm/build.sh

# End-to-end Linux boot demo: boots the reference VM under HVF, the
# init script hits MMDS at 169.254.169.254, the response is captured
# from PL011 and asserted by the test.
#
# Requires `make build-reference-vm` to have been run, plus the HVF
# entitlement (codesigned automatically below).
demo: build-reference-vm
	@$(CARGO) test -p squib-vmm --test linux_boot_smoke --no-run --quiet
	@for bin in $$($(CARGO) test -p squib-vmm --test linux_boot_smoke --no-run --message-format=json 2>/dev/null \
	                | jq -r 'select(.profile.test == true) | .filenames[]'); do \
	    echo "signing $$bin"; \
	    codesign --sign - --entitlements $(ENTITLEMENTS) --deep --force $$bin; \
	done
	@$(CARGO) test -p squib-vmm --test linux_boot_smoke -- \
	    --nocapture --include-ignored test_reference_vm_boots_linux_and_curls_mmds

# Run the Firecracker compatibility suite. Top-level integration tests live in
# `tests/firecracker-compat/`; each `tests/*.rs` exercises one row in
# `specs/21-api-compat-matrix.md` against the real `squib-api` axum router on a
# per-test UDS. No HVF / vmnet entitlement is required because the suite verifies
# *wire-shape* parity against a stub VMM event loop.
compat-test:
	@$(CARGO) test -p firecracker-compat --tests

# Run the criterion bench harness and stash the JSON output under
# `docs/perf/<git-sha>/<bench>.json`. Per `specs/71-performance-budgets.md` § 7
# the published numbers live in `docs/perf/`; this target is the pipe.
#
# `cargo bench` writes `target/criterion/<bench>/new/estimates.json` (and the HTML
# report) per axis. The Phase 1 skeleton bench in `crates/vmm/benches/boot.rs`
# requires the `bench` feature flag.
PERF_OUT := docs/perf/$(shell git rev-parse --short HEAD 2>/dev/null || echo unstaged)
BENCH_FLAGS := --warm-up-time 1 --measurement-time 3 --sample-size 10
bench-publish:
	@mkdir -p $(PERF_OUT)
	@$(CARGO) bench --features bench -p squib-vmm      --bench boot          -- $(BENCH_FLAGS) || true
	@$(CARGO) bench --features bench -p squib-snapshot --bench dirty_bitmap  -- $(BENCH_FLAGS) || true
	@criterion_dir="$$($(CARGO) metadata --format-version 1 --no-deps | jq -r '.target_directory')/criterion"; \
	  if [ -d "$$criterion_dir" ]; then \
	    cp -R "$$criterion_dir"/* $(PERF_OUT)/ 2>/dev/null || true; \
	    echo "perf numbers published under $(PERF_OUT)"; \
	  else \
	    echo "no criterion output produced at $$criterion_dir (bench may have failed)"; exit 1; \
	  fi

# Soak harness for orchestrator SDKs (firectl, firecracker-go-sdk, firecracker-
# containerd). Per `specs/91-impl-plan.md` § 10 Phase 7.4 each external SDK is a
# checked-in `tools/soak/<sdk>/run.sh` that spins up squib on a per-test UDS,
# drives the SDK against it, and reports pass/fail. This umbrella target probes
# each SDK's runner and skips with a one-line note when the binary is missing.
SOAK_RUNNERS := tools/soak/firectl/run.sh tools/soak/firecracker-go-sdk/run.sh tools/soak/firecracker-containerd/run.sh
soak:
	@fail=0; \
	for runner in $(SOAK_RUNNERS); do \
	  if [ -x "$$runner" ]; then \
	    echo "=== $$runner ==="; \
	    if ! $$runner; then \
	      echo "[$$runner] FAILED"; \
	      fail=1; \
	    fi; \
	  else \
	    echo "[$$runner] SKIP (not installed; see tools/soak/README.md)"; \
	  fi; \
	done; \
	exit $$fail

.PHONY: build build-release test lint fmt fmt-check audit deny doc run sign sign-bridged sign-jail sign-all verify verify-squib verify-jail verify-jail-preserves-entitlements hvf-test vmnet-test snapshot-smoke build-reference-vm demo notarize release update-submodule pkg homebrew-formula compat-test bench-publish soak
