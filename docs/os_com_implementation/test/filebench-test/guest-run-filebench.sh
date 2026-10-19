#!/bin/sh
# SPDX-License-Identifier: MPL-2.0
#
# =========================================================================
#  宏测试 guest 侧：filebench 在 ext4 / rNVPC / rNVPC-extend 上的吞吐
# =========================================================================
#
# 由宿主 harness（run-filebench.sh）在每轮前安装为
#   /benchmark/filebench/aster_ext4_macro/run.sh
# 并由 benchmark 框架以 BENCHMARK_ARGS 传参（**最多 10 个**，第 10 个可用）。
#
# 【参数】
#   $1  DEV_TAG   设备标签（sata|nvme）——仅用于日志标识，真正的盘由宿主
#                 XFSTESTS_TEST_IMAGE 挂成 /dev/vdc
#   $2  FS         ext4|rnvpc|rnvpc_extend
#   $3  WORKLOAD   write-heavy|fileserver|varmail|seqwrite
#   $4  THREADS    filebench $nthreads
#   $5  RUNTIME    filebench `run` 秒数
#   $6  NFILES     fileset entries（0 = 用 workload 默认）
#   $7  FILESIZE   fileset size（如 64k；空 = 用默认）
#   $8  NJMB       nojournal_meta_batch: on|global|off（仅 nojournal 档有意义）
#   $9  NJWB       nojournal_meta_writeback: on|sync|off（同上）
#   $10 WRCAP_MB   writeback_capacity_mb（dirty 预算 MB）
#
# 【为什么在 guest 里生成 .f】filebench 的变量没有可靠的命令行覆盖接口，而
# benchmark 框架的参数只能经 BENCHMARK_ARGS 传（且每轮重建 initramfs 太慢）。
# 所以 workload 模板作为 heredoc 内嵌在本脚本里，运行时就地渲染到 /tmp 再跑。
# 各 .f 的原始出处见 README.md。
#
# 【产出标记】宿主解析只看这两行（自描述，不依赖轮次顺序）：
#   === FILEBENCH_PARAM:  dev=.. fs=.. workload=.. threads=.. runtime=.. ...
#   === FILEBENCH_RESULT: <tag> status=ok|crash|raw ... ops_per_sec=.. mb_per_sec=..
#
# 【退出码】filebench 非 0 退出**不**让本脚本失败：宏测试关心的是"跑出了什么"，
# 崩了也要把已有的数带回去（status=crash）。
# =========================================================================

DEV_TAG="${1:-dut}"
FS="${2:-ext4}"
WORKLOAD="${3:-write-heavy}"
THREADS="${4:-4}"
RUNTIME="${5:-30}"
NFILES="${6:-0}"
FILESIZE="${7:-}"
NJMB="${8:-on}"
NJWB="${9:-on}"
WRCAP_MB="${10:-256}"

# filebench 二进制：**优先用随 job 打包的 1.5-alpha3**（research1 自建，解释器/RUNPATH
# 指向 /nix/store/...glibc-2.40-66，与 fio 同一套）。
# 不能不用 nixpkgs 的 1.4.9.1：它在当前环境 worker 跑空（IO Summary 全是 0 ops）。
FBDIR="/benchmark/filebench/aster_ext4_macro"
if [ -x "${FBDIR}/bin/filebench" ]; then
    FILEBENCH="${FBDIR}/bin/filebench"
else
    FILEBENCH="/benchmark/bin/filebench"
fi
MKE2FS="${FBDIR}/mkfs/mke2fs"
MNT="/mnt/dut"
DEV="/dev/vdc"
WL="/tmp/wl.f"
FB_OUT="/tmp/filebench.out"

# 与 fio harness 的 guest 脚本**逐字一致**（四档共用同一 mkfs 参数），
# 否则宏测试与 fio 的带宽结果不在同一口径上。
MKE2FS_ARGS="-F -q -t ext4 -b 4096 -I 256 -E nodiscard -O extent,filetype,^metadata_csum,^dir_index,^64bit,^flex_bg,^inline_data,has_journal"

