# 使用指南

squib 是一款跑在 Apple Silicon 上的 microVM 监视器，对外讲的是 Firecracker
那一套 HTTP API，吃的是同一份 JSON 配置，吐出的快照也是同样的封装格式 ——
但底层换成了 HVF 加 `vmnet.framework`，不再依赖 KVM 和 Linux 的 TAP。这份指南
写给运维使用者：装上它、把第一台 guest 跑起来、用 SDK 把它接进既有流程、做
快照、再把快照拉起来，万一翻车了知道往哪儿查。

如果你想看接口逐个字段的兼容性表，去翻
[`api-deviations.md`](./api-deviations.md) 和
[`specs/21-api-compat-matrix.md`](../specs/21-api-compat-matrix.md)。
想动手改 squib 的，请看 [`dev-guide.md`](./dev-guide.md)。

English: [user-guide.md](./user-guide.md)。

## 1. 环境要求

| 项目 | 最低 | 推荐 |
|------|------|------|
| 硬件 | Apple Silicon（M1/M2/M3/M4） | M2 Pro 及以上 |
| 系统 | macOS 15 Sequoia | macOS 26 Tahoe |
| 架构 | 仅 `aarch64-apple-darwin` | — |
| 磁盘 | 二进制 + 参考 VM 占 200 MiB | 留快照的话 1 GiB |
| 网络 | 拉参考 kernel 时需要外网 | — |

Intel Mac 不支持，今后也不会支持。没有 `x86_64-apple-darwin` 这条编译目标。
Guest 只跑 aarch64 Linux。

## 2. 安装

### 方式 A：`.pkg` 安装包

到 Release 页面下载已签名并经过公证的安装包，双击装上即可。安装后会落到
`/usr/local/bin/squib`、`/usr/local/bin/squib-jail`，以及（如果一并打包了）
`/usr/local/libexec/squib/gvproxy`。

### 方式 B：Homebrew

Homebrew 4.x 起不再接受从本地文件路径安装 formula —— 必须先把它放进一个
tap。一次性配置好之后再安装。Apple Silicon 上 tap 路径固定在 `/opt/homebrew`
下（squib 只支持 arm64），所以下面这条命令在 bash、zsh、fish、nushell 任一
shell 里都能直接跑，不依赖 `$(brew --repo …)` 这种命令替换语法：

```
brew tap-new tyrchen/squib
cp dist/homebrew/squib.rb /opt/homebrew/Library/Taps/tyrchen/homebrew-squib/Formula/squib.rb
brew install --HEAD --build-from-source tyrchen/squib/squib
```

formula 改动后要重装，先重新 `cp` 一次再 `brew reinstall --HEAD tyrchen/squib/squib`。

第一个正式 tag 之前，formula 只支持 HEAD 安装；打 tag 之后会指向公证过的
`.pkg`，tap 也会正式发布，到时候就不再需要 `tap-new` + `cp` 这两步。

### 方式 C：源码编译

```bash
git clone https://github.com/tyrchen/squib && cd squib
make sign                # 编译并用 com.apple.security.hypervisor 做 ad-hoc 签名
./target/aarch64-apple-darwin/release/squib --version
```

本地开发必须签名 —— 没有 `com.apple.security.hypervisor` 这枚 entitlement，HVF
连初始化都不愿意。`make sign` 默认用 ad-hoc 身份（`-`）；正式发布时把
`SIGN_ID` 换成 Developer ID 的哈希即可。

签名细节、entitlement 报错怎么排查，详见
[`macos-setup.md`](./macos-setup.md)。

## 3. 第一次启动 —— 参考 VM

参考 VM 是一台 256 MiB、基于 busybox initramfs 的小机器，整条启动路径都会被
它走一遍：HVF vCPU 线程、GIC、virtio-mmio 总线、PL011 串口、MMDS 一来一回，
最后用 PSCI 关机。

```bash
make build-reference-vm   # 把 vmlinux + busybox 拉到 examples/reference-vm/build/
make demo                 # 自动签名并启动，跑通 MMDS 握手才算过
```

跑到最后串口里应该会出现：

```
[init] hitting MMDS at http://169.254.169.254/latest/meta-data/instance-id
===SQUIB-DEMO===
"i-squibdemo"
===END===
[init] OK
```

