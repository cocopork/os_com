#!/bin/sh
# SPDX-License-Identifier: MPL-2.0
set -eu

log=$1
for metric in \
    native_foreground_total \
    native_foreground_body \
    native_flush_total \
    native_completion_wait \
    nv_foreground_total \
    nv_foreground_persist \
    nv_flush_total \
    nv_completion_total \
    nv_async_ready_wait
do
    grep -q "\[PIPELINE\] name=$metric " "$log"
done
