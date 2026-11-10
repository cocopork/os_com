#!/bin/sh
# SPDX-License-Identifier: MPL-2.0

select_fio_cpus() {
    root=$1
    jobs=$2
    case $jobs in ''|*[!0-9]*|0) return 1 ;; esac

    selected=
    remaining=$jobs
    position=1
    while [ "$remaining" -gt 0 ]; do
        progressed=0
        for queue in "$root"/*; do
            [ -f "$queue/cpu_list" ] || continue
            cpu=$(tr -d ' ' <"$queue/cpu_list" | tr ',' '\n' | sed -n "${position}p")
            [ -n "$cpu" ] || continue
            if [ -n "$selected" ]; then
                selected="$selected,$cpu"
            else
                selected=$cpu
            fi
            remaining=$((remaining - 1))
            progressed=1
            [ "$remaining" -eq 0 ] && break
        done
        [ "$progressed" -eq 1 ] || return 1
        position=$((position + 1))
    done
    printf '%s\n' "$selected"
}
