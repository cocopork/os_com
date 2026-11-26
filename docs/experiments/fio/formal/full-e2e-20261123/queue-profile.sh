#!/bin/bash
# SPDX-License-Identifier: MPL-2.0

virtqueues_for_device() {
    local profile=$1 device=$2
    case "$profile:$device" in
        four:nvme|four:sata) printf '%s\n' 4 ;;
        device-aligned:nvme) printf '%s\n' 8 ;;
        device-aligned:sata) printf '%s\n' 1 ;;
        *) return 2 ;;
    esac
}

iothread_cpus_for_device() {
    local profile=$1 device=$2
    case "$profile:$device" in
        four:nvme|four:sata) printf '%s\n' '60 61 62 63' ;;
        device-aligned:nvme) printf '%s\n' '56 57 58 59 60 61 62 63' ;;
        device-aligned:sata) printf '%s\n' 63 ;;
        *) return 2 ;;
    esac
}
