# Documentation index

Operator- and contributor-facing documentation for **squib**.

| Doc | Audience | 中文 |
|-----|----------|------|
| [user-guide.md](./user-guide.md) | Operators driving squib | [user-guide.zh-CN.md](./user-guide.zh-CN.md) |
| [dev-guide.md](./dev-guide.md) | Contributors hacking on squib | [dev-guide.zh-CN.md](./dev-guide.zh-CN.md) |
| [api-deviations.md](./api-deviations.md) | SDK authors, orchestrator integrators | — |
| [macos-setup.md](./macos-setup.md) | First-time installers, release engineers | — |
| [perf/index.md](./perf/index.md) | Performance regression triage | — |
| [perf/boot-tuning.md](./perf/boot-tuning.md) | Boot-time tuning levers | — |
| [research/index.md](./research/index.md) | Engineers reading the prior-art memos | — |

For specifications (PRD, component design, decision log) see the top-level
[`specs/`](../specs/index.md). For the test surface see
[`tests/firecracker-compat/`](../tests/firecracker-compat).

## Reading order

1. **Start here**: [`README.md`](../README.md).
2. **As an operator**: [`user-guide.md`](./user-guide.md) →
   [`macos-setup.md`](./macos-setup.md) →
   [`api-deviations.md`](./api-deviations.md).
3. **As a contributor**: [`dev-guide.md`](./dev-guide.md) →
   [`specs/index.md`](../specs/index.md) →
   [`specs/91-impl-plan.md`](../specs/91-impl-plan.md).
4. **Performance triage**: [`perf/index.md`](./perf/index.md).
5. **Spec deep dive**: [`specs/index.md`](../specs/index.md).
