CARGO ?= cargo
ENTITLEMENTS := apps/squib/squib.entitlements
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

# Codesign the release binary with the hypervisor + vmnet entitlements. Ad-hoc identity
# (`-`) is fine for local dev; CI uses a Developer ID. See
# docs/research/hvf-prior-art-deep-dive.md §7 and aarch64-hvf-guest-stack.md §9.4.
sign: build-release
	codesign --entitlements $(ENTITLEMENTS) \
	         --options runtime \
	         --force \
	         --sign - \
	         $(SQUIB_BIN)

verify:
	codesign --display --entitlements - $(SQUIB_BIN)

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

.PHONY: build build-release test lint fmt fmt-check audit deny doc run sign verify notarize release update-submodule
