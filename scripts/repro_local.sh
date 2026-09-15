#!/usr/bin/env bash
# Repro / diagnostic script for local testing
# Usage: ./scripts/repro_local.sh [--iterations N] [--url URL] [--capture]
# Default: iterations=200, url=https://www.baidu.com, capture=false
#
# For example, `bash ./scripts/repro_local.sh --iterations 100`
#

set -euo pipefail
IFS=$'\n\t'

REPO_DIR=$(cd "$(dirname "$0")/.." && pwd)
LOGDIR="$REPO_DIR/debug/repro_$(date +%Y%m%dT%H%M%S)"
mkdir -p "$LOGDIR"

# Detect ../debug/server.crt etc exist
# If missing, generate certificates using scripts/selfsign.sh
if [[ ! -f "$REPO_DIR/debug/server.crt" || ! -f "$REPO_DIR/debug/server.key" || ! -f "$REPO_DIR/debug/root.crt" ]]; then
  echo "Certs missing; generating self-signed certs via scripts/selfsign.sh" | tee -a "$LOGDIR/run.log"
  mkdir -p "$REPO_DIR/debug"
  cd "$REPO_DIR/debug"
  if bash "$REPO_DIR/scripts/selfsign.sh" CN JiangSu ChangZhou MyGreatOrg Root_CA Server1 email@example.com example.com 123.45.67.89 >>"$LOGDIR/run.log" 2>&1; then
    echo "Self-signed certs created." | tee -a "$LOGDIR/run.log"
  else
    echo "Failed to create self-signed certs; aborting." | tee -a "$LOGDIR/run.log"
    exit 1
  fi
fi

ITERATIONS=200
URL="https://www.baidu.com"
CAPTURE=false
SLEEP_BETWEEN=0.05

while [[ $# -gt 0 ]]; do
  case $1 in
    --iterations) ITERATIONS="$2"; shift 2 ;;
    --url) URL="$2"; shift 2 ;;
    --capture) CAPTURE=true; shift 1 ;;
    --help) echo "Usage: $0 [--iterations N] [--url URL] [--capture]"; exit 0 ;;
    *) echo "Unknown arg: $1"; exit 1 ;;
  esac
done

echo "Repro: iterations=$ITERATIONS url=$URL capture=$CAPTURE"

# Commands (customize if desired)
SERVER_LOG="$LOGDIR/server.log"
CLIENT_LOG="$LOGDIR/client.log"
CURL_LOG="$LOGDIR/curl_results.txt"
PCAP_FILE="$LOGDIR/capture.pcap"

