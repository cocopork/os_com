#!/bin/sh
# SPDX-License-Identifier: MPL-2.0
set -eu

. /ext2/filebench.cfg

RUNTIME=${RUNTIME:-60}
NFILES=${NFILES:-10000}

FILEBENCH=/ext2/filebench
MNT=/filebench-ext4
WL=/tmp/filebench.f

fail() { echo "FILEBENCH_ERROR case=$CASE message=$1"; exit 2; }
now_s() { awk '{printf "%.3f", $1}' /proc/uptime; }

case "$MODE" in
    ext4)
        OPTIONS="delalloc,rnvpc=off,rnvpc_extend=off,rtio=off,pagecache_capacity_mb=128"
        ;;
    rnvpce)
        OPTIONS="delalloc,rnvpc=off,rnvpc_extend=on,rtio=on,pagecache_capacity_mb=64"
        ;;
    *) fail invalid-mode ;;
esac

chmod 0755 "$FILEBENCH"
mkdir -p "$MNT"
mount -t ext4 -o "$OPTIONS" /dev/vdc "$MNT" || fail mount
mkdir -p "$MNT/data"

cat >"$WL" <<EOF
set \$dir=$MNT/data
set \$nfiles=$NFILES
set \$filesize=128k
set \$nthreads=16
set \$runtime=$RUNTIME
set \$readiosize=1m
set \$writeiosize=16k

define fileset name=benchfiles,path=\$dir,size=\$filesize,entries=\$nfiles,dirwidth=100,prealloc=100
EOF

case "$WORKLOAD" in
    fileserver)
        cat >>"$WL" <<'EOF'
define process name=fileserver,instances=1
{
  thread name=fileserverthread,memsize=10m,instances=$nthreads
  {
    flowop openfile name=open-file,filesetname=benchfiles,fd=1
    flowop readwholefile name=read-file,fd=1,iosize=$readiosize
    flowop appendfilerand name=write-file-1,fd=1,iosize=$writeiosize
    flowop appendfilerand name=write-file-2,fd=1,iosize=$writeiosize
    flowop closefile name=close-file,fd=1
    flowop statfile name=stat-file,filesetname=benchfiles
  }
}
EOF
        ;;
    varmail)
        cat >>"$WL" <<'EOF'
define process name=varmail,instances=1
{
  thread name=varmailthread,memsize=10m,instances=$nthreads
  {
    flowop openfile name=open-mail,filesetname=benchfiles,fd=1
    flowop readwholefile name=read-mail,fd=1,iosize=$writeiosize
    flowop writewholefile name=rewrite-mail,srcfd=1,fd=1,iosize=$writeiosize
    flowop fsync name=sync-mail,fd=1
    flowop closefile name=close-mail,fd=1
    flowop statfile name=stat-mail,filesetname=benchfiles
  }
}
EOF
        ;;
    webserver-10r1w)
        cat >>"$WL" <<'EOF'
define process name=webserver,instances=1
{
  thread name=webserverthread,memsize=10m,instances=$nthreads
  {
    flowop openfile name=open-file,filesetname=benchfiles,fd=1
    flowop readwholefile name=read-01,fd=1,iosize=$readiosize
    flowop readwholefile name=read-02,fd=1,iosize=$readiosize
    flowop readwholefile name=read-03,fd=1,iosize=$readiosize
    flowop readwholefile name=read-04,fd=1,iosize=$readiosize
    flowop readwholefile name=read-05,fd=1,iosize=$readiosize
    flowop readwholefile name=read-06,fd=1,iosize=$readiosize
    flowop readwholefile name=read-07,fd=1,iosize=$readiosize
    flowop readwholefile name=read-08,fd=1,iosize=$readiosize
    flowop readwholefile name=read-09,fd=1,iosize=$readiosize
    flowop readwholefile name=read-10,fd=1,iosize=$readiosize
    flowop appendfilerand name=write-01,fd=1,iosize=$writeiosize
    flowop closefile name=close-file,fd=1
  }
}
EOF
        ;;
    webserver-1r1w)
        cat >>"$WL" <<'EOF'
define process name=webserver,instances=1
{
  thread name=webserverthread,memsize=10m,instances=$nthreads
  {
    flowop openfile name=open-file,filesetname=benchfiles,fd=1
    flowop readwholefile name=read-01,fd=1,iosize=$readiosize
    flowop appendfilerand name=write-01,fd=1,iosize=$writeiosize
    flowop closefile name=close-file,fd=1
  }
}
EOF
        ;;
    *) fail invalid-workload ;;
esac

cat >>"$WL" <<'EOF'
echo "filebench workload loaded"
run $runtime
EOF

echo "FILEBENCH_CONFIG case=$CASE device=$DEVICE mode=$MODE workload=$WORKLOAD threads=16 nfiles=$NFILES filesize=128k runtime_s=$RUNTIME total_data_cache_mib=128 mount_options=$OPTIONS"
cat /proc/cmdline
T0=$(now_s)
status=0
"$FILEBENCH" -f "$WL" || status=$?
T1=$(now_s)
echo "FILEBENCH_COMPLETE case=$CASE status=$status start_s=$T0 end_s=$T1"
sync_status=0
sync || sync_status=$?
umount_status=0
umount "$MNT" || umount_status=$?
echo "FILEBENCH_CLEANUP case=$CASE sync_status=$sync_status umount_status=$umount_status"
[ "$status" -eq 0 ] && [ "$sync_status" -eq 0 ] && [ "$umount_status" -eq 0 ]