如果连 kernel banner 都没看到 `make demo` 就挂了，多半是签名出了问题，
跳到 [§ 9 故障排查](#9-故障排查)。

## 4. 怎么驱动 squib

驱动方式有三种，选哪个看场景：静态配置文件、用 `curl` 直接打 UDS、或者拿
某个 Firecracker SDK 当客户端。

### 静态配置文件

```bash
squib --config-file examples/reference-vm/config.json
```

squib 会按上游 Firecracker 的顺序回放配置（boot-source → drives →
network-interfaces → mmds-config → machine-config），最后替你调一次
`InstanceStart`。如果不想再绑 UDS，加 `--no-api`。

### 直接走 UDS 打 HTTP

```bash
squib --api-sock /tmp/squib.sock &

curl --unix-socket /tmp/squib.sock http://localhost/version
# {"firecracker_version":"1.16.0","squib_version":"0.1.0"}

curl --unix-socket /tmp/squib.sock -X PUT http://localhost/boot-source \
  -H 'Content-Type: application/json' \
  -d '{"kernel_image_path":"/path/to/Image","boot_args":"console=ttyAMA0"}'

curl --unix-socket /tmp/squib.sock -X PUT http://localhost/actions \
  -H 'Content-Type: application/json' \
  -d '{"action_type":"InstanceStart"}'
```

请求形态、状态码、`{"fault_message": "..."}` 错误体都和上游对齐。
`Server: Firecracker API` 这个响应头原样保留，所以那些靠 sniff header 判断
对端身份的 SDK 一样能认。

### SDK

`firectl`、`firecracker-go-sdk`、`firecracker-containerd`、
`weaveworks/ignite` 都能不改一行代码直接驱动 squib，把它们指向 squib 的
二进制或者 socket 就行 —— 除了
[文档化的几处差异](./api-deviations.md) 之外，它们看不出对面是 squib。

```bash
firectl --firecracker-binary "$(which squib)" \
  --kernel-image "$PWD/examples/reference-vm/build/Image" \
  --kernel-opts "console=ttyAMA0 reboot=k panic=1"
```

## 5. 网络

按手头的 entitlement 和想要的网络拓扑选模式：

| `--network=` | 需要 entitlement | 网络形态 | 吞吐 |
|--------------|------------------|----------|------|
| `shared`（默认） | `com.apple.security.hypervisor`（自签即可） | guest 出公网，走 NAT | ≥ 1 Gbit/s |
| `host` | 同上 | host-only，没有外网 | n/a |
| `bridged` | `com.apple.vm.networking`（受限） | guest 在物理网络拿一个 L2 地址 | ≥ 1 Gbit/s |
| `userspace` | 不需要 | 把 gvproxy 拉起来作为子进程 | 约 300–400 Mbit/s |

没什么特殊需求就用 `shared`。`bridged` 那个受限 entitlement 要走 Apple
DTS 申请，默认编译里直接禁用了 —— 想用得打开 `bridged` cargo feature，再
拿带这个 entitlement 的身份重签。`userspace` 是退路，给那种公司 Mac 被
锁得死死、没法用 vmnet 的同学。

`PUT /network-interfaces/{id}` 里的 `host_dev_name` 字段在 wire 上原样保留
（这样老配置才能 round-trip 过去），但内部映射成一个叫 `squib-tap-<iface_id>`
的 vmnet 句柄 —— Linux 风格的 TAP 名字在 host 这边没有任何效果。

## 6. MMDS

microVM 的元数据服务还是挂在 `169.254.169.254`，和上游 Firecracker
完全一样。V1（直接 GET）和 V2（IMDSv2，需要 token）都支持：

```bash
# 写入数据
curl --unix-socket /tmp/squib.sock -X PUT http://localhost/mmds \
  -H 'Content-Type: application/json' \
  -d '{"latest":{"meta-data":{"instance-id":"i-squibdemo"}}}'

# 绑到某个网络接口上
curl --unix-socket /tmp/squib.sock -X PUT http://localhost/mmds/config \
  -H 'Content-Type: application/json' \
  -d '{"version":"V2","network_interfaces":["eth0"]}'
```

guest 内部读取：

```sh
TOKEN=$(wget -qO- --method=PUT --header='X-metadata-token-ttl-seconds: 60' \
  http://169.254.169.254/latest/api/token)
wget -qO- --header="X-metadata-token: $TOKEN" \
  http://169.254.169.254/latest/meta-data/instance-id
```

JSON Pointer 那一套和上游一模一样。

## 7. 快照

squib 的快照外层封装和上游 Firecracker 字节对齐 ——
`bitcode::serialize(Snapshot { header, data: MicrovmState })` 之后跟 8 字节
CRC-64。但 `MicrovmState` *里面* 的内容是按 HVF 形态走的：sysreg 集合不同、
GIC blob 不同，所以 KVM 和 HVF 之间没法跨着读。

### 保存

```bash
curl --unix-socket /tmp/squib.sock -X PATCH http://localhost/vm \
  -H 'Content-Type: application/json' -d '{"state":"Paused"}'

curl --unix-socket /tmp/squib.sock -X PUT http://localhost/snapshot/create \
  -H 'Content-Type: application/json' \
  -d '{"snapshot_path":"/tmp/vm.snap","mem_file_path":"/tmp/vm.mem","snapshot_type":"Full"}'
```

`Diff` 类型的快照靠 HVF 的 `hv_vm_protect` 在两次 save 之间追踪脏页。

### 查看

```bash
squib --describe-snapshot /tmp/vm.snap
# magic: 0x07101984_AAAA_0000
# version: 1.0.0
# crc_ok: YES
```

CRC 校验失败的话同样会把元数据打出来，但 `crc_ok` 显示 `NO`，并以退出码 2
告退。

### 恢复

```bash
squib --api-sock /tmp/squib2.sock &

curl --unix-socket /tmp/squib2.sock -X PUT http://localhost/snapshot/load \
  -H 'Content-Type: application/json' \
  -d '{"snapshot_path":"/tmp/vm.snap","mem_backend":{"backend_type":"File","backend_path":"/tmp/vm.mem"}}'
```

走 Mach exception port 的 postcopy（`pager-live-mach`）默认是开的：没碰到的
内存页第一次访问时才懒加载，所以哪怕几个 GiB 的 VM，也是几百毫秒就能把控制
权交回 API 层。

## 8. CLI 速查

完整的 CLI 形态在 [`specs/50-cli.md`](../specs/50-cli.md)。日常会用到的：

| 选项 | 默认 | 作用 |
|------|------|------|
| `--api-sock <path>` | `/run/firecracker.socket` | API server 绑的 UDS 路径 |
| `--id <id>` | `anonymous` | microVM 实例 id |
| `--config-file <path>` | — | 回放静态配置文件 |
| `--no-api` | — | 只回放，不绑 UDS |
| `--metadata <path>` | — | 启动时给 MMDS 喂初始数据 |
| `--log-path <path>` | stderr | tracing 日志输出 |
| `--level <lvl>` | `Info` | `Off` / `Error` / `Warning` / `Info` / `Debug` / `Trace` |
| `--network <mode>` | `shared` | `shared` / `host` / `bridged` / `userspace` |
| `--gvproxy-path <path>` | `$SQUIB_GVPROXY_PATH` | 覆盖打包进来的 gvproxy 路径 |
| `--snapshot-version` | — | 打印快照格式版本后退出 |
| `--describe-snapshot <path>` | — | 查看快照文件后退出 |

那些只在 Linux 上有意义的 flag（`--seccomp-filter`、`--no-seccomp`、
`--enable-pci`、x86 那一组 `cpu_template`、`huge_pages: "2M"` 等）squib 都
照单全收，启动时 warn 一次就过 —— 不让任何老 launcher 因为这个炸掉。完整
列表见 [`api-deviations.md`](./api-deviations.md)。

## 9. 故障排查

**刚启动就 `HV_ERROR` 或 `EX_NOPERM`。** 二进制没拿到 hypervisor
entitlement，先确认一下：

```bash
codesign --display --entitlements - "$(which squib)"
```

如果输出里看不到 `com.apple.security.hypervisor`，重跑 `make sign`。

**`make demo` 启动了，但 MMDS 那条断言一直没触发。** 参考 kernel 没编进
PL011 console 驱动 —— kernel 是起来了，可 earlycon 没绑上去，串口就一直是
哑的。换成 `make hvf-test`，那边失败时会把 DRAM 里的 ringbuffer 整段 dump
出来，更好排错。

**`PUT /actions {InstanceStart}` 回 `400 fault_message: "VMM not yet wired"`。**
你跑的是 Phase < 1.6 阶段的版本：API 层已经齐了，但 HVF 后端还没接上来。
拉一下 `master` 或者等下一个 tag。

**长请求收到 `504 Gateway Timeout`。** ApiAction 超过它那一类的超时阈值后
squib 会直接 504，详见
[`api-deviations.md § Squib-only response codes`](./api-deviations.md#squib-only-response-codes)。
对应动作还在 VMM 里挂着，客户端用同样的幂等键重试即可。

**`--network=bridged` 报 "requires `--features bridged`"。** 你拿的是默认编译。
bridged 模式要 `com.apple.vm.networking` 这枚受限 entitlement，并且要
重新签名。在那之前先用 `--network=shared`（NAT）或者 `--network=userspace`
（gvproxy）。

**从浏览器下载下来的二进制带了 quarantine 属性。** macOS 会拦着不让跑，
手动剥掉即可：

```bash
xattr -d com.apple.quarantine /path/to/squib
```

其他奇怪问题，加 `--level Debug --show-log-origin` 重跑，按对应子系统
（`squib_hv` / `squib_vmm` / `squib_api` / `squib_net` / `squib_snapshot`）
grep 日志。

## 10. 接下来读什么

- [`api-deviations.md`](./api-deviations.md) —— 所有和上游 Firecracker
  不一样的地方，每条都附带 curl 复现。
- [`macos-setup.md`](./macos-setup.md) —— entitlement、签名翻车、网络
  模式选型的细节。
- [`perf/index.md`](./perf/index.md) —— 每个 revision 实测的启动、快照、
  内存数字。
- [`dev-guide.md`](./dev-guide.md) —— 怎么编、怎么测、怎么提 PR。
- [`specs/`](../specs/index.md) —— 完整设计文档。
