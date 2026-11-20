#!/bin/sh
# sing-box panel 一键安装（Debian / Ubuntu / 其它 systemd 发行版）
# 用法：sudo ./install.sh
set -eu

PREFIX="${PREFIX:-/opt/sing-box-panel}"
HERE=$(cd "$(dirname "$0")" && pwd)

[ "$(id -u)" = "0" ] || { echo "请用 root 运行：sudo $0" >&2; exit 1; }
command -v systemctl >/dev/null 2>&1 || { echo "未检测到 systemd" >&2; exit 1; }

echo "==> 环境检查"
# 透明代理 / TUN 虚拟网卡 / 接管本机 53 端口都依赖 root：
# systemd 服务默认以 root 运行；手动调试也请 sudo 启动面板，
# 用普通用户跑起来的面板这些功能全部不可用。
NFT_BIN=""
for c in /usr/sbin/nft /sbin/nft nft; do
  if command -v "$c" >/dev/null 2>&1; then NFT_BIN="$c"; break; fi
done
if [ -z "$NFT_BIN" ]; then
  echo "    [提示] 未找到 nft 命令：透明代理不可用。"
  echo "           Debian/麒麟: sudo apt install nftables"
  echo "           RHEL/欧拉系: sudo dnf install nftables"
else
  echo "    nftables: $NFT_BIN ✓"
fi
if [ ! -c /dev/net/tun ]; then
  echo "    [提示] 缺少 /dev/net/tun：TUN 虚拟网卡不可用。可尝试："
  echo "           mkdir -p /dev/net && mknod /dev/net/tun c 10 200 && chmod 666 /dev/net/tun"
else
  echo "    /dev/net/tun ✓"
fi

echo "==> 安装到 $PREFIX"
install -d -m 755 "$PREFIX/bin" "$PREFIX/data"

# 已存在的实例先停掉，避免旧二进制被占用 / 端口被占导致服务起不来：
# 1) systemd 服务里的旧实例
if systemctl is-active --quiet sing-box-panel 2>/dev/null; then
  echo "==> 停止已运行的 sing-box-panel 服务"
  systemctl stop sing-box-panel
fi
# 2) 手动启动的 panel（之前 ./panel 或 sudo ./panel 跑的实例）——
#    不停掉它会一直占着 9090 端口，装完服务后「面板无法启动」
MANUAL_PIDS=$(pgrep -x panel 2>/dev/null || true)
if [ -n "$MANUAL_PIDS" ]; then
  echo "==> 检测到手动运行的 panel 进程（pid: $(echo $MANUAL_PIDS | tr '\n' ' ')），停止它们"
  kill $MANUAL_PIDS 2>/dev/null || true
  sleep 1
  kill -9 $MANUAL_PIDS 2>/dev/null || true
fi

install -m 755 "$HERE/panel" "$PREFIX/bin/panel"

# sing-box 与 mihomo 都是可选内核：分包（-with-sing-box / -with-mihomo）
# 里只有其一，包里有就装，没有就跳过——set -e 下 install 一个不存在的
# 文件会让整个脚本半途而废（panel 已装好，service 却没装上）。
if [ -f "$HERE/sing-box" ]; then
  install -m 755 "$HERE/sing-box" /usr/local/bin/sing-box
fi

if [ -f "$HERE/mihomo" ]; then
  install -m 755 "$HERE/mihomo" /usr/local/bin/mihomo
  echo "==> 已安装 mihomo，可在面板「设置 → 内核」里切换"
fi

# 数据目录可能被非 root 手动运行写过（文件属主是普通用户）。服务以 root
# 运行本身不受影响，但残留的非 root 属主会在排查权限问题时误导人，统一收归 root。
if [ -d "$PREFIX/data" ]; then
  NONROOT=$(find "$PREFIX/data" ! -user root 2>/dev/null | head -1 || true)
  if [ -n "$NONROOT" ]; then
    echo "==> 数据目录存在非 root 属主文件，已统一改为 root（服务以 root 运行）"
    chown -R root:root "$PREFIX/data"
  fi
fi

# panel.conf 只在缺失时安装，避免覆盖用户改过的监听地址 / 口令
if [ ! -f "$PREFIX/bin/panel.conf" ]; then
  install -m 600 "$HERE/panel.conf" "$PREFIX/bin/panel.conf"
  echo "==> 已安装默认 panel.conf（listen=127.0.0.1:9090，可自行修改）"
fi

install -m 644 "$HERE/sing-box-panel.service" /etc/systemd/system/sing-box-panel.service
systemctl daemon-reload
systemctl enable --now sing-box-panel

sleep 2
echo
echo "==> 服务状态"
systemctl --no-pager --lines=6 status sing-box-panel || true

echo
IP=$(hostname -I 2>/dev/null | awk '{print $1}')
echo "面板地址: http://${IP:-<本机IP>}:9090"
if [ ! -f "$PREFIX/data/settings.json" ]; then
  echo "首次使用：打开面板网页，按引导设置登录账户密码"
  echo "（自动化部署可在 panel.conf 填 password= 跳过网页初始化）"
fi
echo
echo "常用命令："
echo "  systemctl restart sing-box-panel     # 重启面板（会连带重启当前内核）"
echo "  journalctl -u sing-box-panel -f      # 看日志"
echo "  mihomo -v / sing-box version         # 看两个内核的版本"
echo
echo "权限说明：服务以 root 运行，透明代理 / TUN / 接管 53 端口都依赖 root。"
echo "临时手动调试请先停服务再 sudo 启动，用完记得恢复服务："
echo "  systemctl stop sing-box-panel && sudo $PREFIX/bin/panel -listen 0.0.0.0:9090"
echo "  调试完成后：sudo systemctl start sing-box-panel"
echo "（普通用户身份启动的面板，透明代理、TUN、接管 53 端口均不可用）"
