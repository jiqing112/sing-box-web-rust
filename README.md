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

- 默认监听 `127.0.0.1:9090`；首次启动打开网页会要求设置登录账户密码，
  `-password`/`PANEL_PASSWORD`/panel.conf 的 `password=` 提供密码则跳过网页初始化。
- **首次部署开箱即跑**：添加订阅并启用节点后，点「启动核心」（或重启面板）会自动
  「渲染 → 校验 → 应用 → 拉起核心」，无需先去配置页手工操作。
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
  测速接口**默认命中 5 分钟缓存**（只实测缺失/过期的节点，响应里带 `measured`/`cached` 计数），
  需要强制重测时传 `{"refresh": true}`；面板「节点」页的「强制重测」勾选框就是它。
- **透明代理（Linux）**：nftables redirect + 局域网 DNS 劫持(53→5354) + 本机接管（cgroup v2 回环防护）、
  RFC1918 自动探测、下发前 preflight（nft -c 探测内核 redirect 能力）、
  「入站只绑回环」告警、核心停止/切换时自动撤规则防断网。
- **TUN（Windows 需管理员）**、**路由模式**（规则分流/全局代理/全局直连）、
  **局域网代理**（SOCKS/HTTP、可选鉴权）、**本机 mixed 端口改配**。
- **安全**：登录限速（5 次锁 30s 翻倍至 15min）、Bearer token（7 天、上限 64）、
  面板内关闭登录开关、`panel.conf` 忘密重置、登录哈希 sha256(user:pass)。
- **运维**：配置快照（默认 3 份、孤儿 GC）、失败回滚、监听地址热切换（含同端口换绑回滚）、
  panel.conf 回写、systemd / OpenWrt procd / 计划任务开机自启（按 init 系统自动适配）、进程 RSS 上报（psapi / VmRSS）。
- **Windows 托盘**：双击托盘图标打开网页配置，右键启停/重启核心、开机自启开关、优雅退出；
  explorer 重启自动恢复图标，`-notray` 可关闭（unix 无此模块，零依赖零开销）。
- **代理模式页 + 系统代理**（Windows）：入口（系统代理/TUN 网卡）与出口（路由模式）
  同页整合，状态条四态提示两者关系；系统代理一键把登录用户的 Windows 代理指向面板
  mixed 端口（随设置），写完广播刷新浏览器免重启生效，关闭还原开启前原值。
- **首次运行自动应用**：启动时无已应用配置且有启用节点 → 自动「渲染→应用」拉起核心；
  网页点「启动核心」遇到空配置同样引导完成，首次部署零手工。
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
  git tag v1.3.0 && git push origin v1.3.0
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

> 以下所有流程均在真实环境验证过：Debian 13 虚拟机（amd64）、Debian 小主机（x86_64，
> 迁移 Go 版数据）、Debian 13 + qemu-mipsel（模拟 MT7621 运行 mipsle 产物）。
> 命令可直接复制执行，版本号 `1.0.1` 换成 Release 页最新即可。

### 第 0 步：选对包

在目标机执行 `uname -m` 对号入座：

| `uname -m` 输出 | 下载包 | 典型设备 |
| --- | --- | --- |
| `x86_64` | `*-linux-amd64.tar.gz` | x86 服务器 / NAS / 软路由 |
| `aarch64` | `*-linux-arm64.tar.gz` | MT7981（红米 AX3000、CR880x 等）、ARM 盒子 |
| `mipsel` | `*-linux-mipsle.tar.gz` | MT7621（Cudy AC1200、小米 mini 等 MIPS 老路由） |
| （Windows） | `*-windows-amd64.zip` | Windows 10/11 x64 |

**端口占用总表**（后面反复用到，先记住）：

| 端口 | 用途 | 默认监听范围 |
| --- | --- | --- |
| 9090 | 面板 Web | `127.0.0.1`（conf 可改 `0.0.0.0`） |
| 9091 | Clash API（面板内部读状态/测速） | 仅 `127.0.0.1` |
| 7890 | mixed 入站（系统代理） | 仅 `127.0.0.1` |
| 12345 | 透明代理 redirect 入站 | `0.0.0.0` |
| 5354 | 局域网 DNS 劫持入站 | `0.0.0.0` |
| 1080 / 8118 | 局域网 SOCKS5 / HTTP 代理 | `0.0.0.0`（开关开启时） |
| 53 | DNS 入站（sing-box 模板自带） | 仅 `127.0.0.1` |

