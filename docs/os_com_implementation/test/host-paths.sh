#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
# Source from a host-side harness; documents now live inside the code repository.
OS_COM_IMPL_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
OS_COM_ROOT="${OS_COM_ROOT:-$(dirname -- "$(dirname -- "$OS_COM_IMPL_ROOT")")}"
# Legacy harness containers may mount the documentation separately at /root/impl.
if [[ ! -f "$OS_COM_ROOT/Cargo.toml" && -f /root/asterinas/Cargo.toml ]]; then
    OS_COM_ROOT=/root/asterinas
fi
