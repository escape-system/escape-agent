#!/usr/bin/env bash
set -e
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
echo "==> Building Web UI..."
cd "$DIR/ui" && ./build.sh
echo "==> Building Rust release binary..."
cd "$DIR" && cargo build --release -p oc-cli --bin opencode
echo "==> Installing binary to /usr/local/bin/opencode..."
install -m 0755 "$DIR/target/release/opencode" /usr/local/bin/opencode.new
mv -f /usr/local/bin/opencode.new /usr/local/bin/opencode
ln -sf /usr/local/bin/opencode /usr/local/bin/escape-agent
echo "==> Restarting escape-agent.service..."
systemctl restart escape-agent.service
echo "==> Status:"
systemctl status escape-agent.service --no-pager