### 场景 A：Debian / Ubuntu 服务器（amd64 / arm64）

#### A1. 下载与校验

```bash
# 在 Release 页找到最新版本，或直接用命令下载（版本号自行替换）
curl -fsSL -o /tmp/panel.tgz \
  https://github.com/jiqing112/sing-box-web-rust/releases/download/v1.3.0/sing-box-web-rust-1.1.0-linux-amd64.tar.gz

# 校验完整性（可选但建议）
curl -fsSL -o /tmp/panel.tgz.sha256 \
  https://github.com/jiqing112/sing-box-web-rust/releases/download/v1.3.0/sing-box-web-rust-1.1.0-linux-amd64.tar.gz.sha256
cd /tmp && sha256sum -c panel.tgz.sha256
# 预期输出: sing-box-web-rust-1.1.0-linux-amd64.tar.gz: OK

tar -xzf /tmp/panel.tgz -C /tmp && cd /tmp/sing-box-web-rust-1.1.0-linux-amd64
ls
# 预期: install.sh  mihomo  panel  panel.bat  panel.conf  README-linux.txt
#       README-windows.txt  sing-box  sing-box-panel.service  start.bat
```

包内 `panel` 是面板本体（前端已内嵌，无任何外部依赖），`sing-box` / `mihomo` 是两个官方内核。

#### A2. 安装

```bash
sudo ./install.sh
```

脚本做的事（每步都有输出）：

1. 停掉已在运行的旧 `sing-box-panel` 服务（如有）；
2. `panel` → `/opt/sing-box-panel/bin/panel`，数据目录 `/opt/sing-box-panel/data`；
3. `sing-box` → `/usr/local/bin/sing-box`，`mihomo` → `/usr/local/bin/mihomo`；
4. `panel.conf` 只在缺失时安装（**不会覆盖**你改过的配置）；
5. 写入并启用 systemd 服务 `sing-box-panel`（开机自启）；
6. 打印面板地址和初始密码。

#### A3. 首次登录

- 浏览器打开 `http://<本机IP>:9090`，用户名 `admin`。
- 密码从哪来（优先级从高到低）：
  1. `panel.conf` 里写了 `password=xxx` → 用它；
  2. 启动参数 `-password xxx` 或环境变量 `PANEL_PASSWORD` → 用它；
  3. 都没给 → 自动生成随机密码，**安装输出里直接打印**，同时落在
     打开网页按引导完成首次账户设置；或在 panel.conf 填 `password=` 后重启。
- 登录后请到「设置 → 访问控制」改掉密码。
- 忘记密码：编辑 `/opt/sing-box-panel/bin/panel.conf`，填上 `username=admin` 和
  新 `password=xxx`，`systemctl restart sing-box-panel` 即完成重置。

#### A4. 第一次上手（面板操作顺序）

1. **订阅页** → 添加订阅：粘贴机场订阅链接，自动识别 Clash YAML / Base64 URI / sing-box JSON；
   卡片上会显示流量用量与到期时间（机场响应头里有就显示）。
2. **节点页** → 点测速：延迟写入内存缓存（默认 5 分钟 TTL）；不用的节点把开关关掉。
3. **配置页** → 校验 → 应用：应用面板按「模板 + 启用节点」渲染配置，
   `sing-box check` 通过才会落盘重启；失败自动回滚上一份配置。
4. **透明代理页 / 局域网代理页**：按需开启（详见场景 B6 / A9）。
5. **概览页**「运行中」旁的绿色徽标是当前出口 IP，可用来确认流量真的走了节点。

#### A5. 日常运维

```bash
systemctl status sing-box-panel        # 面板服务状态（内核是面板子进程）
systemctl restart sing-box-panel       # 重启面板（连带重启内核）
journalctl -u sing-box-panel -f        # 面板自身日志
ss -tlnp | grep -E '9090|9091|7890'    # 端口确认
```

