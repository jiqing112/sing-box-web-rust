sing-box panel · Windows 免安装版
==================================

包里有什么
----------
  panel.exe       面板本体（Web 控制台，已内嵌前端）
  sing-box.exe    官方 sing-box 内核（面板会自动在自身目录找到它）
  mihomo.exe      Mihomo（Clash Meta）内核，可选的第二个内核
  panel.conf      面板配置：监听地址 / 登录账户 / 密码
  start.bat       双击启动（会打开浏览器并显示初始密码）
  README.txt      本文件


三步用起来
----------
1. 解压到任意目录（路径别带空格问题不大，但建议放在 D:\sing-box-web 这类简单路径）。
   注意：不要在压缩包里直接双击，必须先解压。

2. 双击 start.bat。
   - 面板会在后台启动，浏览器自动打开 http://127.0.0.1:9090
   - 首次启动会自动生成随机密码，窗口里会打印出来（也存在 panel-data\initial-password）

3. 登录后：
   - 「订阅」页 → 添加订阅链接（支持 Clash / sing-box / v2ray 格式）
   - 「配置」页 → 生成配置 → 校验 → 应用
   - 之后把系统或浏览器的代理指向 127.0.0.1:7890 即可上网

默认代理端口
------------
  mixed（HTTP + SOCKS5）  127.0.0.1:7890
  局域网代理（可选）       需在「透明代理 / TUN」页手动开启，默认关闭


两个内核，随时切换
------------------
包里的 mihomo.exe 是可选的第二内核，默认仍然用 sing-box。切换入口在
「设置 → 内核」，点一下就换：

  sing-box  配置 config.json（JSON），校验用 sing-box check -c config.json
  mihomo    配置 config.yaml（Clash 系 YAML），校验用 mihomo -t -d <data> -f config.yaml

切换时面板会按新内核重新渲染一份配置，停旧核心 → 校验 → 启动 → 探活，
失败自动切回原内核。两个内核的配置、模板、快照互不覆盖，切回去还在。

节点只存一份，渲染时翻译成 Clash proxy，所以订阅解析、测速、选节点在两个内核下一致。

mihomo 第一次启动会下载约 12 MB 的 geo 数据（geosite.dat / country.mmdb），
内置模板已把 geox-url 指向 jsDelivr 镜像，国内也能下。校验超时放宽到 90 秒。
数据放在 panel-data\mihomo\ 下，和 sing-box 的 cache.db 分开放，不会互相锁文件。

想启动时强制指定内核（覆盖面板里存的选择），在命令行里跑：
  panel.exe -engine mihomo


常见问题
--------
· 想要整机接管（含不走系统代理的程序）
  右键 start.bat / panel.exe →「以管理员身份运行」，然后在「TUN 模式」页打开开关。
  非管理员运行时该开关会提示无权限，属正常。
  sing-box 的虚拟网卡叫 singbox-tun，mihomo 的叫 mihomo-tun；mihomo 自带 wintun，
  不需要另外放 wintun.dll。

· 想让手机 / 其它电脑也用这个代理
  把 panel.conf 里的 listen 改成 0.0.0.0:9090 并重启面板，
  然后在「透明代理 / TUN」页开启「局域网代理」并设置用户名密码
  （留空 = 无鉴权开放代理，同一网段任何人都能用，别在公共网络这么干）。

· 忘记密码
  编辑 panel.conf，把 password= 改成新密码后重启面板即可。
  注意：改 username 时必须同时填 password，否则本次修改会被忽略。

· Windows 提示「已保护你的电脑」/ SmartScreen
  这是未签名程序的常规提示，点「更多信息」→「仍要运行」。

· 面板占多少资源
  面板本体约 11–14 MB 内存，核心由内核自己决定（sing-box 通常 70–90 MB，
  mihomo 略高一些）。

· mihomo 显示「不可用」
  说明 mihomo.exe 不在面板同目录，或者跑不起来。用 mihomo.exe -v 验证，
  或用 -mihomo D:\path\mihomo.exe 指定路径。

· 卸载
  先在面板里「停止核心」，退出面板进程，然后直接删掉整个目录即可，不写注册表。


目录说明
--------
  panel-data\settings.json        面板设置（口令哈希、开关状态、当前内核）
  panel-data\template.json        sing-box 配置模板（进阶用户可改）
  panel-data\config.json          下发给 sing-box 的配置
  panel-data\template.yaml        mihomo 配置模板
  panel-data\config.yaml          下发给 mihomo 的配置
  panel-data\nodes-*.json         订阅解析出的节点（两个内核共用）
  panel-data\snapshots\           配置快照（默认保留 3 份，可回滚）
  panel-data\mihomo\              mihomo 工作目录（geo 数据、cache.db）
  panel-data\initial-password     首次启动生成的密码，登录后建议删除

更详细的说明见项目主页 README。
