# 开发指南

想动手改 squib，先读这篇。这里讲 workspace 怎么布的、build/test/lint 三件套
怎么跑、HVF 测试为什么要给二进制签名才能跑起来、加新功能时该往哪儿插桩。
默认你已经看过 [使用指南](./user-guide.md)，本机能跑 `make sign`。

English: [dev-guide.md](./dev-guide.md)。

## 1. 心智模型

squib 是单个 Rust workspace：一个主二进制（`squib`）、一个辅助二进制
（`squib-jail`）、外加按职责拆出来的十几个 lib crate。crate 图是严格的
DAG —— 最底下是零依赖的 `squib-core`，最顶上是 `apps/squib-cli`。

两条铁律：

1. **`unsafe` 只能出现在两个 crate 里**：`squib-hv`（applevisor / `hv_*`
   边界）和 `squib-net::sys`（手写的 `vmnet` FFI）。其他 crate 一律
   `#![forbid(unsafe_code)]`，CI 会 grep 兜底。
2. **specs 是事实唯一来源。** 每个有分量的设计选择都在
   [`specs/99-key-decisions.md`](../specs/99-key-decisions.md) 里有对应的
   D-record。每条 wire 形态、错误字符串、CLI flag 都在
   [`specs/21-api-compat-matrix.md`](../specs/21-api-compat-matrix.md) 里有
   一行。代码和 spec 对不上时只改一边 —— 不要两边都改，更不要两边都不改。

## 2. Workspace 布局

```
apps/
  squib/           主 CLI 二进制，签 com.apple.security.hypervisor
  squib-jail/      jailer 替身，纯 libc，没有任何 Apple entitlement

crates/
  core             squib-core      可移植类型 + trait，零依赖
  api              squib-api       axum-on-UDS 的 Firecracker API + JSON 配置加载
  hv               squib-hv        通过 applevisor 和 HVF 打交道（unsafe 边界 #1）
  arch             squib-arch      aarch64 内存布局、vCPU 初值、sysreg、ESR_EL2、PSCI
  fdt              squib-fdt       基于 vm-fdt 的 FDT 构造器
  loader           squib-loader    kernel 加载器：Image / Image.gz / Image.zst / PE
  bus              squib-bus       MMIO 总线 + BusDevice trait
  virtio           squib-virtio    virtio-MMIO 传输层 + 各设备子 crate
  gic              squib-gic       in-kernel GICv3 封装（hv_gic_*）
  mmds             squib-mmds      移植过来的 dumbo + mmds 包拦截
  net              squib-net       vmnet 集成 + gvproxy 嵌入（unsafe 边界 #2）
  snapshot         squib-snapshot  bitcode + serde 状态文件，稀疏内存，postcopy
  vmm              squib-vmm       VMM 主体：builder、vCPU 线程、设备管理、事件循环
  host             squib-host      Mach 异常 pager、信号处理、子进程管理
  legacy           squib-legacy    PL011、RTC、boot timer

tests/
  firecracker-compat              一行 21-api-compat-matrix 对应一个文件

examples/
  reference-vm                    busybox-on-initramfs 的端到端 demo
```

每个 crate 详细职责和完整依赖图在
[`specs/61-crates-and-features.md`](../specs/61-crates-and-features.md)。

## 3. 编译、测试、Lint

全部走 Makefile，你不需要记 shell 脚本。

```bash
make build          # cargo build --workspace --all-targets
make build-release  # cargo build --release --bin squib --bin squib-jail
make test           # cargo test --workspace --all-features（仅快测试）
make lint           # cargo clippy --workspace --all-targets --all-features -- -D warnings
make fmt            # cargo +nightly fmt --all
make fmt-check      # CI 用的 check-only 版本
make audit          # cargo audit
make deny           # cargo deny check
make doc            # cargo doc --workspace --no-deps
```

发 PR 之前至少跑过 `make lint && make fmt-check && make test`。CI 还会再
跑一遍 `make compat-test`，外加在自托管的 Apple Silicon runner 上跑 live
HVF / vmnet 那两套。

clippy 默认开 `pedantic`（warn）+ `-D warnings`。边界模块（`squib-api`、
controller dispatch）再叠加一层 `unwrap_used` / `expect_used` /
`indexing_slicing` / `panic` 的 deny。如果你在非 `#[cfg(test)]` 代码里想
写 `.unwrap()`，停下来 —— 改 `?` 或者显式 match。

## 4. HVF 测试要签名

HVF 不签名根本初始化不了，它认 `com.apple.security.hypervisor`。
`cargo test` **不会** 自动给 test 二进制签名，所以但凡直接调 HVF 或者
`vmnet.framework` 的测试在没签名前都会挂。套路是这样的：