内核运行日志在面板「服务」页（尾部 400 行环形缓冲，不落盘）；连接与流量在「连接」页实时看。

#### A6. 升级版本

```bash
systemctl stop sing-box-panel
sudo install -m 755 新目录/panel /opt/sing-box-panel/bin/panel
systemctl start sing-box-panel
```

数据目录不动，订阅 / 节点 / 设置 / 快照全保留。systemd 单元与 Go 版同路径同服务名，
从 Go 版原地升级不用改 unit。

#### A7. 卸载

```bash
sudo systemctl disable --now sing-box-panel
sudo rm -f /etc/systemd/system/sing-box-panel.service /usr/local/bin/sing-box /usr/local/bin/mihomo
sudo rm -rf /opt/sing-box-panel
```

#### A8. 透明代理（本机 + 局域网 TCP 接管）

面板「透明代理」页打开总开关即可，nftables `inet singbox` 表由面板自动下发与撤除：

- 「接管网段」默认自动探测本机所在 RFC1918 网段，可手填（如 `192.168.1.0/24`）；
- 「同时接管本机流量」= output 链也重定向（服务器自己发的请求也走代理，自动排除面板进程）；
- 「接管局域网 DNS」= 53 端口重定向到内核 DNS（5354），客户端解析不再被污染；
- 目标为内网的流量永远直连；redirect 只接管 TCP，QUIC 自动回落 TCP。

客户端（局域网设备）什么都不用装：**网关和 DNS 都指向本机 IP** 即生效。

#### A9. 故障排查速查表

| 症状 | 原因与处理 |
| --- | --- |
| 启动报 `listen ...: Address in use` | 9090 被占：`ss -tlnp | grep 9090` 找到占用者，或改 panel.conf 的 listen |
| 面板开了但核心起不来 | 「服务」页看内核日志；常见是 `sing-box` 路径不对（面板设置→内核里能看到探测结果） |
| 应用配置返回 422 | 内核 check 不过，报错原文会透出（如节点字段缺失）；先在节点页关掉可疑节点再应用 |
| 改完配置不生效 | 配置只在「应用」时生效；删订阅后也需要重新应用一次 |
| 订阅拉取报 403 | 机场按 UA 拦截：面板固定用 `clash-verge/v1.7.7` UA，一般不会；换个网络确认机场本身可达 |
| 订阅刷新显示成功但节点没变 | 机场返回 204（内容未变化/限流），面板按「未变化」处理属正常行为 |
| 测速全是 -1 | 所有节点超时（6 秒/个）：先确认概览页出口 IP 徽标；或机场节点本身不可用 |
| 点测速秒回、数字没变 | 5 分钟内命中缓存属正常：勾选「强制重测」或传 `refresh:true` 才重新实测 |
| 概览出口 IP 与实际不符 | 切路由模式/内核/启停核心会自动失效缓存；若刚在面板外改了系统代理，等 60 秒或刷新页面 |
| **整网断网了** | 核心没跑但 redirect 规则还在（异常掉电等）：`nft delete table inet singbox` 立即恢复直连；正常关闭面板会自动撤表 |
| 面板密码忘了 | 见 A3「忘记密码」 |

### 场景 B：OpenWrt 路由器（MT7981 / AX3000 / CR880x 等，arm64）

#### B1. 前置检查

```bash
uname -m                      # 必须是 aarch64
df -h / /overlay              # 看可用空间；panel 8.2MB + 内核（见 B2 空间方案）
opkg print-architecture      # 确认包管理正常（仅装 kmod-tun 时才需要 opkg）
```

面板与内核都是静态 musl 二进制，**不需要装任何依赖包**；只有 TUN 模式需要 `kmod-tun`。

#### B2. 下载解压

空间够（≥35MB 可写）就直接放路由器；不够就放 U 盘（挂载点下文以 `/mnt/usb` 为例）：

```bash
# 电脑上执行，Router_IP 换成路由器 LAN IP
scp sing-box-web-rust-*-linux-arm64.tar.gz root@Router_IP:/tmp

# 路由器上执行
mkdir -p /opt/sing-box-panel
tar -xzf /tmp/sing-box-web-rust-*-linux-arm64.tar.gz -C /opt/sing-box-panel --strip-components=1
cd /opt/sing-box-panel && ls
# panel  sing-box  mihomo  install.sh  panel.conf  sing-box-panel.service ...
```

