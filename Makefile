CARGO ?= cargo
ENTITLEMENTS := apps/squib/squib.entitlements
ENTITLEMENTS_BRIDGED := apps/squib/squib-bridged.entitlements
TARGET_DIR := $(shell $(CARGO) metadata --format-version 1 --no-deps | python3 -c 'import sys, json; print(json.load(sys.stdin)["target_directory"])')
SQUIB_BIN := $(TARGET_DIR)/aarch64-apple-darwin/release/squib

build:
	@$(CARGO) build --workspace --all-targets

build-release:
	@$(CARGO) build --release --bin squib

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
# CI uses a Developer ID.
sign: build-release
	codesign --entitlements $(ENTITLEMENTS) \
	         --options runtime \
	         --force \
	         --sign - \
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
	         --sign - \
	         $(SQUIB_BIN)

verify:
	codesign --display --entitlements - $(SQUIB_BIN)

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

# Notarize the signed binary. Requires APPLE_ID, APPLE_TEAM_ID, and an app-specific
# password in env (or use --keychain-profile if you've set one up).
notarize: sign
	xcrun notarytool submit --wait \
	  --apple-id $(APPLE_ID) \
	  --team-id $(APPLE_TEAM_ID) \
	  --password $(APPLE_NOTARY_PASSWORD) \
	  $(SQUIB_BIN)

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

.PHONY: build build-release test lint fmt fmt-check audit deny doc run sign sign-bridged verify hvf-test vmnet-test build-reference-vm demo notarize release update-submodule