```bash
make hvf-test       # 先 --no-run 编出来，再逐个签名，最后 --include-ignored 重跑
make vmnet-test     # 给 vmnet FFI 测试用的同型套路
make demo           # 端到端跑参考 VM
```

那些 live HVF / vmnet 测试上的 `#[ignore]` 是故意的：贡献者还没跑过
`make sign` 的时候，普通 `cargo test` 也得能过。上面三个 Makefile target
签完名再翻 `--include-ignored`。

CI 出 release：`SIGN_ID=<DeveloperID-hash> make sign-all` 出带 hardened
runtime 的二进制，`make pkg` + `make notarize` 打包并送公证。完整流程见
[`macos-setup.md`](./macos-setup.md)。

## 5. trait 主干

要往 hypervisor 这层加东西，入口都在 `squib-core`：

- `HypervisorBackend` —— 工厂，目前只有一份实现，在 `squib-hv`。
- `Vm` —— 持有内存区域、IRQ chip 句柄、快照接口。
- `Vcpu` —— 单个 vCPU，跑到下一个 `VmExit`。
- `VmExit` —— 退出原因的代数和（`Mmio`、`Hypercall`、`PsciCall`、
  `Shutdown` 等等），VMM 事件循环就靠它分发。

HVF 实现都在 `squib-hv` 里。凡是要碰 `applevisor` 或 `hv_*` 的代码都得
进这个 crate；其他地方一律 `#![forbid(unsafe_code)]`。这个 trait 边界是
设计性的 —— 哪天 `applevisor` 不更新了，我们要的是机械替换，不是重写。

碰这一层之前先读 [`specs/11-runtime-core.md`](../specs/11-runtime-core.md)。

## 6. 加新功能怎么落地

按依赖排好序的实现计划在 [`specs/91-impl-plan.md`](../specs/91-impl-plan.md)。
绝大多数功能改动落到下面这几类里：

| 你想干 | 改哪儿 | 对应 spec |
|--------|--------|-----------|
| 加一个 API 端点或者改请求形态 | `squib-api`、controller、compat suite | [20-firecracker-api.md](../specs/20-firecracker-api.md), [21-api-compat-matrix.md](../specs/21-api-compat-matrix.md) |
| 加一个 virtio 设备 | `squib-virtio/<device>`、device manager | [14-virtio-and-devices.md](../specs/14-virtio-and-devices.md) |
| 调启动路径 | `squib-arch`、`squib-fdt`、`squib-loader`、`squib-vmm` | [13-arch-and-boot.md](../specs/13-arch-and-boot.md) |
| 改 HVF | 只动 `squib-hv` | [12-hvf-backend.md](../specs/12-hvf-backend.md) |
| 改网络 | `squib-net`、device manager | [30-networking.md](../specs/30-networking.md) |
| 改快照 | `squib-snapshot`、`squib-host`（postcopy） | [16-snapshots.md](../specs/16-snapshots.md) |
| 加 CLI flag | `apps/squib-cli/src/cli.rs`、compat matrix | [50-cli.md](../specs/50-cli.md) |

凡是会动 wire 形态的改动，固定流程：

1. 在 `specs/21-api-compat-matrix.md` 里加或者改对应那一行。
2. 在 `tests/firecracker-compat/` 里加一条测试，覆盖新形态。
3. 写实现。
4. 跑 `make compat-test` 跑到绿。

如果改动有分量 —— 加新错误码、新格式、想锁住未来不再漂的行为 —— 在
`specs/99-key-decisions.md` 里开一条 D-record。已有的 D-record 不要原地
改，新建一条 D-id 把旧的 supersede 掉，并互相反查。

## 7. 测试矩阵

| 层 | 位置 | 什么时候跑 |
|----|------|------------|
| 单元测试 | 各 crate 内 `#[cfg(test)] mod tests` | 每次 `cargo test` |
| 集成测试 | 各 crate 的 `tests/` | 每次 `cargo test` |
| Compat suite | `tests/firecracker-compat/`（一行 matrix 一个文件） | `make compat-test`，每次 CI |
| Live HVF | `crates/{hv,vmm}/tests/`，全部带 `#[ignore]` | `make hvf-test`（签过名） |
| Live vmnet | `crates/net/tests/`，带 `#[ignore]` | `make vmnet-test`（签过名） |
| 端到端 demo | `crates/vmm/tests/linux_boot_smoke.rs` | `make demo` |
| 快照冒烟 | `make snapshot-smoke` | 手动 / 发布 gate |
| 跨文件系统拒绝 | `make snapshot-cross-fs-test` | 手动；要 `hdiutil` |
| SDK soak | `tools/soak/{firectl,firecracker-go-sdk,firecracker-containerd}/run.sh` | `make soak`；缺二进制就 skip |
| Bench | `cargo bench --features bench` | 每次发布跑 `make bench-publish` |