> 16MB flash 机型：内核放不进 flash，改解压到 U 盘
> `mkdir -p /mnt/usb/singbox && tar -xzf ... -C /mnt/usb/singbox --strip-components=1`，
> 并确保 U 盘在开机阶段自动挂载（块设备挂载点 / extroot）。

#### B3. 首次启动（先手动跑通，再上自启）

```bash
cd /opt/sing-box-panel
./panel -data /opt/sing-box-panel/data -listen 0.0.0.0:9090 -password 你的密码
# 保持这个 SSH 会话不关，浏览器打开 http://Router_IP:9090 按 A4 流程配置；
# 配置应用成功、概览页显示「运行中」后，Ctrl+C 停掉，进入 B4。
```

#### B4. 开机自启（procd）

面板里「设置 → 开机自启 → 面板」打开开关：检测到 OpenWrt（无 systemd）会自动生成
`/etc/init.d/sing-box-panel`（START=99，崩溃由 procd `respawn` 自动拉起）并 enable。
生成后可 `ls -l /etc/rc.d/S99sing-box-panel` 确认；改过安装路径就在面板里把开关关一次再开一次。

#### B5. 透明代理与客户端指向

面板「透明代理」页打开开关（接管网段会自动探测，多网段路由器按需手填）。
局域网设备**网关与 DNS 都指向路由器 LAN IP** 即被接管；路由器自己发出的流量默认不走代理。

#### B6. TUN 模式（可选）

```bash
opkg update && opkg install kmod-tun
```

然后面板「透明代理」页打开 TUN 开关（需要面板以 root 运行，OpenWrt 默认就是）。
TUN 与 redirect 可同时开：TUN 接管本机，redirect 接管局域网。

#### B7. OpenWrt 特别注意

- 防火墙（fw4）与面板的 nft 表（`inet singbox`）互不干扰；关掉面板开关会自动撤表防断网。
- 「同时接管本机流量」依赖 cgroup v2 且按 systemd 的 cgroup 路径排除面板进程，
  OpenWrt（procd）下**不建议开启**，局域网接管不受影响。
- 整网断网的救援命令同 A8 表格（`nft delete table inet singbox`）。

### 场景 C：MIPS 小路由（Cudy AC1200 / MT7621，16MB flash）

用 `*-linux-mipsle.tar.gz`（软浮点；qemu-mipsel 实测：登录、真实订阅 TLS 拉取、渲染、
双内核切换、经内核真实出网全部通过）。内存 128MB 够用：面板 ~8MB + 内核真实占用 ~25MB
（RSS 里其余是内核可回收的代码页）。

**flash 是唯一硬约束**：16MB flash 刨掉系统通常只剩 1–2MB 可写，而内核解压后 15–20MB，
所以内核必须放外置存储：

```bash
# 插上 U 盘 / TF 卡后（OpenWrt 默认自动挂载到 /mnt/sda1 类路径）
mount | grep mmc\|sd         # 确认挂载点
mkdir -p /mnt/usb/singbox
tar -xzf /tmp/sing-box-web-rust-*-linux-mipsle.tar.gz -C /mnt/usb/singbox --strip-components=1
cd /mnt/usb/singbox && ./panel -data /mnt/usb/singbox/data -listen 0.0.0.0:9090 -password 你的密码
```

其余流程与场景 B 完全一致（自启、透明代理、客户端指向）；包里 sing-box / mihomo
都是官方 mipsle softfloat 静态版，无需挑选。extroot（把 overlay 整体搬到 U 盘）的机器
可以无视外置存储注意事项，直接当大 flash 机器用。

### 场景 D：Windows

