# sing-box panel (Rust)

面向 **MT7981 / 16 MB flash** 小机器的 sing-box Web 控制面，Rust 重写版：
**单二进制、前端 embed、静态 musl 链接、常驻内存 5–8 MB**。
内置 **sing-box 与 mihomo 双内核**，面板里一键切换。
x86_64 / arm64 / mipsel(软浮点) 三架构可用。

本仓库是 [`sing-box-web`](https://github.com/jiqing112/sing-box-web)（Go 版）的 **Rust 完整移植**：
接口路径、JSON 字段、配置文件格式、数据目录结构与 Go 版逐字段兼容，
**Go 版的存量数据目录（settings.json / subs.json / nodes-*.json / config.json）可以直接迁过来用**。

## 产物尺寸

| 平台 | 二进制 | 说明 |
| --- | --- | --- |
| linux-amd64 (musl) | ~8.7 MB | 全静态，任何 glibc/musl 系统直接跑 |
| linux-arm64 (musl) | ~8.2 MB | MT7981（Filogic 820, Cortex-A53）即此架构 |
| windows-amd64 | ~8.3 MB | 同功能 |
| linux-mipsle (musl, 软浮点) | ~8.5 MB | MT7621 / MT7981 之外的 MIPS 小路由（Cudy AC1200 等），qemu-mipsel 实测 |

其中 ~5.2 MB 是内嵌的 mihomo geo 数据（gzip 压缩包内 GeoSite.dat + geoip.metadb）。
不需要离线 geo 时用 `--no-default-features` 编译可减到 ~3.1 MB。

## 快速开始

```sh
./panel -data ./data -listen 0.0.0.0:9090 -password your-pass
```

- 默认监听 `127.0.0.1:9090`，用户名 `admin`；不给 `-password` 会生成随机口令写入 `data/initial-password`。
- 双内核部署：把官方 `sing-box` / `mihomo` 放在面板同目录，或用 `-sing-box` / `-mihomo` 指定路径。
- 全部命令行参数：`-data -listen -sing-box -mihomo -engine -clash -token -password -autostart`
  （兼容 `-flag v` 与 `--flag=v` 两种写法；`PANEL_TOKEN` / `PANEL_PASSWORD` 环境变量同样有效）。
- 详细部署见下方 [部署指南](#部署指南)；发版由 GitHub Actions 自动完成，见 [发布与自动构建](#发布与自动构建github-actions)。

## 与 Go 版的功能对齐清单

- **订阅**：Clash YAML / Base64(URI 列表) / sing-box JSON 三种格式自动识别；
  剩余流量等占位条目过滤、跨订阅 tag 去重、节点启停、1–720h 自动更新调度。
- **配置渲染**：sing-box（JSON）与 mihomo（Clash YAML）双模板，节点 outbound 双向翻译
  （ss/vmess/vless/trojan/hysteria2/tuic/http/socks + ws/grpc/h2 + tls/reality/utls）；
  selector+urltest 策略组、规则集专用直连出站、jsDelivr 镜像迁移、download_detour→http_client 改写。
- **内核切换事务**：probe → geo 预热 → 渲染 → check → 写入 → 重启 → 探活 → 失败逐级回滚
  （实测离线切换 mihomo 0.84 s / sing-box 0.41 s）。
- **进程管理**：进程组拉起、ANSI 日志环形缓冲 400 行、10 分钟 3 次崩溃限流自动重启、
  按数据目录识别的孤儿清扫（Windows：PowerShell CIM + wmic 兜底；Linux：/proc 扫描）。
- **Clash API 桥**：延迟测速（worker 池、TTL 缓存、切内核清零）、连接/流量 WebSocket
  （REST 轮询裁剪 200 条 + 2s 节流）、`/api/v1/clash/*` 白名单反代。
- **透明代理（Linux）**：nftables redirect + 局域网 DNS 劫持(53→5354) + 本机接管（cgroup v2 回环防护）、
  RFC1918 自动探测、下发前 preflight（nft -c 探测内核 redirect 能力）、
  「入站只绑回环」告警、核心停止/切换时自动撤规则防断网。
- **TUN（Windows 需管理员）**、**路由模式**（规则分流/全局代理/全局直连）、
  **局域网代理**（SOCKS/HTTP、可选鉴权）、**本机 mixed 端口改配**。
- **安全**：登录限速（5 次锁 30s 翻倍至 15min）、Bearer token（7 天、上限 64）、
  面板内关闭登录开关、`panel.conf` 忘密重置、登录哈希 sha256(user:pass)。
- **运维**：配置快照（默认 3 份、孤儿 GC）、失败回滚、监听地址热切换（含同端口换绑回滚）、
  panel.conf 回写、systemd / OpenWrt procd / 计划任务开机自启（按 init 系统自动适配）、进程 RSS 上报（psapi / VmRSS）。
- **前端**：与 Go 版同一套 `web/dist`（无框架原生 JS），ETag 协商缓存 + SPA fallback，零改动复用。

## 与 Go 版的实现差异（不影响功能）

| 项 | Go | Rust |
| --- | --- | --- |
| 空闲内存回收 | `debug.FreeOSMemory()`（90s 无请求触发） | 系统分配器（musl 静态后常驻本就只有 5–8 MB，无需手动还页） |
| 堆软上限 | `debug.SetMemoryLimit(96MB)` | 不适用（无 GC 堆） |
| 请求体上限 | 各 handler 分别 LimitReader | 路由层统一 8 MB，handler 内同额校验 |
| Clash YAML 里 `type: ss` | 跳过（bug） | 归一为 shadowsocks（修复） |

## 本地构建

```bash
# Windows
cargo build --release
# Linux x86_64 静态（ musl）：用 zig 交叉或在目标机原生编译
cargo zigbuild --release --target x86_64-unknown-linux-musl
# Linux arm64（MT7981）
cargo zigbuild --release --target aarch64-unknown-linux-musl
# Linux mipsel 软浮点（MT7621，Cudy AC1200 等；Tier 3 目标，需 nightly）
rustup target add mipsel-unknown-linux-musl --toolchain nightly
rustup component add rust-src --toolchain nightly
# 链接用 musl 交叉工具链：curl -LO http://musl.cc/mipsel-linux-musl-cross.tgz 解压后加入 PATH
export CARGO_TARGET_MIPSEL_UNKNOWN_LINUX_MUSL_LINKER=mipsel-linux-musl-gcc
export CC_mipsel_unknown_linux_musl=mipsel-linux-musl-gcc
export RUSTFLAGS="-C target-feature=+soft-float -C link-arg=-static"
cargo +nightly build -Z build-std=std,panic_abort --release --target mipsel-unknown-linux-musl
# 不内嵌 geo（二进制 ~3.1 MB，首次切 mihomo 需联网下载 geo）
cargo zigbuild --release --target aarch64-unknown-linux-musl --no-default-features
```

测试：

```bash
cargo test            # 订阅解析 / 双内核渲染 / 引擎差异 / inspect 断言
```

## 发布与自动构建（GitHub Actions）

仓库自带 CI（[`.github/workflows/release.yml`](.github/workflows/release.yml)）：

- **发版**：推送 `v*` 标签即自动构建全部平台并发布 Release——

  ```bash
  git tag v1.0.1 && git push origin v1.0.1
  ```

  完成后在 [Releases](https://github.com/jiqing112/sing-box-web-rust/releases) 页面拿到各平台
  `tar.gz / zip` 与 `.sha256` 校验文件，Release Notes 自动生成。
- **试构建**：Actions 页 `release` 工作流支持 `workflow_dispatch` 手动触发，
  构建产物上传为 workflow artifact（不打 Release），适合验证构建改动。
- **产物矩阵**：

  | 产物 | 面板 | 内置内核 | 适用 |
  | --- | --- | --- | --- |
  | `*-linux-amd64.tar.gz` | x86_64 musl 静态 | sing-box + mihomo | x86 服务器 / NAS / 软路由 |
  | `*-linux-arm64.tar.gz` | arm64 musl 静态 | sing-box + mihomo | MT7981（AX3000 系）、ARM 盒子 |
  | `*-linux-mipsle.tar.gz` | mipsel musl 软浮点 | sing-box + mihomo (softfloat) | MT7621（Cudy AC1200 等 MIPS 老路由） |
  | `*-windows-amd64.zip` | x64 | sing-box.exe + mihomo.exe | Windows（TUN 需管理员） |

  每个包内含：`panel`（面板二进制）、两个内核、`install.sh`、`panel.conf` 模板、
  `sing-box-panel.service`、`start.bat`（Windows 包）。
- **升级内核版本**：改 workflow 顶部的 `SB_VER` / `MH_VER` 两个环境变量再打 tag。
- mipsel 是 Rust Tier 3 目标，CI 用 nightly + `-Z build-std` + musl.cc 交叉工具链构建；
  该项 `continue-on-error`，即使失败也不影响其余平台发版。

## 部署指南

### 前置：确认架构与包名

在目标机上执行 `uname -m` 对号入座：`x86_64` → linux-amd64 包；`aarch64` → linux-arm64 包；
`mipsel` → linux-mipsle 包（MT7621 等软浮点 MIPS）。

### 1. Debian / Ubuntu 服务器（amd64 / arm64）

```bash
# 下载并解压（版本号换成 Release 页最新）
curl -fsSL -o panel.tgz https://github.com/jiqing112/sing-box-web-rust/releases/download/v1.0.1/sing-box-web-rust-1.0.1-linux-amd64.tar.gz
tar -xzf panel.tgz && cd sing-box-web-rust-*-linux-amd64

sudo ./install.sh     # 装到 /opt/sing-box-panel，systemd 启动并设开机自启
```

- 安装完访问 `http://<本机IP>:9090`；初始密码打印在安装脚本末尾，
  或在 `data/initial-password` 文件里（登录后请在「设置 → 访问控制」改掉并删除该文件）。
- `install.sh` 只在缺失时安装 `panel.conf`（监听地址 / 账户密码可持久化，见包内注释）。
- 常用命令：

  ```bash
  systemctl status sing-box-panel     # 面板服务状态
  systemctl restart sing-box-panel    # 重启面板（连带重启当前内核）
  journalctl -u sing-box-panel -f     # 面板日志；内核日志在面板「服务」页看
  ```

- **升级**：停服务 → 用新 `panel` 覆盖 `/opt/sing-box-panel/bin/panel` → 起服务。
  数据目录不动，配置与订阅全保留；systemd unit 与 Go 版同路径同服务名，原地升级不用改 unit。
- 防火墙记得放行 9090（面板）；用「局域网代理 / 透明代理」再按需放行 1080/8118。

### 2. OpenWrt 路由器（MT7981 / AX3000 系，arm64）

内核与面板都是静态 musl 二进制，OpenWrt 直接跑，不装任何依赖包：

```bash
# 在电脑上把包传上路由器（Router_IP 换成路由器地址）
scp sing-box-web-rust-*-linux-arm64.tar.gz root@Router_IP:/tmp
ssh root@Router_IP
mkdir -p /opt/sing-box-panel && tar -xzf /tmp/sing-box-web-rust-*-linux-arm64.tar.gz -C /opt/sing-box-panel --strip-components=1
cd /opt/sing-box-panel && ./panel -data /opt/sing-box-panel/data -listen 0.0.0.0:9090 -password 你的密码
```

- 浏览器打开 `http://Router_IP:9090`，添加订阅 → 应用配置。
- **开机自启**：面板「设置 → 开机自启」打开开关即可——OpenWrt 上检测到 procd 会自动生成
  `/etc/init.d/sing-box-panel` 并 enable（崩溃由 procd respawn 拉起），无需手写 init 脚本。
- **透明代理（推荐 redirect 模式）**：面板「透明代理」页打开开关即可，
  nftables redirect 不需要任何 kmod；局域网设备把网关和 DNS 指到路由器就自动被接管。
  「同时接管本机流量」和「接管局域网 DNS」按需开。
- **TUN 模式**需要先装 `kmod-tun`（`opkg update && opkg install kmod-tun`），面板以 root 运行即可开。
- 若用 firewall4/fw4，面板的 nft 表（`inet singbox`）独立管理，互不干扰；关闭开关会自动撤表防断网。

### 3. MIPS 小路由（Cudy AC1200 / MT7621，16MB flash）

用 `*-linux-mipsle.tar.gz`（软浮点，qemu-mipsel 已实测：登录 / 真实订阅 TLS 拉取 / 渲染 /
双内核切换 / 经内核真实出网全部通过）：

```bash
scp sing-box-web-rust-*-linux-mipsle.tar.gz root@Router_IP:/tmp
ssh root@Router_IP
```

**flash 空间是这类机器的主要约束**（16MB flash 刨掉系统后通常只剩 1–2MB 可写）：

- 面板（~8.5MB）+ 双内核（sing-box ~20MB / mihomo ~15MB）**整体放外置存储**——
  U 盘 / TF 卡挂到 `/mnt`（或 extroot），再把包解到那里：

  ```bash
  mkdir -p /mnt/singbox && tar -xzf /tmp/sing-box-web-rust-*-linux-mipsle.tar.gz -C /mnt/singbox --strip-components=1
  cd /mnt/singbox && ./panel -data /mnt/singbox/data -listen 0.0.0.0:9090 -password 你的密码
  ```

- 自启：面板里开「开机自启」生成 `/etc/init.d/sing-box-panel`（确保外置存储在开机阶段已挂载，
  即 procd 的 START=99 时 U 盘已就绪；extroot 天然满足）。
- 内存 128MB 足够：面板 ~8MB + 内核真实占用 ~25MB（其余 RSS 是内核可回收的代码页）。
- MT7621 是软浮点 CPU，务必用 softfloat 内核（包里已配好，不用自己挑）。

### 4. Windows

解压 `*-windows-amd64.zip` 后运行包内 `start.bat`（默认监听 127.0.0.1:9090，
可用 `panel.conf` 或命令行改）。**TUN 模式需要以管理员身份运行**（右键“以管理员身份运行”再启 start.bat）。
开机自启在面板里开开关，自动注册 SYSTEM 计划任务（`sing-box-panel`）。

### 5. 首次登录与安全

- 密码来源优先级：命令行 `-password` > 环境变量 `PANEL_PASSWORD` > `panel.conf` >
  自动生成随机密码（写入 `data/initial-password`）。
- `panel.conf` 两项都留空 = 关闭登录（内网可信环境才建议）；忘记密码时改 conf 里的
  username/password 重启面板即重置。
- 面板带登录限速（5 次失败锁 30s 递增）与 Bearer token 会话（7 天）。
- 暴露到公网请务必改强密码，或只监听 `127.0.0.1` 用反向代理加鉴权。

## 迁移（从 Go 版）

停掉旧面板后，把 Rust 版二进制放进同目录、`-data` 指向原数据目录即可。
settings/subs/nodes/config/template/snapshots 全部兼容；
mihomo 的 geo 数据若已存在会被继续使用，缺失时自动从二进制内嵌副本释放（零联网）。

## 协议

MIT，详见 [LICENSE](LICENSE)。`web/dist` 前端与 [`sing-box-web`](https://github.com/jiqing112/sing-box-web)（Go 版）同源同协议。

## 接口一览

与 Go 版完全一致（`/api/v1/...` 全部 30 个端点 + 2 个 WebSocket），详见 Go 版 README。
`GET /api/v1/status` 的 `panel_version` 为 `1.0.0`。

## 实测记录（本仓库开发期）

- Windows 11 x64（sing-box 1.14.3 / mihomo 1.19.33，mock 本地内核 + 真实机场订阅数据迁移）：
  登录/限速、订阅解析（Clash YAML + Base64 URI）、渲染、check、apply、重启、
  真实 56 节点代理 204（0.25 s）、WS traffic/connections、TUN 开关（管理员）、
  快照/回滚、路由模式、局域网代理、监听热切换（含同端口换绑）、panel.conf 回写、
  内核双向切换、崩溃限流、外部击杀核心自动拉起。
- Debian 13 x86_64（VM，离线，内嵌 geo）：
  订阅经反向隧道拉取、Linux 入站（redirect/dns-in/dns-lan）渲染、apply 后
  mihomo 0.84 s / sing-box 0.41 s 双向切换、nftables 表下发与撤除、
  `ip netns` 模拟客户端（10.99.0.2）经 redirect → sing-box → direct 全链路 200、
  连接表出现 `sourceIP 10.99.0.2`、局域网 SOCKS/HTTP 开放代理 200、
  systemd unit 生成 + enable/disable、面板 RSS 5.6 MB。
