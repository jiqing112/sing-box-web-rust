#!/bin/sh
# sing-box panel 一键安装（Debian / Ubuntu / 其它 systemd 发行版）
# 用法：sudo ./install.sh
set -eu

PREFIX="${PREFIX:-/opt/sing-box-panel}"
HERE=$(cd "$(dirname "$0")" && pwd)

[ "$(id -u)" = "0" ] || { echo "请用 root 运行：sudo $0" >&2; exit 1; }
command -v systemctl >/dev/null 2>&1 || { echo "未检测到 systemd" >&2; exit 1; }

echo "==> 安装到 $PREFIX"
install -d -m 755 "$PREFIX/bin" "$PREFIX/data"

# 已存在同名进程时先停掉，避免旧二进制被占用
if systemctl is-active --quiet sing-box-panel 2>/dev/null; then
  echo "==> 停止已运行的 sing-box-panel"
  systemctl stop sing-box-panel
fi

install -m 755 "$HERE/panel" "$PREFIX/bin/panel"
install -m 755 "$HERE/sing-box" /usr/local/bin/sing-box

# mihomo 是可选的第二个内核：包里有就装，没有（老包）就跳过，
# 面板照常用 sing-box，只是「设置 → 内核」里 mihomo 显示不可用。
if [ -f "$HERE/mihomo" ]; then
  install -m 755 "$HERE/mihomo" /usr/local/bin/mihomo
  echo "==> 已安装 mihomo，可在面板「设置 → 内核」里切换"
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