1. 解压 `*-windows-amd64.zip` 到任意目录（路径建议不要带中文）。
2. 双击 `start.bat`（或命令行 `panel.exe -data .\panel-data -listen 0.0.0.0:9090 -password 你的密码`）。
3. 浏览器 `http://127.0.0.1:9090`，按 A4 流程配置。
4. **代理模式页**（见下方专述）：至少开一个「流量入口」，路由模式才会作用于普通流量。
5. 开机自启：面板里开开关，或跑 `panel.bat autostart-on`（非管理员自动弹 UAC）——
   两者操作的是同一个 SYSTEM 计划任务 `sing-box-panel`，状态互通；
   `/TR` 参数按数组直传，安装路径带空格也能正常注册。

**系统托盘**（仅 Windows）：panel.exe 运行时任务栏右下角有绿菱形托盘图标——
**双击打开网页配置**，右键菜单：打开面板 / 启动·停止·重启核心 / 开机自启开关 / 退出
（退出与 Ctrl+C 同为优雅退出，自动停核心、撤代理规则）。托盘状态 3 秒刷新；
不想要托盘用 `-notray` 启动。注意：以 SYSTEM 计划任务开机自启的面板运行在
会话 0，没有桌面也就没有托盘，属正常现象。

**代理模式页**（仅 Windows；Linux 同页为「透明代理」redirect）：
- **出口策略：路由模式**——规则分流 / 全局代理 / 全局直连。注意它只对
  **进入核心的流量**生效，入口没开时普通流量根本不进核心。
- **流量入口**（两者至少开其一）：
  - **系统代理**：一键把当前登录用户的 Windows 系统代理指向面板 mixed 端口
    （`127.0.0.1:7890`，端口随设置联动）。写完广播 WinInet 刷新，Edge/Chrome
    **免重启即时生效**；关闭自动还原开启前原值（原 ProxyEnable/Server/Override
    存于 `panel-data/sysproxy-state.json`）。面板以 SYSTEM 计划任务运行时，
    自动定位到交互会话真实用户 hive 写入（console 无登录用户时枚举 RDP Active
    会话），日志明确记录写到哪个用户；普通用户面板直写 HKCU。
  - **TUN 虚拟网卡**：接管整机流量（含不走系统代理的程序），需管理员。
- **状态提示条**：四态实时提示——全关（警示「路由模式不生效」）/ 仅系统代理 /
  仅 TUN / 双开（提示冗余）。

**panel.bat 管理脚本**：双击 = 智能默认——未运行则**后台隐藏启动**（无黑窗，
仅托盘 + 自动开网页，脚本窗口自关），已运行则弹管理菜单。命令行直跑：
`start`（前台）/ `startbg`（后台 + 托盘）/ `stop`（只停本数据目录的内核，
不误伤其他实例）/ `restart` / `status` / `open` / `autostart-on|off` / `menu`。
start.bat 双击同为后台常驻；两者脚本均为 GBK 编码（中文 Windows 默认控制台直接可用）。

### 首次登录与安全

- 密码来源优先级：命令行 `-password` > 环境变量 `PANEL_PASSWORD` > `panel.conf` >
  全新面板未提供密码时进入网页首次初始化流程。
- **关闭登录**只能用面板内「设置 → 允许免密码访问」开关（`panel.conf` 不再承担
  此语义；全新面板未提供密码时进入网页首次初始化流程）。
- 面板自带登录限速（连续 5 次失败锁 30 秒，逐次翻倍至 15 分钟）与 Bearer token 会话（7 天）。
- 暴露公网：务必改强密码，或保持 `127.0.0.1` 监听 + 反向代理加鉴权。

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

- Windows x64（sing-box 1.14.3 / mihomo 1.19.33，真实机场订阅 28 个 vless 节点，2026-11 全量回归）：
  登录/限速、订阅解析与自动更新（回拨 `updated_at` 后 60 s 内自动刷新）、渲染/check/apply、
  rule|global|direct 三模式真实出网（出口 MY，百度直连）、TUN 双内核（`singbox-tun` / `mihomo-tun`）、
  局域网 SOCKS/HTTP 鉴权（407 / SOCKS 拒绝）、WS 流量与连接、双内核切换（1.0–1.3 s）、
  快照回滚、监听热切换、孤儿清扫（清掉残留内核且不误杀其他代理实例）、崩溃自动拉起；
  面板**冷启动 167 ms 即可访问**（进程清扫放后台），内核随后自动拉起。
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
