#!/usr/bin/env bash
# Let the camera assign our address via DHCP (waiting up to 90 s), then try the handshake.
# Usage: scripts/diagnose-dhcp.sh [camera-connection] [home-connection]
CAM=${1:-FUJIFILM-X-E3-175A}
HOME_NET=${2:-JEES}
BIN="$(dirname "$0")/../target/release/openfuji-cli"
OUT=~/openfuji-diagnose-dhcp.txt
{
echo "### $(date)  camera=$CAM (DHCP)"
nmcli connection modify "$CAM" ipv4.method auto ipv4.addresses "" ipv4.dhcp-timeout 90 \
  ipv4.never-default yes ipv6.method disabled connection.autoconnect no
START=$(date +%s)
nmcli --wait 100 connection up "$CAM"; echo "nmcli exit $? after $(( $(date +%s) - START ))s"
ip -4 addr show wlan0 | grep inet
ip route | grep -E '192\.168\.0\.'
for i in 1 2 3; do timeout 2 bash -c '</dev/tcp/192.168.0.1/55740' && echo "port 55740 reachable" && break; sleep 1; done
echo; echo "### handshake (60 s)"
timeout 90 "$BIN" --ip 192.168.0.1 --handshake-timeout 60 --busy-attempts 2 -vv info 2>&1 | head -120
echo; echo "### NetworkManager log"
journalctl -u NetworkManager --since "-3 min" --no-pager 2>/dev/null | grep -iE 'dhcp|wlan0|fuji' | tail -30
} 2>&1 | tee "$OUT"
# Put the static address back so the next attempt doesn't hang.
nmcli connection modify "$CAM" ipv4.method manual ipv4.addresses 192.168.0.2/24
nmcli connection up "$HOME_NET" >/dev/null 2>&1 && echo "back on $HOME_NET"
echo "Saved to $OUT"
