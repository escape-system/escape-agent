#!/usr/bin/env bash
set -e
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
echo "==> Building Web UI..."
cd "$DIR/ui" && ./build.sh
echo "==> Building Rust release binary..."
cd "$DIR" && cargo build --release -p oc-cli
echo "==> Restarting escape-agent.service..."
systemctl restart escape-agent.service
echo "==> Status:"
systemctl status escape-agent.service --no-pager
