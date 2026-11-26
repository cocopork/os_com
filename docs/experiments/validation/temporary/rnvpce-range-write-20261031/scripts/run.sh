#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO=$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel)

export ART=${ART:?set ART to an external artifact directory}
export TARGET=${TARGET:-/dev/nvme1n1p1}
export TOOLS_IMAGE=${TOOLS_IMAGE:-os-com-recovery-tools:workspace-20261018}

[[ ${1:-} == --authorized-target ]] || {
    echo "pass --authorized-target after confirming the target may be erased" >&2
    exit 2
}
exec "$REPO/docs/experiments/validation/temporary/scalable-nvpage-pool-20261031/scripts/run.sh" \
    --authorized-target