case "$FS" in
    ext4)
        MOUNT_OPTS="writeback_capacity_mb=${WRCAP_MB}"
        ;;
    ext2like)
        MKE2FS_ARGS="-F -q -t ext4 -b 4096 -I 256 -E nodiscard -O extent,filetype,^metadata_csum,^dir_index,^64bit,^flex_bg,^inline_data,^has_journal"
        MOUNT_OPTS="writeback_capacity_mb=${WRCAP_MB}"
        ;;
    ext4_nomerge)
        MOUNT_OPTS="writeback_capacity_mb=${WRCAP_MB},wb_merge=off"
        ;;
    ext2like_nomerge)
        MKE2FS_ARGS="-F -q -t ext4 -b 4096 -I 256 -E nodiscard -O extent,filetype,^metadata_csum,^dir_index,^64bit,^flex_bg,^inline_data,^has_journal"
        MOUNT_OPTS="writeback_capacity_mb=${WRCAP_MB},wb_merge=off"
        ;;
    ext4_sync)
        MOUNT_OPTS="writeback_capacity_mb=${WRCAP_MB},wb_sync_wait=on"
        ;;
    ext2like_sync)
        MKE2FS_ARGS="-F -q -t ext4 -b 4096 -I 256 -E nodiscard -O extent,filetype,^metadata_csum,^dir_index,^64bit,^flex_bg,^inline_data,^has_journal"
        MOUNT_OPTS="writeback_capacity_mb=${WRCAP_MB},wb_sync_wait=on"
        ;;
    rnvpc)
        MOUNT_OPTS="writeback_capacity_mb=${WRCAP_MB},rnvpc=on"
        ;;
    rnvpc_extend)
        MOUNT_OPTS="writeback_capacity_mb=${WRCAP_MB},rnvpc_extend=on"
        ;;
    *)
        echo "[filebench] unknown_fs fs=$FS" >&2
        exit 2
        ;;
esac

case "$NJMB" in
    on|owner|2) NJMB_VALUE=2 ;;
    global|1)   NJMB_VALUE=1 ;;
    off|0)      NJMB_VALUE=0 ;;
    *)          NJMB_VALUE=2 ;;
esac
case "$NJWB" in
    off|0)      NJWB_VALUE=0 ;;
    sync|1)     NJWB_VALUE=1 ;;
    asyncbio|bio|3) NJWB_VALUE=3 ;;
    *)          NJWB_VALUE=2 ;;
esac
MOUNT_OPTS="${MOUNT_OPTS},nojournal_meta_batch=${NJMB_VALUE}"
MOUNT_OPTS="${MOUNT_OPTS},nojournal_meta_writeback=${NJWB_VALUE}"

now_s() { awk '{printf "%.3f", $1}' /proc/uptime; }

echo "=== FILEBENCH_PARAM: dev=${DEV_TAG} fs=${FS} workload=${WORKLOAD} threads=${THREADS} runtime=${RUNTIME} nfiles=${NFILES} filesize=${FILESIZE:-default} mount_opts=${MOUNT_OPTS} ==="

# ---- 格式化 + 挂载 ------------------------------------------------------
mkdir -p "$MNT"
umount "$MNT" 2>/dev/null || true
if [ -x "$MKE2FS" ]; then
    "$MKE2FS" $MKE2FS_ARGS "$DEV" || { echo "[filebench] mke2fs failed" >&2; exit 3; }
