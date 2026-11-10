#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source "$SCRIPT_DIR/cpu_selection.sh"

root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
for queue in 0 1 2 3; do
    mkdir -p "$root/$queue"
done
printf '0, 1, 2, 3\n' >"$root/0/cpu_list"
printf '16, 17, 18, 19\n' >"$root/1/cpu_list"
printf '32, 33, 34, 35\n' >"$root/2/cpu_list"
printf '48, 49, 50, 51\n' >"$root/3/cpu_list"

[[ $(select_fio_cpus "$root" 1) == 0 ]]
[[ $(select_fio_cpus "$root" 2) == 0,16 ]]
[[ $(select_fio_cpus "$root" 4) == 0,16,32,48 ]]
[[ $(select_fio_cpus "$root" 8) == 0,16,32,48,1,17,33,49 ]]
[[ $(select_fio_cpus "$root" 16) == 0,16,32,48,1,17,33,49,2,18,34,50,3,19,35,51 ]]

if select_fio_cpus "$root" 0; then
    echo "accepted zero jobs" >&2
    exit 1
fi

echo "CPU selection fixtures passed"
