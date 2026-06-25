#!/usr/bin/env bash
# Rebuild + restart the openproxy service.
# Usage: ./bin/rebuild.sh
set -euo pipefail
cd "$(dirname "$0")/.."

echo "==> Building (release)..."
cargo build --workspace --release

echo "==> Restarting services..."
systemctl restart openproxy.service

sleep 2
echo "==> Status:"
systemctl is-active openproxy.service
echo "==> Health:"
curl -s http://localhost:8787/v1/health
echo ""
curl -s -o /dev/null -w "dashboard HTTP %{http_code}\n" http://localhost:8787/admin/