else
    echo "WARNING: static mke2fs missing at $MKE2FS; fallback to rm-clean" >&2
    rm -rf "${MNT:?}"/* 2>/dev/null || true
fi
mount -t ext4 -o "$MOUNT_OPTS" "$DEV" "$MNT" || { echo "[filebench] mount failed: $MOUNT_OPTS" >&2; exit 3; }

# ---- 渲染 workload ------------------------------------------------------
# 占位符：@DIR@ @NTHREADS@ @RUNTIME@ @NFILES@ @FILESIZE@
render() {
    sed -e "s|@DIR@|$MNT/fb|g" \
        -e "s|@NTHREADS@|$THREADS|g" \
        -e "s|@RUNTIME@|$RUNTIME|g" \
        -e "s|@NFILES@|$1|g" \
        -e "s|@FILESIZE@|$2|g" > "$WL"
}

# workload 默认参数（NFILES/FILESIZE 传 0/空时用这里的值）
case "$WORKLOAD" in
    write-heavy) D_NFILES=2048; D_FSIZE=64k ;;
    fileserver)  D_NFILES=3000; D_FSIZE=128k ;;
    varmail)     D_NFILES=1000; D_FSIZE=16k ;;
    seqwrite)    D_NFILES=1;     D_FSIZE=1024m ;;
    *) echo "[filebench] unknown_workload workload=$WORKLOAD (expect write-heavy|fileserver|varmail|seqwrite)" >&2; exit 2 ;;
esac
N_FILES="${NFILES:-0}"; [ "$N_FILES" = "0" ] && N_FILES="$D_NFILES"
# 宿主用 `-` 表示"用 workload 默认值"（不能传空串：空参数会被框架吞掉/错位）。
N_FSIZE="$FILESIZE"
if [ -z "$N_FSIZE" ] || [ "$N_FSIZE" = "-" ]; then N_FSIZE="$D_FSIZE"; fi

mkdir -p "$MNT/fb"

# write-heavy：移植 research1/implementation/testing/writeback-physical-view/
#   workloads/filebench-write-heavy.f（结构逐字保留，变量参数化）。
if [ "$WORKLOAD" = "write-heavy" ]; then
render "$N_FILES" "$N_FSIZE" <<'EOF'
set $dir=@DIR@
set $nfiles=@NFILES@
set $meandirwidth=32
set $filesize=@FILESIZE@
set $nthreads=@NTHREADS@
set $iosize=16k
set $appendsize=16k
set $runtime=@RUNTIME@

define fileset name=writefiles,path=$dir,size=$filesize,entries=$nfiles,dirwidth=$meandirwidth,prealloc=80

define process name=writers,instances=1
{
  thread name=writerthread,memsize=4m,instances=$nthreads
  {
    flowop createfile name=create-file,filesetname=writefiles,fd=1
    flowop writewholefile name=write-file,srcfd=1,fd=1,iosize=$iosize
    flowop closefile name=close-created,fd=1
    flowop openfile name=open-append,filesetname=writefiles,fd=1
    flowop appendfilerand name=append-file,iosize=$appendsize,fd=1
    flowop closefile name=close-appended,fd=1
    flowop deletefile name=delete-file,filesetname=writefiles
  }
}

echo "filebench write-heavy loaded"

run $runtime
EOF

# fileserver：标准 filebench workload（create/write/append/read/delete/stat）。
elif [ "$WORKLOAD" = "fileserver" ]; then
render "$N_FILES" "$N_FSIZE" <<'EOF'
set $dir=@DIR@
set $nfiles=@NFILES@
set $meandirwidth=20
set $meanfilesize=@FILESIZE@
set $nthreads=@NTHREADS@
set $iosize=1m
set $meanappendsize=16k
set $runtime=@RUNTIME@

define fileset name=bigfileset,path=$dir,size=$meanfilesize,entries=$nfiles,dirwidth=$meandirwidth,prealloc=80

define process name=filereader,instances=1
{
  thread name=filereaderthread,memsize=10m,instances=$nthreads
  {
    flowop createfile name=createfile1,filesetname=bigfileset,fd=1
    flowop writewholefile name=wrtfile1,srcfd=1,fd=1,iosize=$iosize
    flowop closefile name=closefile1,fd=1
    flowop openfile name=openfile1,filesetname=bigfileset,fd=1
    flowop appendfilerand name=appendfilerand1,iosize=$meanappendsize,fd=1
    flowop closefile name=closefile2,fd=1
    flowop openfile name=openfile2,filesetname=bigfileset,fd=1
    flowop readwholefile name=readfile1,fd=1,iosize=$iosize
    flowop closefile name=closefile3,fd=1
    flowop deletefile name=deletefile1,filesetname=bigfileset
    flowop statfile name=statfile1,filesetname=bigfileset
  }
}

echo "filebench fileserver loaded"

run $runtime
EOF

# varmail：标准邮件服务器负载（create + write + fsync + read + delete + append）
#   —— 元数据/持久化最重的一档，rNVPC 与 native 的差异最容易在这里放大。
elif [ "$WORKLOAD" = "varmail" ]; then
render "$N_FILES" "$N_FSIZE" <<'EOF'
set $dir=@DIR@
set $nfiles=@NFILES@
set $meandirwidth=1000000
set $filesize=@FILESIZE@
set $nthreads=@NTHREADS@
set $meaniosize=16k
set $runtime=@RUNTIME@

define fileset name=bigfileset,path=$dir,size=$filesize,entries=$nfiles,dirwidth=$meandirwidth,prealloc=100

define process name=rmail,instances=1
{
  thread name=rmailthread,memsize=10m,instances=$nthreads
  {
    flowop createfile name=createfile1,filesetname=bigfileset,fd=1
    flowop writewholefile name=writefile1,srcfd=1,fd=1,iosize=$meaniosize
    flowop fsync name=fsync1,fd=1
    flowop closefile name=closefile1,fd=1
    flowop openfile name=openfile1,filesetname=bigfileset,fd=1
    flowop readwholefile name=readfile1,fd=1,iosize=$meaniosize
    flowop closefile name=closefile2,fd=1
    flowop deletefile name=deletefile1,filesetname=bigfileset
    flowop appendfilerand name=appendfile1,fd=1,iosize=$meaniosize
  }
}

echo "filebench varmail loaded"

run $runtime
EOF

# seqwrite：filemicro 风格的单文件顺序写（纯数据面，元数据干扰最小）。
elif [ "$WORKLOAD" = "seqwrite" ]; then
render "$N_FILES" "$N_FSIZE" <<'EOF'
set $dir=@DIR@
set $filesize=@FILESIZE@
set $nthreads=@NTHREADS@
set $iosize=1m
set $runtime=@RUNTIME@

define file name=bigfile,path=$dir,size=$filesize,prealloc

define process name=seqwriter,instances=1
{
  thread name=seqwriterthread,memsize=10m,instances=$nthreads
  {
    flowop openfile name=openfile1,filesetname=bigfile,fd=1
    flowop write name=write1,fd=1,iosize=$iosize
    flowop closefile name=closefile1,fd=1
  }
}

echo "filebench seqwrite loaded"

run $runtime
EOF
fi

# ---- 跑 -----------------------------------------------------------------
T0="$(now_s)"
"$FILEBENCH" -f "$WL" > "$FB_OUT" 2>&1
FB_RC=$?
T1="$(now_s)"
ELAPSED=$(awk -v a="$T0" -v b="$T1" 'BEGIN{printf "%.3f", b-a}')

# filebench 的汇总行各版本格式略有差异，全部按"抓关键字"解析，抓不到就如实记 raw。
# 1.5-alpha3: `163107 ops 16301.271 ops/s 0/4658 rd/wr 163.6mb/s 0.242ms/op`
# 1.4.9.1  : `0 ops, 0.000 ops/s, (0/0 r/w), 0.0mb/s, 0us cpu/op, 0.0ms latency`
# 注意 mb/s、ms/op 前**可能没有空格**，所以模式里空格可选、单位为 ms/(op|latency)。
SUMMARY="$(grep -E 'IO Summary' "$FB_OUT" | tail -1)"
pick() { printf '%s' "$1" | grep -oE "$2" | head -1 | grep -oE '[0-9][0-9.]*'; }
OPS=$(pick "$SUMMARY" '[0-9][0-9.]* *ops/s')
MBS=$(pick "$SUMMARY" '[0-9][0-9.]* *mb/s')
# 1.5-alpha3 写 `0.242ms/op`，1.4.9.1 写 `0.0ms latency`（无斜杠）→ 只认 "ms"。
LAT=$(pick "$SUMMARY" '[0-9][0-9.]* *ms')

if [ -z "$SUMMARY" ]; then
    STATUS=crash
elif [ "$FB_RC" -ne 0 ]; then
    STATUS=partial
else
    STATUS=ok
fi

# 末 20 行原始输出随日志带回宿主：解析失败的现场在宿主侧还能人工看。
echo "--- FILEBENCH_RAW_BEGIN (rc=$FB_RC) ---"
tail -20 "$FB_OUT" 2>/dev/null || true
echo "--- FILEBENCH_RAW_END ---"

echo "=== FILEBENCH_RESULT: ${DEV_TAG}_${FS}_${WORKLOAD}_t${THREADS} status=${STATUS} rc=${FB_RC} elapsed_s=${ELAPSED} ops_per_sec=${OPS:-0} mb_per_sec=${MBS:-0} latency_ms=${LAT:-0} ==="

# ---- 收尾 ---------------------------------------------------------------
umount "$MNT" 2>/dev/null || true
exit 0
