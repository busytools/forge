# forged — daemon for forge-sdk over JSON-RPC + WebSocket

See `~/.claude-subspace/plans/2026-04-25-forged-wire-spec.md` for the
authoritative wire spec.

## Install on macOS

```bash
# Build + install the binary
cargo install --path . --root /usr/local

# Substitute the plist template
USERNAME=$(whoami)
WG_IP="10.x.x.x"          # your Studio's WireGuard address
sed -e "s/__USERNAME__/$USERNAME/g" -e "s/__WG_IP__/$WG_IP/g" \
    launchd/com.subspace.forged.plist \
    | sudo tee /Library/LaunchDaemons/com.subspace.forged.plist > /dev/null
sudo chown root:wheel /Library/LaunchDaemons/com.subspace.forged.plist
sudo chmod 644         /Library/LaunchDaemons/com.subspace.forged.plist

# Bootstrap + enable
sudo launchctl bootstrap system /Library/LaunchDaemons/com.subspace.forged.plist
sudo launchctl enable system/com.subspace.forged

# Verify
launchctl print system/com.subspace.forged
forged status
```

`KeepAlive` is on, so the daemon auto-restarts on crash. The
`ProgramArguments` shell wrapper polls for the WireGuard interface
address before `exec`ing the binary — keeps the launchd unit from
flapping when the laptop boots before the WG tunnel comes up.

## Caddy reverse proxy (optional, for TLS)

```caddy
forged.example.com {
  reverse_proxy <your-WG-IP>:7373
}
```

## Config

`~/.config/forged/forged.toml`:

```toml
bind = ["10.x.x.x:7373", "127.0.0.1:7373"]
log_dir = "~/Library/Logs/forged"
log_retention_days = 14
```

Each `bind` entry becomes its own listener task. Missing file → loopback
default (`127.0.0.1:7373`). Unknown keys are rejected at startup so
typos don't silently disable a setting.

## Logs

```
~/Library/Logs/forged/
  forged.events.log     # INFO+ structured events
  forged.errors.log     # WARN+ only
  forged.audit.log      # per-WS-connection records (target = forged::audit)
  forged.stdout.log     # pre-tracing-init stdout (launchd-captured)
  forged.stderr.log     # pre-tracing-init stderr (launchd-captured)
```

Daily rotation; 14-day retention by default. The retention sweep runs at
boot and only touches files prefixed `forged.` so unrelated content in
the log dir is left alone.

Override the live filter ad-hoc with `RUST_LOG=forged=debug` etc. — the
plist above sets `forged=info,forge_sdk=warn` as the default.
