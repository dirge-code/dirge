#!/bin/bash
# Build the dirge microVM Alpine guest image.
# Prerequisites: buildah
set -euo pipefail
cd "$(dirname "$0")/../.."
echo "=== Building dirge-microvm:alpine ==="
buildah build --storage-driver vfs -t dirge-microvm:alpine -f images/alpine/Dockerfile .
echo "=== Done ==="
buildah images --storage-driver vfs | grep dirge-microvm
