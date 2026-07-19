#!/usr/bin/env bash
# Thin shim — REUSE the canonical VMID allocator from the terraform-modules
# checkout instead of copying its ~140 lines here. The logic (live qm-list
# query, mkdir lock, PVE-host-keyed reservation file) lives in ONE place so
# every project targeting the same Proxmox node shares the same protection.
#
# Usage (same as the underlying script):
#   ./free-vmid.sh [min] [max]        # prints one free VMID (default 2000-2100)
#   ./free-vmid.sh --release <id>     # drop a reservation
#
# Override the checkout location with TERRAFORM_MODULES_DIR if it isn't the
# default sibling of this repo.
set -euo pipefail

MODULES_DIR="${TERRAFORM_MODULES_DIR:-$(cd "$(dirname "$0")/../../.." && pwd)/terraform-modules}"
SCRIPT="$MODULES_DIR/examples/terragrunt/get-free-vmid.sh"

if [ ! -f "$SCRIPT" ]; then
    echo "terraform-modules not found at $MODULES_DIR — cloning canonical copy..." >&2
    git clone git@github.com:glennswest/terraform-modules.git "$MODULES_DIR" >&2
fi

exec bash "$SCRIPT" "$@"
