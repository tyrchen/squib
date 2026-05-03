---
title: 40-jailer — squib-jail Darwin shim with Firecracker jailer flag set
type: design
status: draft
last_updated: 2026-05-03
depends_on: 00-prd.md, 70-security.md
---

# 40 · Jailer — `squib-jail` Darwin shim

Status: draft · Owner: apps/squib-jail · Depends on: [00-prd.md](./00-prd.md), [70-security.md](./70-security.md)

## 1. Purpose

Provide a drop-in replacement for the Linux `jailer` binary so launchers (firecracker-go-sdk, firecracker-containerd, ignite) can call `squib-jail` with the same flags they call `jailer` and get a usable Darwin sandbox. The principle: never break a launcher.

`squib-jail` is a separate binary (`apps/squib-jail`) so the privilege-drop work happens before `squib` itself runs.

## 2. Flag surface

Same flags as upstream `jailer`. Behaviour split per platform:

### 2.1 Genuinely supported on Darwin

| Flag | Behaviour |
|------|-----------|
| `--id <str>` | Instance ID; used in chroot path naming |
| `--exec-file <path>` | Path to `squib`; copied into the chroot |
| `--uid <u32>` | `setuid` after privilege bring-up |
| `--gid <u32>` | `setgid` after privilege bring-up |
| `--chroot-base-dir <path>` | Default `/srv/jailer` (Darwin-equivalent override via `SQUIB_JAIL_BASE`) |
| `--daemonize` | Genuine: `setsid` + redirect 0/1/2 to `/dev/null` |
| `--resource-limits <kvs>` | `setrlimit` with the documented keys |

### 2.2 Accept-and-warn (no Darwin equivalent)

| Flag | Behaviour |
|------|-----------|
| `--cgroup <kv>`, `--parent-cgroup <path>`, `--cgroup-version <1\|2>` | Accept-and-warn (no cgroups on Darwin) |
| `--netns <path>` | Accept-and-warn (no Linux netns on Darwin) |
| `--new-pid-ns` | Accept-and-warn (no PID namespaces on Darwin); `posix_spawn` for signal lineage decoupling |

### 2.3 Squib extensions

| Flag | Behaviour |
|------|-----------|
| `--macos-sandbox-profile <name>` | Apply a bundled `sandbox_init` profile by name |

Bundled profile names (in `apps/squib-jail/profiles/`):

- `default` — deny network egress, allow vmnet socket, allow `/srv/jailer/<id>` r/w.
- `permissive` — close to no sandboxing; for debugging.

## 3. Sequence

```text
1. Parse flags via clap (same shape as upstream).
2. Resolve chroot path: <chroot_base_dir>/firecracker/<id>/root.
3. mkdir -p the chroot; mount squib binary inside (cp).
4. setrlimit(...) per --resource-limits.
5. If --daemonize: setsid, redirect fds.
6. chroot(<path>) via Darwin chroot(2).
7. setgid(--gid), setuid(--uid).
8. If --macos-sandbox-profile: sandbox_init(profile_data, 0, &errbuf).
9. execve("/squib", remaining_argv).
10. Exit codes match upstream jailer.
```

`chroot(2)` exists on Darwin and is sufficient for filesystem-scope confinement; it is not a security boundary against root, matching upstream jailer's posture (jailer is defence-in-depth, not isolation).

## 4. Behaviour edges

- **Already-chrooted**: if invoked recursively, the inner `chroot` fails; we exit with the upstream-equivalent error.
- **Missing entitlement**: `squib` inside the chroot still needs the codesigned binary's entitlements. Copying preserves codesigning when the source is a Mach-O bundle; we explicitly verify this on every start.
- **`posix_spawn` for `--new-pid-ns`**: not a real PID namespace, just a clean signal lineage. Documented in `docs/api-deviations.md`.
- **No seccomp**: `--seccomp-filter` and `--no-seccomp` are accepted-and-warned at the **squib** binary level (see [21-api-compat-matrix.md § 3](./21-api-compat-matrix.md#3-cli-flag-compatibility)), not in the jailer. The jailer does not parse them.

## 5. Invariants

| # | Invariant | Pinned by |
|---|-----------|-----------|
| I-JAIL-1 | Every upstream `jailer` flag parses without error. | Compat suite |
| I-JAIL-2 | Exit codes match upstream `jailer` for every documented error. | Per-error CI test |
| I-JAIL-3 | The squib binary in the chroot retains its codesigned entitlements. | `codesign -dvvv` check in CI after `squib-jail` runs |
| I-JAIL-4 | `--macos-sandbox-profile` failures are surfaced as a non-zero exit code with a documented message. | Test with a malformed profile |

## 6. Cross-references

- ← Depends on: [00-prd.md](./00-prd.md), [70-security.md](./70-security.md)
- → Consumed by: [50-cli.md](./50-cli.md), [72-testing-strategy.md](./72-testing-strategy.md)
- ↔ Related research: [docs/research/firecracker-subsystems.md § Jailer](../docs/research/firecracker-subsystems.md)
