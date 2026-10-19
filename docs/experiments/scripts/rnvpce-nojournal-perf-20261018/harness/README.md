# rNVPCe / EXT4 buffered write 对照

`run.py`会格式化所选分区，只能在已经获得该分区数据清空授权后使用。设备映射在脚本内固定，检查大小、挂载和swap状态；同名日志不可覆盖。所有测试串行执行，不能共享辅助ext2镜像并发运行。

2026-10-18结果见 `../../../reports/RNVPCE-BUFFER-PERFORMANCE-2026-10-18.md`。原始镜像和辅助盘保存在 `/home/zn/asterinas-build-env/rnvpce-buffer-20261018/`。

在已绑定本仓库的 `os-com-rnvpc-rebuild` 容器内准备 release ISO：

```sh
sg docker -c 'docker exec -e CARGO_NET_OFFLINE=true -e GIT_CONFIG_COUNT=1 -e GIT_CONFIG_KEY_0=safe.directory -e GIT_CONFIG_VALUE_0=/root/asterinas -e CARGO_BUILD_JOBS=16 -e SMP=64 -e MEM=8G -e CONSOLE=ttyS0 -w /root/asterinas/kernel os-com-rnvpc-rebuild cargo osdk build --release --init-args="/ext2/rebuild-fio.sh" --kcmd-args="console=ttyS0 ostd.log_level=error rnvpc=off rnvpc_extend=off rtio=off rnvpc_size=128M rnvpc_meta_reserve=64M rnvpc_data_limit=64M rnvpc_sim_mode=fast rtio_cpu_isolation=on rtio_cpu_trace=on"'
```

新建独立artifacts目录，复制本次ISO为`selected.iso`，准备包含fio和shell运行环境的`ext2.img`及辅助`exfat.img`（沿用已验证direct-sweep镜像），安装host debugfs，确保Docker镜像`os-com-recovery-tools:20261017`可用。测试期间脚本会更新辅助ext2中的guest脚本和配置。禁止使用`ostd.log_level=info`，其逐系统调用日志会严重干扰测量。

在仓库根目录执行单点，`/path/to/new-artifacts`替换为准备好的目录：

```sh
python3 docs/os_com_implementation/test/rnvpce-buffer-compare/run.py nvme \
  --variant rnvpce --rw randwrite --jobs 1 --total-mib 2048 \
  --format-authorized --artifacts /path/to/new-artifacts
python3 docs/os_com_implementation/test/rnvpce-buffer-compare/summarize.py /path/to/new-artifacts
```

遍历variant=ext4/rnvpce、rw=randwrite/write、jobs=1/16、total-mib=2048/32，共16点。失败后仍可继续下一点，但必须重新格式化，不能使用`--skip-format`复用失败现场作为全新点。

结果解释：`fio_mib_s`保留失败前计时，只有`outcome=complete`才算完整通过；`drained_mib_s`仅为完整成功点计算，包含启动fio至sync返回的时间、不包含umount耗时。32MiB短测不代表稳态。guest结束、QEMU正常退出和e2fsck结构通过均不等价于fio成功或数据读回通过。检查guest日志中的fio error/io_bytes和BUFFER_COMPLETE umount_status。

2026-10-18修复后的复测使用`--runtime 180`，只提高单点超时上限，目标数据量仍为2GiB。`run.py`目前会在fio出错、数据量不足或卸载失败时返回非零，并在manifest记录内核diff哈希；历史旧日志的host_exit解释见原报告。最终复测结果见 `../../../reports/RNVPCE-BUFFER-FIX-2026-10-18.md`。

2026-10-18 nojournal 接入后：`--journal on|off`控制格式化时磁盘是否保留 journal（默认on）；rNVPCe 无论磁盘是否带干净 journal 都不启动 JBD2，原生EXT4继续按磁盘特性启用。manifest分别记录requested_disk_journal与expected_runtime_journal；它们是请求配置与预期策略，不是从guest测得的状态。带待恢复日志的磁盘必须先通过原生EXT4恢复。此前rNVPCe性能报告是在仍启动JBD2的旧内核上取得，不代表新策略结果。相同磁盘journal配置下比较，勿把切换磁盘格式与代码优化混为一项收益。
