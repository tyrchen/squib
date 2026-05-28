#!/usr/bin/env bash
# Publish Squib crates in dependency order.
#
# Re-running this script is safe: crate versions that already exist on
# crates.io are skipped. crates.io rate limits new crate creation, so 429
# responses are treated as retryable and the server-provided retry time is
# honored when Cargo includes one.

set -euo pipefail

DELAY="${PUBLISH_DELAY:-60}"
MAX_ATTEMPTS="${PUBLISH_MAX_ATTEMPTS:-8}"
RETRY_DELAY="${PUBLISH_RETRY_DELAY:-300}"
CARGO_BIN="${CARGO:-cargo}"

publish_args=()
if [[ "${PUBLISH_NO_VERIFY:-0}" == "1" ]]; then
    publish_args+=(--no-verify)
fi

if [[ -n "${CARGO_PUBLISH_FLAGS:-}" ]]; then
    # shellcheck disable=SC2206
    publish_args+=(${CARGO_PUBLISH_FLAGS})
fi

sleep_after_success() {
    if (( DELAY > 0 )); then
        echo "  Waiting ${DELAY}s before next publish..."
        sleep "${DELAY}"
    fi
}

retry_seconds_from_output() {
    local output="$1"
    local retry_at retry_epoch now_epoch

    retry_at="$(printf '%s\n' "$output" | sed -n 's/.*try again after \(.* GMT\).*/\1/p' | tail -n 1)"
    if [[ -z "$retry_at" ]]; then
        return 1
    fi

    retry_epoch="$(date -j -f '%a, %d %b %Y %H:%M:%S %Z' "$retry_at" '+%s' 2>/dev/null \
        || date -u -d "$retry_at" '+%s' 2>/dev/null \
        || true)"
    if [[ -z "$retry_epoch" ]]; then
        return 1
    fi

    now_epoch="$(date -u '+%s')"
    if (( retry_epoch <= now_epoch )); then
        printf '5\n'
    else
        printf '%s\n' "$((retry_epoch - now_epoch + 5))"
    fi
}

is_already_published() {
    local output="$1"

    printf '%s\n' "$output" | grep -Eiq \
        'already exists|already uploaded|already been uploaded|is already published|crate version .* is already uploaded'
}

is_retryable_publish_failure() {
    local output="$1"

    printf '%s\n' "$output" | grep -Eiq \
        'no matching package named|failed to select a version|status (code )?429|Too Many Requests|rate limit|too many new crates|try again after'
}

publish_crate() {
    local crate_name="$1"
    local attempt=1
    local output wait_seconds

    while true; do
        echo "=== Publishing ${crate_name} (attempt ${attempt}/${MAX_ATTEMPTS}) ==="
        if (( ${#publish_args[@]} > 0 )); then
            output="$("${CARGO_BIN}" publish -p "${crate_name}" "${publish_args[@]}" 2>&1)" && {
                echo "$output"
                echo "  ${crate_name} published successfully"
                sleep_after_success
                return
            }
        elif output="$("${CARGO_BIN}" publish -p "${crate_name}" 2>&1)"; then
            echo "$output"
            echo "  ${crate_name} published successfully"
            sleep_after_success
            return
        fi

        if is_already_published "$output"; then
            echo "$output"
            echo "  ${crate_name} already published, skipping"
            return
        fi

        if (( attempt < MAX_ATTEMPTS )) && is_retryable_publish_failure "$output"; then
            echo "$output"
            wait_seconds="$(retry_seconds_from_output "$output" || printf '%s\n' "$RETRY_DELAY")"
            echo "  Retryable publish failure; waiting ${wait_seconds}s before retry..."
            sleep "$wait_seconds"
            attempt="$((attempt + 1))"
            continue
        fi

        echo "$output"
        echo "  ${crate_name} failed"
        exit 1
    done
}

publish_group() {
    local group_name="$1"
    shift

    echo ""
    echo "Publishing ${group_name}..."
    for crate in "$@"; do
        publish_crate "$crate"
    done
}

# Layer 0: crates with no Squib workspace dependencies.
BASE_CRATES=(
    squib-bus
    squib-core
    squib-jail
)

# Layer 1: direct consumers of squib-core.
CORE_CONSUMER_CRATES=(
    squib-api
    squib-arch
    squib-mmds
)

# Layer 2: architecture/device primitives.
DEVICE_PRIMITIVE_CRATES=(
    squib-fdt
    squib-gic
    squib-loader
    squib-snapshot
)

# Layer 3: crates that need GIC, snapshot, or MMDS primitives.
DEVICE_CRATES=(
    squib-host
    squib-legacy
    squib-virtio
)

# Layer 4: runtime backends and networking.
RUNTIME_CRATES=(
    squib-hv
    squib-net
)

# Layer 5: VMM orchestration.
VMM_CRATES=(
    squib-vmm
)

# Layer 6: umbrella library.
UMBRELLA_CRATES=(
    squib
)

# Layer 7: applications that depend on the umbrella crate.
APP_CRATES=(
    squib-cli
)

publish_group "base crates" "${BASE_CRATES[@]}"
publish_group "core consumer crates" "${CORE_CONSUMER_CRATES[@]}"
publish_group "device primitive crates" "${DEVICE_PRIMITIVE_CRATES[@]}"
publish_group "device crates" "${DEVICE_CRATES[@]}"
publish_group "runtime crates" "${RUNTIME_CRATES[@]}"
publish_group "VMM crates" "${VMM_CRATES[@]}"
publish_group "umbrella crates" "${UMBRELLA_CRATES[@]}"
publish_group "application crates" "${APP_CRATES[@]}"

echo ""
echo "=== All publishable Squib crates are published. ==="
