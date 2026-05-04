# frozen_string_literal: true
#
# Homebrew formula for squib — macOS-native microVM monitor with a
# Firecracker-compatible API. Implements the soft requirement S4 from
# `specs/00-prd.md` § 9 (Homebrew formula alongside direct `.pkg` download).
#
# Pre-1.0 layout: HEAD-only. The formula clones master, builds via cargo,
# ad-hoc signs both binaries with the squib + squib-jail entitlements (so
# `com.apple.security.hypervisor` is granted to a self-built squib), and
# installs them under `bin`.
#
# After the first tagged release, add a `url` + `sha256` for the release
# tarball and gate the HEAD path under `head do`.
#
# Usage:
#   brew install --HEAD --build-from-source ./dist/homebrew/squib.rb
class Squib < Formula
  desc "macOS-native microVM monitor with a Firecracker-compatible API"
  homepage "https://github.com/tyrchen/squib"
  license "Apache-2.0"
  head "https://github.com/tyrchen/squib.git", branch: "master"

  depends_on :macos
  depends_on macos: :sequoia # macOS 15+ required for hv_gic_*. See R9.
  depends_on arch: :arm64    # Apple Silicon only. See R10.
  depends_on "rust" => :build

  def install
    # Build both release binaries. The workspace is configured for
    # `aarch64-apple-darwin` via `.cargo/config.toml`, so the standard
    # cargo invocation hits the right target. `--locked` enforces that
    # `Cargo.lock` (committed) is the only resolution graph used at
    # install time; per `specs/70-security.md` § 10 (Supply chain) it's
    # the only thing that prevents a yanked transitive crate sneaking
    # in between formula publish and the user's brew install.
    system "cargo", "build", "--release", "--locked",
           "--bin", "squib", "--bin", "squib-jail"

    bin.install "target/aarch64-apple-darwin/release/squib"
    bin.install "target/aarch64-apple-darwin/release/squib-jail"

    # Ship the entitlement plists alongside the binaries so a brew user
    # can re-sign with their own Developer ID if required.
    (share/"squib").install "apps/squib/squib.entitlements"
    (share/"squib").install "apps/squib/squib-bridged.entitlements"
    (share/"squib").install "apps/squib-jail/squib-jail.entitlements"

    # Ad-hoc codesign so a freshly-built binary can request HVF on first
    # run. Users who want the notarized release should grab the .pkg from
    # the GitHub Releases page instead.
    system "codesign", "--entitlements", share/"squib"/"squib.entitlements",
           "--options", "runtime", "--force", "--sign", "-",
           bin/"squib"
    system "codesign", "--entitlements", share/"squib"/"squib-jail.entitlements",
           "--options", "runtime", "--force", "--sign", "-",
           bin/"squib-jail"
  end

  def caveats
    <<~EOS
      squib has been ad-hoc codesigned with `com.apple.security.hypervisor`.
      For bridged-mode networking (`--network=bridged`) you need a build
      signed with `com.apple.vm.networking` (restricted entitlement; see
      `specs/30-networking.md` § 5). Run:

        make sign-bridged   # in the squib source tree

      after rebuilding with `--features bridged`. The notarized .pkg
      shipped on the GitHub Releases page already does this for the
      `squib-bridged` variant.
    EOS
  end

  test do
    # `--snapshot-version` is the cheapest "did the binary load?" smoke
    # — it never opens a UDS, never touches HVF, and exits 0 on success.
    assert_match(/^v?\d+/, shell_output("#{bin}/squib --snapshot-version"))
    # squib-jail exits non-zero with --version because clap also wants
    # the required flags; the `--help` path is the parse-only smoke.
    assert_match "squib-jail", shell_output("#{bin}/squib-jail --help")
  end
end
