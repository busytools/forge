#!/usr/bin/env bash
# Boot two mitmproxies (native on 9001, alt on 9002) writing to /tmp/forge-wire-check/.
# Idempotent: kills any prior instances first. Generates mitmproxy CA on first run.
set -euo pipefail

WORK=/tmp/forge-wire-check
mkdir -p "$WORK"

# Generate mitmproxy CA if not yet present (first run on this machine)
if [ ! -f "$HOME/.mitmproxy/mitmproxy-ca-cert.pem" ]; then
    echo "Generating mitmproxy CA (first run)..."
    mitmdump --listen-port 19999 --quiet > /dev/null 2>&1 &
    GEN_PID=$!
    sleep 2
    kill "$GEN_PID" 2>/dev/null || true
    wait "$GEN_PID" 2>/dev/null || true
fi

# Kill any prior wire-check mitmproxies
pkill -f 'mitmdump.*--listen-port 900[12]' 2>/dev/null || true
sleep 1

# Fresh flow files
rm -f "$WORK"/flows-native.mitm "$WORK"/flows-alt.mitm

# Boot the two proxies
nohup mitmdump --listen-port 9001 -w "$WORK/flows-native.mitm" \
    --set flow_detail=0 \
    > "$WORK/mitm-native.log" 2>&1 &
NATIVE_PID=$!
echo "native  mitmdump (port 9001) pid: $NATIVE_PID"

nohup mitmdump --listen-port 9002 -w "$WORK/flows-alt.mitm" \
    --set flow_detail=0 \
    > "$WORK/mitm-alt.log" 2>&1 &
ALT_PID=$!
echo "forge   mitmdump (port 9002) pid: $ALT_PID"

# Wait for both to actually be listening
for _ in 1 2 3 4 5 6 7 8 9 10; do
    sleep 0.3
    if lsof -iTCP:9001 -sTCP:LISTEN > /dev/null 2>&1 && \
       lsof -iTCP:9002 -sTCP:LISTEN > /dev/null 2>&1; then
        echo "Both proxies listening."
        echo
        echo "Native capture → $WORK/flows-native.mitm"
        echo "Forge  capture → $WORK/flows-alt.mitm"
        echo
        echo "Run native in one pane with HTTPS_PROXY=http://127.0.0.1:9001"
        echo "Run forge in another pane with HTTPS_PROXY=http://127.0.0.1:9002"
        echo "Use NODE_EXTRA_CA_CERTS=\$HOME/.mitmproxy/mitmproxy-ca-cert.pem (plus SSL_CERT_FILE / CURL_CA_BUNDLE / REQUESTS_CA_BUNDLE pointing to the same path)"
        echo
        echo "When both sessions have exited cleanly, stop proxies with:"
        echo "  pkill -INT -f 'mitmdump.*--listen-port 900' && sleep 2"
        exit 0
    fi
done

echo "ERROR: one or both proxies failed to start listening" >&2
exit 1
