#!/bin/bash
# SPDX-License-Identifier: MPL-2.0
#
# =========================================================================
#  方案 C：让 `asterinas-dev` 里的 harness 结果直接落回宿主
# =========================================================================
#
# 背景：`asterinas-dev` 只挂了 `os_com → /root/asterinas`，**没挂**
# `os_com_implementation`，所以它里面的 `/root/impl` 是容器可写层里的副本，
# 产物必须 `docker cp` 才回得来。
#
# Docker 不允许给**运行中**的容器新增挂载，所以这里借**已挂载的 os_com** 当跳板：
#   <宿主 os_com>/.impl-host  ==(bind mount)==  /root/asterinas/.impl-host
#   再在容器内:  mount --bind /root/asterinas/.impl-host /root/impl
# 于是容器写 `/root/impl/**` 会实时出现在宿主 `<os_com>/.impl-host/**`。
#
# 再把宿主侧 `os_com_implementation/test/aster-ext4-baseline-test/results`
# 换成指向 bridge 的**软链**，规范路径下就零拷贝地看到全部结果。
#
# 【语义与代价】
#   - `/root/impl` 下的**脚本是 bridge 里的副本**，不是 os_com_implementation 的实时
#     视图。**改了 `os_com_implementation` 下的脚本后要重跑本脚本**（或单独 tar 同步）。
#   - `results/` 例外：它是软链 → bridge，所以结果双向实时。
#   - 嵌套 bind **不持久**：容器重启/重建后要重跑本脚本（幂等，可直接重跑）。
#   - 回退：`docker exec asterinas-dev umount /root/impl`
#           `rm <宿主 results 软链> && mv <bridge results> 回原位`
#   - bridge 目录已加进 `os_com/.git/info/exclude`（不动受版本控制的 .gitignore）。
#
# 【用法】 bash setup-impl-bridge.sh
# =========================================================================
set -euo pipefail

source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/../host-paths.sh"

HOST_IMPL="${OS_COM_IMPL_ROOT}"
HOST_REPO="${OS_COM_ROOT}"
BRIDGE="$HOST_REPO/.impl-host"
CONTAINER=asterinas-dev
CTR_BRIDGE=/root/asterinas/.impl-host
SUB=test/aster-ext4-baseline-test

echo '--- ① 把容器 /root/impl 里宿主没有的文件补回归档（不覆盖宿主已有）---'
STAGE=/tmp/impl-dev-copy
rm -rf "$STAGE"; mkdir -p "$STAGE"
if docker exec "$CONTAINER" test -d /root/impl; then
    docker cp "$CONTAINER:/root/impl/." "$STAGE/" >/dev/null
fi
cp -an "$STAGE/$SUB/." "$HOST_IMPL/$SUB/" 2>/dev/null || true
echo "    归档里 $(basename "$HOST_IMPL/$SUB/results/20260923_133450_aster-ext4-baseline")/logs 下 extend 日志数: $(ls -1 "$HOST_IMPL/$SUB/results/20260923_133450_aster-ext4-baseline/logs" 2>/dev/null | grep -c extend || true)"

echo '--- ② 建 bridge 并从 os_com_implementation 同步脚本（排除 .git 与 results）---'
mkdir -p "$BRIDGE"
tar -C "$HOST_IMPL" --exclude=.git --exclude="$SUB/results" -cf - . | tar -C "$BRIDGE" -xf -
mkdir -p "$BRIDGE/$SUB"

echo '--- ③ results 归入 bridge，规范路径改为软链 ---'
if [ -L "$HOST_IMPL/$SUB/results" ]; then
    echo "    已是软链，跳过"
else
    mkdir -p "$BRIDGE/$SUB/results"
    if [ -d "$HOST_IMPL/$SUB/results" ]; then
        cp -an "$HOST_IMPL/$SUB/results/." "$BRIDGE/$SUB/results/" 2>/dev/null || true
        rm -rf "$HOST_IMPL/$SUB/results"
    fi
    ln -s "$BRIDGE/$SUB/results" "$HOST_IMPL/$SUB/results"
    echo "    已建: $HOST_IMPL/$SUB/results -> $BRIDGE/$SUB/results"
fi

echo '--- ④ 容器内嵌套 bind（幂等）---'
if docker exec "$CONTAINER" grep -q " /root/impl " /proc/mounts; then
    echo "    /root/impl 已挂载，跳过"
else
    docker exec "$CONTAINER" mount --bind "$CTR_BRIDGE" /root/impl
    echo "    已挂载 $CTR_BRIDGE -> /root/impl"
fi

echo '--- ⑤ 验证 ---'
docker exec "$CONTAINER" sh -c "ls /root/impl/$SUB/"
echo "    bridge 侧 results: $(ls -1 "$BRIDGE/$SUB/results" | wc -l) 个条目"
# 软链只架在 results/ 上，所以探针必须写进 results/ 才能验证到"规范路径"那一段。
docker exec "$CONTAINER" sh -c "touch /root/impl/$SUB/results/.bind-probe"
echo "    探针 bridge 侧: $(ls "$BRIDGE/$SUB/results/.bind-probe" 2>/dev/null || echo '缺失!')"
echo "    探针规范路径:   $(ls "$HOST_IMPL/$SUB/results/.bind-probe" 2>/dev/null || echo '缺失!')"
rm -f "$BRIDGE/$SUB/results/.bind-probe"

echo '--- ⑥ 让 git 忽略 bridge ---'
if grep -qxF '.impl-host/' "$HOST_REPO/.git/info/exclude" 2>/dev/null; then
    echo "    已忽略"
else
    echo '.impl-host/' >> "$HOST_REPO/.git/info/exclude"
    echo "    已加入 $HOST_REPO/.git/info/exclude"
fi

echo
echo '完成。以后在 asterinas-dev 里跑 harness 的结果会直接出现在：'
echo "  $HOST_IMPL/$SUB/results/  (软链 -> $BRIDGE/$SUB/results)"
