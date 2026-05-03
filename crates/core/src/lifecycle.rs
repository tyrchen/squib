//! VM lifecycle phases — internal six-state enum.
//!
//! The internal phase enum is richer than the upstream three-value wire vocabulary so
//! handlers can produce precise `fault_message`s on misordered requests. The wire form
//! is a separate type owned by `squib-api`; this enum never crosses the HTTP boundary.
//!
//! See [11-runtime-core.md §
//! 3.1](../../../specs/11-runtime-core.md#31-internal-lifecyclephase-vs-wire-vmstate).

use core::fmt;

/// The wire-shape vocabulary served by `GET /` — exactly the upstream three values.
///
/// Serializes to the literal upstream strings (`"Not started"`, `"Running"`, `"Paused"`)
/// — note the space + lowercase `s` in `"Not started"`. SDKs and `firectl` sniff these
/// strings; squib emits them verbatim.
///
/// `WireVmState` lives here (not in `squib-api`) so the `LifecyclePhase::wire_state`
/// collapse function can be defined alongside its target without `squib-core` taking a
/// dependency on the API crate.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
pub enum WireVmState {
    /// VMM has started but no microvm is running. Wire string: `"Not started"`.
    #[default]
    NotStarted,
    /// Microvm has booted and at least one vCPU is active. Wire string: `"Running"`.
    Running,
    /// Microvm has booted but vCPUs are paused. Wire string: `"Paused"`.
    Paused,
}

impl fmt::Display for WireVmState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotStarted => f.write_str("Not started"),
            Self::Running => f.write_str("Running"),
            Self::Paused => f.write_str("Paused"),
        }
    }
}

/// Internal lifecycle phase — six values. Never serialized to the wire.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Hash)]
pub enum LifecyclePhase {
    /// No configuration posted yet.
    #[default]
    Uninitialized,
    /// Configuration posted, awaiting `PUT /actions {InstanceStart}`.
    NotStarted,
    /// Boot orchestration in progress (vCPUs spawning, GIC live, devices wired).
    Starting,
    /// At least one vCPU is active.
    Running,
    /// Microvm booted but vCPUs are paused.
    Paused,
    /// Terminal state after PSCI `SYSTEM_OFF` / `SYSTEM_RESET` or fatal vCPU panic.
    Shutdown,
}

impl LifecyclePhase {
    /// Collapse to the upstream three-value vocabulary served by `GET /`.
    ///
    /// `Uninitialized | NotStarted | Starting | Shutdown` all map to
    /// [`WireVmState::NotStarted`] — clients see only the upstream three values.
    #[must_use]
    pub const fn wire_state(self) -> WireVmState {
        match self {
            Self::Uninitialized | Self::NotStarted | Self::Starting | Self::Shutdown => {
                WireVmState::NotStarted
            }
            Self::Running => WireVmState::Running,
            Self::Paused => WireVmState::Paused,
        }
    }

    /// `true` once the microvm has booted (Running or Paused).
    #[must_use]
    pub const fn is_post_boot(self) -> bool {
        matches!(self, Self::Running | Self::Paused)
    }

    /// `true` while the VMM is still accepting pre-boot configuration mutations.
    #[must_use]
    pub const fn is_pre_boot(self) -> bool {
        matches!(self, Self::Uninitialized | Self::NotStarted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_state_collapses_per_spec_11_3_1() {
        assert_eq!(
            LifecyclePhase::Uninitialized.wire_state(),
            WireVmState::NotStarted
        );
        assert_eq!(
            LifecyclePhase::NotStarted.wire_state(),
            WireVmState::NotStarted
        );
        assert_eq!(
            LifecyclePhase::Starting.wire_state(),
            WireVmState::NotStarted
        );
        assert_eq!(
            LifecyclePhase::Shutdown.wire_state(),
            WireVmState::NotStarted
        );
        assert_eq!(LifecyclePhase::Running.wire_state(), WireVmState::Running);
        assert_eq!(LifecyclePhase::Paused.wire_state(), WireVmState::Paused);
    }

    #[test]
    fn wire_state_displays_upstream_strings_verbatim() {
        assert_eq!(WireVmState::NotStarted.to_string(), "Not started");
        assert_eq!(WireVmState::Running.to_string(), "Running");
        assert_eq!(WireVmState::Paused.to_string(), "Paused");
    }

    #[test]
    fn pre_post_boot_predicates() {
        assert!(LifecyclePhase::Uninitialized.is_pre_boot());
        assert!(LifecyclePhase::NotStarted.is_pre_boot());
        assert!(!LifecyclePhase::Starting.is_pre_boot());
        assert!(LifecyclePhase::Running.is_post_boot());
        assert!(LifecyclePhase::Paused.is_post_boot());
        assert!(!LifecyclePhase::Shutdown.is_post_boot());
    }
}
