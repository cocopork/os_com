#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
[[ $# -eq 3 ]] || { echo "usage: $0 SYSFS_MQ_ROOT QUEUES ALLOWED_CPU_LIST" >&2; exit 2; }
python3 "$SCRIPT_DIR/summarize.py" select-hctx-cpus "$1" "$2" "$3"
