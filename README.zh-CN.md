# squib

跑在 Apple Silicon 上的 microVM 监视器，对外讲的是 Firecracker 的 HTTP API。
JSON 形态、UDS 接口、连那个 `Server: Firecracker API` 响应头都和上游一致 ——
但底层换成了 HVF 加 `vmnet.framework`，不再依赖 KVM 和 Linux 的 TAP。

- **仅 Apple Silicon**、**仅 HVF**（不接 VZ.framework）、**仅 aarch64 Linux guest**。
- 不出 0.x 的预览版，单一 1.0 直接对齐 Firecracker 全部能力。
- 快照：Full + Diff（靠 `hv_vm_protect` 追踪脏页），加 Mach 异常驱动的 postcopy。
- 网络：`--network={shared|bridged|host|userspace}` —— 有 entitlement 时走 vmnet，没有就退到 gvproxy。

English: [README.md](./README.md)。

## 状态

> 1.0 之前。Phase 7（"compat suite + perf + 抛光"）正在跑，在线启动路径
> 末尾（vCPU 线程 + kernel image + PL011 接线）记在
> [`specs/93-improvements-review.md`](./specs/93-improvements-review.md)
> 的 "Phase 1（end of Phase 1.6 落地）"。

## 快速开始

```bash
# 本地编译并 ad-hoc 签名（HVF 需要 com.apple.security.hypervisor）。
make sign

# 起一个空的 squib，把 Firecracker API 暴露在 UDS 上。
target/release/squib --api-sock /tmp/squib.sock

# 用 curl、上游 getting-started.md 的请求序列、或者任何认 Server: Firecracker API 的 SDK 来驱动它。
curl --unix-socket /tmp/squib.sock http://localhost/version
```

跑一台真正的 Linux guest：

```bash
make build-reference-vm    # 把 vmlinux + alpine busybox 拉到 examples/reference-vm/build/
make demo                   # 签好名的 HVF 端到端测试
```

## 文档

- **使用指南**：[`docs/user-guide.zh-CN.md`](./docs/user-guide.zh-CN.md) ——
  装、跑、驱动、网络、MMDS、快照、排错。
- **开发指南**：[`docs/dev-guide.zh-CN.md`](./docs/dev-guide.zh-CN.md) ——
  workspace 布局、build/test/lint、HVF 测试签名、加功能怎么落到对应 spec。
- **设计文档**：[`specs/`](./specs/index.md) —— PRD、数据模型、运行时
  核心、HVF 后端、设备、MMDS、快照、网络、jailer、CLI。
- **研究备忘**：[`docs/research/`](./docs/research/index.md) —— 现有方案
  深挖、HVF / aarch64 / vmnet 笔记。
- **API 差异**：[`docs/api-deviations.md`](./docs/api-deviations.md) ——
  compat matrix 里每条 P/A/R 行的 curl 复现。
- **macOS 配置**：[`docs/macos-setup.md`](./docs/macos-setup.md) ——
  entitlement、签名、gvproxy、网络模式取舍。
- **性能数字**：[`docs/perf/`](./docs/perf/index.md) —— 每个 revision 自家
  实测，不抄上游。

## 兼容性

按 [`specs/21-api-compat-matrix.md`](./specs/21-api-compat-matrix.md)，上游
Firecracker 的每个端点、字段、CLI flag 都标了状态：**F**（完全对齐）、
**P**（形态对齐，语义不同）、**A**（接收下来空操作并 warn 一次）、**R**
（拒绝并给一个稳定的 `fault_message`）。compat suite 在
[`tests/firecracker-compat/`](./tests/firecracker-compat/) 里逐行验证。

## License

Apache-2.0，详见 [LICENSE.md](./LICENSE.md)。

Copyright 2025 Tyr Chen.
