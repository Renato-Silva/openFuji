#!/usr/bin/env bash
# Collect connection diagnostics from a Fujifilm camera, then return to the home Wi-Fi.
# Usage: scripts/diagnose-x-e3.sh [camera-connection] [home-connection]
# Start "Wireless communication" on the camera before running.
CAM=${1:-FUJIFILM-X-E3-175A}
HOME_NET=${2:-JEES}
IP=192.168.0.1
BIN="$(dirname "$0")/../target/release/openfuji-cli"
OUT=~/openfuji-diagnose.txt

{
echo "### $(date)  camera=$CAM"
nmcli connection up "$CAM" || { echo "could not join $CAM"; }
sleep 2
ip -4 addr show | grep -E 'inet 192\.168\.0\.'

echo; echo "### open TCP ports on the camera"
for p in 80 443 5000 8080 15740 51540 51541 51542 51560 51561 51562 55740 55741 55742; do
  if timeout 2 bash -c "</dev/tcp/$IP/$p" 2>/dev/null; then echo "port $p OPEN"; else echo "port $p closed"; fi
done

echo; echo "### does the camera send anything first? (5 s, no data sent)"
timeout 5 nc "$IP" 55740 </dev/null | od -A x -t x1z | head -20
echo "(end)"

for variant in "default" "name=Pixel 9" "random-guid"; do
  echo; echo "### handshake variant: $variant"
  args=(--ip "$IP" --handshake-timeout 20 --busy-attempts 2 -vv)
  case "$variant" in
    "name=Pixel 9") args+=(--name "Pixel 9") ;;
    random-guid) args+=(--guid "$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')") ;;
  esac
  timeout 60 "$BIN" "${args[@]}" info 2>&1 | head -80
  sleep 2
done
} 2>&1 | tee "$OUT"

nmcli connection up "$HOME_NET" >/dev/null 2>&1 && echo "back on $HOME_NET"
echo "Saved to $OUT"
