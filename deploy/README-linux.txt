sing-box panel · Linux 免安装包
================================

包里有什么
----------
  panel                     面板本体（Web 控制台，已内嵌前端）
  sing-box                  官方 sing-box 内核（安装脚本会放到 /usr/local/bin）
  mihomo                    Mihomo（Clash Meta）内核，可选的第二个内核
  panel.conf                面板配置：监听地址 / 登录账户 / 密码
  sing-box-panel.service    systemd 单元
  install.sh                一键安装脚本
  README.txt                本文件


一键安装
--------
  tar -xzf sing-box-web-*-linux-*.tar.gz
  cd sing-box-web-*-linux-*
  sudo ./install.sh

脚本会做这些事：
  1. 面板装到 /opt/sing-box-panel/bin/panel，数据在 /opt/sing-box-panel/data
  2. sing-box 内核装到 /usr/local/bin/sing-box，mihomo 装到 /usr/local/bin/mihomo
  3. 安装并启用 systemd 服务 sing-box-panel（开机自启）
  4. 首次启动生成随机密码，并打印出来（也在 /opt/sing-box-panel/data/initial-password）

安装完访问 http://<本机IP>:9090 ，用 admin + 打印出来的密码登录。

  登录后： 「订阅」页添加订阅 → 「配置」页 生成 / 校验 / 应用
  默认代理端口： mixed 127.0.0.1:7890（仅本机）
  局域网要用： 「透明代理」页开启「局域网代理」并设置用户名密码


两个内核，随时切换
------------------
包里的 mihomo 是可选的第二内核，默认仍然用 sing-box。切换入口在
「设置 → 内核」，点一下就换：

  sing-box  配置 config.json（JSON），校验用 sing-box check -c config.json
  mihomo    配置 config.yaml（Clash 系 YAML），校验用 mihomo -t -d <data> -f config.yaml

切换时面板会：
  · 按新内核重新渲染一份配置（两个内核的配置、模板、快照互不覆盖，切回去还在）
  · 停掉旧核心 → 校验新配置 → 启动新核心 → 探活
  · 任一步失败自动切回原内核，并把旧配置写回去

节点只存一份（sing-box outbound 片段），渲染时翻译成 Clash proxy，
所以订阅解析、测速、选节点在两个内核下行为一致。

mihomo 第一次启动会下载约 12 MB 的 geo 数据（geosite.dat / country.mmdb），
内置模板已把 geox-url 指向 jsDelivr 镜像，国内也能下。校验超时放宽到 90 秒。
下载后的数据在 /opt/sing-box-panel/data/mihomo/ 下，和 sing-box 的 cache.db 分开放，
不会互相锁文件。

想启动时强制指定内核（覆盖面板里存的选择）：
  panel -engine mihomo
自定义内核路径：
  panel -sing-box /usr/local/bin/sing-box -mihomo /usr/local/bin/mihomo


手动安装（不用脚本）
--------------------
  install -d /opt/sing-box-panel/{bin,data}
  install -m755 panel /opt/sing-box-panel/bin/panel
  install -m755 sing-box /usr/local/bin/sing-box
  install -m755 mihomo /usr/local/bin/mihomo
  install -m600 panel.conf /opt/sing-box-panel/bin/panel.conf
  install -m644 sing-box-panel.service /etc/systemd/system/
  systemctl daemon-reload && systemctl enable --now sing-box-panel


升级
----
  解压新包后重新执行 sudo ./install.sh 即可。
  已有的 panel.conf 与 /opt/sing-box-panel/data 不会被覆盖（配置、订阅、快照都保留）。


常见问题
--------
· 想改监听端口 / 口令
  改 /opt/sing-box-panel/bin/panel.conf 后 systemctl restart sing-box-panel。
  注意 unit 里写死了 -listen 0.0.0.0:9090，命令行参数优先于 panel.conf；
  要永久改端口请同时修改 unit 的 ExecStart。

· 忘记密码
  在 panel.conf 里把 password= 改成新密码，重启面板即可。

· 透明代理（nftables）
  「透明代理」页可以接管局域网 TCP 流量与 DNS，需要 root（systemd 服务默认就是 root）。
  开启前请确认本机没有别的代理软件占用 7890 / 12345 / 5354 / 53 端口。

  两个内核在 Linux 上都用同一个 nft 表（inet singbox）：
    sing-box  靠 redirect 入站（模板里的 tproxy 入站）
    mihomo    靠 redir-port（面板渲染时自动写入，端口与面板的透明代理端口一致）
  mihomo 的 DNS 劫持目标是 dns.listen 0.0.0.0:5354，与 sing-box 的 dns-lan 入站同端口。
  所以切换内核后透明代理规则不用重下，面板会自动核对一遍；对不上就关掉透明代理并提示。

· TUN 模式
  Linux 上同样需要 root 与 /dev/net/tun，开启后整机流量走虚拟网卡。
  sing-box 的网卡叫 singbox-tun，mihomo 的叫 mihomo-tun。

· mihomo 报「mihomo 不可用」
  说明二进制不在 /usr/local/bin/mihomo，或者跑不起来。用 mihomo -v 验证，
  或用 -mihomo /path/to/mihomo 指定路径。

· 日志
  journalctl -u sing-box-panel -f


目录说明
--------
  /opt/sing-box-panel/bin/panel.conf          面板配置
  /opt/sing-box-panel/data/settings.json      面板设置（含当前选的内核）
  /opt/sing-box-panel/data/template.json      sing-box 配置模板
  /opt/sing-box-panel/data/config.json        下发给 sing-box 的配置
  /opt/sing-box-panel/data/template.yaml      mihomo 配置模板
  /opt/sing-box-panel/data/config.yaml        下发给 mihomo 的配置
  /opt/sing-box-panel/data/nodes-*.json       订阅解析出的节点（两个内核共用）
  /opt/sing-box-panel/data/snapshots/         配置快照（默认 3 份，可回滚）
  /opt/sing-box-panel/data/mihomo/            mihomo 工作目录（geo 数据、cache.db）
  /opt/sing-box-panel/data/initial-password   首次启动生成的密码，登录后建议删除
