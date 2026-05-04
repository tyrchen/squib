# Documentation index

Operator-facing documentation for **squib**.

| Doc | Audience |
|-----|----------|
| [api-deviations.md](./api-deviations.md) | SDK authors, orchestrator integrators |
| [macos-setup.md](./macos-setup.md) | First-time installers, release engineers |
| [perf/index.md](./perf/index.md) | Performance regression triage |
| [perf/boot-tuning.md](./perf/boot-tuning.md) | Boot-time tuning levers |
| [research/index.md](./research/index.md) | Engineers reading the prior-art memos |

For specifications (PRD, component design, decision log) see the top-level
[`specs/`](../specs/index.md). For the test surface see
[`tests/firecracker-compat/`](../tests/firecracker-compat).

## Reading order

1. **Start here**: [`README.md`](../README.md).
2. **Install**: [`docs/macos-setup.md`](./macos-setup.md).
3. **Wire-shape parity**: [`docs/api-deviations.md`](./api-deviations.md) →
   [`specs/21-api-compat-matrix.md`](../specs/21-api-compat-matrix.md).
4. **Performance**: [`docs/perf/index.md`](./perf/index.md).
5. **Spec deep dive**: [`specs/index.md`](../specs/index.md).