cleanup() {
  echo "Cleaning up..." >&2
  if [[ -n "${TCPDUMP_PID:-}" ]]; then
    kill "$TCPDUMP_PID" 2>/dev/null || true
  fi
  if [[ -n "${CLIENT_PID:-}" ]]; then
    kill "$CLIENT_PID" 2>/dev/null || true
  fi
  if [[ -n "${SERVER_PID:-}" ]]; then
    kill "$SERVER_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

cd "$REPO_DIR"

echo "Installing anytls-rs locally (may take time)..." | tee -a "$LOGDIR/run.log"
cargo install --path "$REPO_DIR" | tee -a "$LOGDIR/run.log"

# Start server
echo "Starting server..." | tee "$LOGDIR/run.log"
RUST_LOG=debug anytls-server -l 0.0.0.0:443 -p password --cert "$REPO_DIR/debug/server.crt" --key "$REPO_DIR/debug/server.key" >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!
sleep 1

echo "Server PID=$SERVER_PID, log=$SERVER_LOG"

# Start client
echo "Starting client..." | tee -a "$LOGDIR/run.log"
RUST_LOG=debug anytls-client -l socks5://127.0.0.1:2080 -s 127.0.0.1:443 -p password --root-cert "$REPO_DIR/debug/root.crt" --sni example.com >"$CLIENT_LOG" 2>&1 &
CLIENT_PID=$!
sleep 1

echo "Client PID=$CLIENT_PID, log=$CLIENT_LOG"

# Optionally capture network traffic on loopback (requires sudo or CAP_NET_RAW)
if $CAPTURE; then
  echo "Starting tcpdump (loopback) to $PCAP_FILE" | tee -a "$LOGDIR/run.log"

  if ! command -v tcpdump >/dev/null 2>&1; then
    echo "tcpdump not found; skipping capture. Install tcpdump or grant capabilities (setcap cap_net_raw,cap_net_admin+e /usr/sbin/tcpdump)." | tee -a "$LOGDIR/run.log"
  else
    # Try running a short capture without sudo to detect if capabilities are available
    if tcpdump -i lo tcp and \(port 443 or port 2080\) -c 1 -w "$LOGDIR/.cap_test.pcap" >/dev/null 2>&1; then
      tcpdump -i lo tcp and \(port 443 or port 2080\) -w "$PCAP_FILE" 2>/dev/null &
      TCPDUMP_PID=$!
      echo "tcpdump PID=$TCPDUMP_PID (no sudo)" | tee -a "$LOGDIR/run.log"
    else
      # Try non-interactive sudo; if not available, print instructions and skip
      if sudo -n true 2>/dev/null; then
        sudo tcpdump -i lo tcp and \(port 443 or port 2080\) -w "$PCAP_FILE" 2>/dev/null &
        TCPDUMP_PID=$!
        echo "tcpdump PID=$TCPDUMP_PID (via sudo)" | tee -a "$LOGDIR/run.log"
      else
        echo "tcpdump requires sudo or CAP_NET_RAW. Run 'sudo -v' to cache credentials or set capabilities on tcpdump. Skipping capture." | tee -a "$LOGDIR/run.log"
      fi
    fi
  fi
fi

# Wait a little for processes to be fully up
sleep 1

# Run curl loop
echo "Starting curl loop: $ITERATIONS iterations -> $CURL_LOG" | tee -a "$LOGDIR/run.log"
>"$CURL_LOG"
for i in $(seq 1 "$ITERATIONS"); do
  TS=$(date +%s.%N)
  if curl -sS --socks5 127.0.0.1:2080 -I "$URL" -m 15 >/dev/null 2>&1; then
    echo "$TS $i:0" >> "$CURL_LOG"
  else
    echo "$TS $i:1" >> "$CURL_LOG"
    # dump short context logs for quick debugging
    echo "--- FAILED iteration $i at $TS ---" >> "$LOGDIR/failures_tail.log"
    echo "--- server.log tail ---" >> "$LOGDIR/failures_tail.log"
    tail -n 200 "$SERVER_LOG" >> "$LOGDIR/failures_tail.log"
    echo "--- client.log tail ---" >> "$LOGDIR/failures_tail.log"
    tail -n 200 "$CLIENT_LOG" >> "$LOGDIR/failures_tail.log"
  fi
  sleep "$SLEEP_BETWEEN"
done

# Allow logs to settle
sleep 1

# Gracefully stop
echo "Stopping client and server" | tee -a "$LOGDIR/run.log"
kill "$CLIENT_PID" 2>/dev/null || true
kill "$SERVER_PID" 2>/dev/null || true
sleep 1

if [[ -n "${TCPDUMP_PID:-}" ]]; then
  sudo kill "$TCPDUMP_PID" 2>/dev/null || true
fi

# Summarize results
if [[ -f "$CURL_LOG" ]]; then
  TOTAL=$(wc -l < "$CURL_LOG" 2>/dev/null | tr -d '[:space:]')
  FAILS=$(grep -c ":1$" "$CURL_LOG" 2>/dev/null || echo 0)
else
  TOTAL=0
  FAILS=0
fi
TOTAL=${TOTAL:-0}
FAILS=${FAILS:-0}
# Normalize to digits only (strip any stray whitespace or other chars)
TOTAL=$(echo "$TOTAL" | tr -dc '0-9')
FAILS=$(echo "$FAILS" | tr -dc '0-9')
TOTAL=${TOTAL:-0}
FAILS=${FAILS:-0}
SUCC=$((TOTAL - FAILS))
PCT=$(awk -v f="$FAILS" -v t="$TOTAL" 'BEGIN{if(t==0) t=1; printf "%.2f", (f/t)*100}')

cat <<EOF | tee "$LOGDIR/summary.txt"
Repro summary
=============
logdir: $LOGDIR
iterations: $ITERATIONS
url: $URL
total: $TOTAL
success: $SUCC
fails: $FAILS
fail pct: $PCT%
EOF

# Show quick grep of suspicious log messages
echo "--- Recent server errors (grep) ---" | tee -a "$LOGDIR/summary.txt"
grep -nE "Failed|error|UnexpectedEof|close_notify|Stream .* close|Channel closed|Broken pipe" "$SERVER_LOG" || true >> "$LOGDIR/summary.txt"

echo "--- Recent client errors (grep) ---" | tee -a "$LOGDIR/summary.txt"
grep -nE "Failed|error|UnexpectedEof|close_notify|Stream .* close|Channel closed|Broken pipe" "$CLIENT_LOG" || true >> "$LOGDIR/summary.txt"

echo "Done. Logs: $LOGDIR"

# Exit with non-zero if any failures
if [[ "$FAILS" -gt 0 ]]; then
  exit 1
fi
exit 0