invariant 重的地方欢迎用 `proptest` 加属性测试（FDT builder、ESR_EL2 解码、
MMDS JSON Pointer 这些）。能不 mock 就不 mock —— 真实现一定比 mock 好，
除非真实现太慢或者非 Send。

## 8. 风格要点

完整 style 指南在项目根的 `CLAUDE.md`。最容易踩坑的几条：

- **`#[cfg(test)]` 之外不要 `unwrap` / `expect`。** 用 `?` 或者显式 match。
  边界模块在 clippy 层已经 deny。
- **`squib-hv` 和 `squib-net::sys` 之外不能有 `unsafe`。** 其他 crate 全都
  `#![forbid(unsafe_code)]`。觉得"为了性能要 unsafe"先 profile —— 它几乎
  从来不是瓶颈。
- **边界处校验。** 任何来自外部的 `String`/`&str` 都要按 *字节* 限长，不是
  字符。结构体级校验用 `validator` crate。每个领域原语都包成 newtype。
- **trait 里默认用原生 `async fn`，需要 object safety 时再用 `async-trait`。**
  需要 `Arc<dyn Trait>` 的场合才用 `async-trait`，并在模块文档里写清楚为什么。
- **payload 用 `bytes::Bytes`，别用 `Vec<u8>`。** 克隆 `Bytes` 只是引用计数
  +1，克隆 `Vec` 是真拷贝。
- **不写解释 *做什么* 的注释。** 只留 `// SAFETY: …`、`// Why: …`、隐含
  invariant、特定 bug 的 workaround。代码 *做什么* 应该靠命名表达。
- **不走 deprecation 流程。** 死代码直接删，不留软删除。

## 9. specs 与 research

两份语料：

- **`specs/`** 是设计契约。按编号顺序读就是 build order
  （[00-prd](../specs/00-prd.md) → [10-data-model](../specs/10-data-model.md) →
  [11-runtime-core](../specs/11-runtime-core.md) →
  [12-hvf-backend](../specs/12-hvf-backend.md) → ⋯）。stakeholder 看
  [00-prd](../specs/00-prd.md) 加 [90-roadmap](../specs/90-roadmap.md)；
  实施某一阶段的工程师看 [91-impl-plan](../specs/91-impl-plan.md)。
- **`docs/research/`** 是现有方案研究备忘：HVF 深挖、aarch64 guest stack、
  性能与快照、macOS hypervisor 生态。底层细节心里没底时，备忘里都标了
  对应的论文、头文件、Apple 文档来源。

完整 spec 阅读顺序见 [`specs/index.md`](../specs/index.md)。research 部分
看 [`docs/research/index.md`](./research/index.md)。

## 10. 发版

```bash
make release                                    # cargo release tag + push
SIGN_ID=<DeveloperID-hash> make sign-all        # 签 release 二进制
SIGN_ID=<DeveloperID-installer-hash> make pkg   # 打 .pkg
APPLE_ID=… APPLE_TEAM_ID=… APPLE_NOTARY_PASSWORD=… make notarize
```

`make notarize` 出的是可 staple 的 `.pkg`，对应 Phase 6 的 exit criterion。
notarytool 提交跑在 post-merge 的异步流水线上（见
`.github/workflows/notarize.yml`），打 tag 不被它阻塞 —— 公证慢一拍不影响
发布节奏。

打 tag 之后跑 `make bench-publish` 重新跑 criterion，按
[`specs/71-performance-budgets.md § 7`](../specs/71-performance-budgets.md#7-publication)
把 JSON + HTML 落到 `docs/perf/<git-sha>/`。perf 数字一律是自家实测，不抄
上游 —— `docs/perf/` 里每个数都对得上某个具体 commit。

## 11. 接下来看什么

- [`specs/91-impl-plan.md`](../specs/91-impl-plan.md) —— 工程师视角的依赖
  排序构建步骤，附工时估算和退出条件。
- [`specs/93-improvements-review.md`](../specs/93-improvements-review.md) ——
  延后处理的 finding backlog，找上手任务可以从这里挑。
- [`specs/72-testing-strategy.md`](../specs/72-testing-strategy.md) —— 测试
  金字塔和上游跟踪纪律。
- [`specs/70-security.md`](../specs/70-security.md) —— 威胁模型、unsafe
  边界、密钥处理、供应链。
