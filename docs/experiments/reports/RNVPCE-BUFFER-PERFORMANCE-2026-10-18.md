# rNVPCe 与 EXT4 buffered write 对照（2026-10-18）

> 后续修复及完整2GiB复测：[rNVPCe容量回压、迁出与卸载修复](RNVPCE-BUFFER-FIX-2026-10-18.md)。本文保留修复前结果。

当前 rNVPCe 未达到接近 EXT4 buffered write 的预期。更重要的是，2GiB 压力测试均在约64MiB后返回 EAGAIN，不能得到有效的持续带宽；新文件顺序写还存在卸载 EINVAL。此次仅增加测试脚本和记录，没有修改内核。

## 配置与口径

- 内核提交：`93c2b45dd4ac49b14daab7e5c4ecaab7af61d286`。ISO SHA256：`0f078e322a4be78649a98fd34bcb0589445dfdc4bc04ef53e9bb53877d6de409`。
- 真盘 `/dev/nvme1n1p1`，每点重新格式化 journal EXT4；Docker 内运行 QEMU/KVM，64 vCPU、8GiB、virtio-blk 单队列1024、indirect关闭、host cache=none/aio=threads。
- fio psync、direct=0、fsync=0、iodepth=1，1/16线程，每线程独立文件，总量2GiB。另用32MiB诊断容量未满时的路径。每点一次，runtime=60为上限而非time_based。
- fio CPU0–31 split，历史隔离配置启用（flush60、virtio62、journal63）；这不代表对 rNVPCe 每个新后台线程均完成独立绑核验证。
- EXT4：`delalloc,rnvpc=off,rnvpc_extend=off,rtio=off`，脏页高64MiB/低32MiB。
- rNVPCe：`delalloc,rnvpc=off,rnvpc_extend=on,rtio=on`；总池128MiB、元数据保留64MiB、数据硬上限64MiB，`rnvpc_sim_mode=fast`。这是DRAM模拟持久内存，启用模拟persist，不是实际NVDIMM性能。
- 两种模式启动参数相同（全局模式关闭、由挂载选项选择），均预留模拟池。
- 4KiB随机写先用native EXT4 direct写完整初始化文件，再卸载重挂，测覆盖写。128KiB顺序写测新文件分配写。两者语义不同，不直接互比。
- fio带宽仅覆盖fio工作阶段；完成后另计sync。失败/不完整写不计算有效排空带宽。32MiB仅需数毫秒至数百毫秒，启动/同步固定开销很大，不能解释成稳态性能或统计结论。

## 主测试：总量2GiB

单位MiB/s。EXT4全部完成、正常卸载且后置e2fsck无错误。

| 工作负载 | 线程 | EXT4 fio | EXT4 含最终sync | rNVPCe |
| --- | ---: | ---: | ---: | --- |
| 4KiB随机写 | 1 | 215.5 | 207.1 | EAGAIN，完成64.00MiB后终止 |
| 4KiB随机写 | 16 | 360.4 | 323.5 | EAGAIN，完成64.25MiB后终止 |
| 128KiB顺序写 | 1 | 577.2 | 515.9 | EAGAIN，完成64.00MiB后终止，卸载EINVAL |
| 128KiB顺序写 | 16 | 1304.5 | 1050.3 | EAGAIN，完成64.00MiB后终止，卸载EINVAL |

rNVPCe终止前fio打印的带宽仍保留在CSV，但不是完整2GiB测试结果，不应作为成功结果与EXT4比较。随机写失败后可以卸载，不能据此把失败写解释为通过。

## 诊断：总量32MiB

| 工作负载 | 线程 | EXT4 fio MiB/s | rNVPCe fio MiB/s | rNVPCe / EXT4 | rNVPCe生命周期状态 |
| --- | ---: | ---: | ---: | ---: | --- |
| 4KiB随机写 | 1 | 1280.0 | 164.1 | 12.8% | 写满，卸载正常 |
| 4KiB随机写 | 16 | 680.9 | 250.0 | 36.7% | 写满，卸载正常 |
| 128KiB顺序写 | 1 | 1000.0 | 271.2 | 27.1% | fio写满，但卸载EINVAL |
| 128KiB顺序写 | 16 | 10666.7 | 323.2 | 3.0% | fio写满，但卸载EINVAL |

顺序写两行只能说明前台write阶段计时，不能作为完整成功性能结果。EXT4的16线程32MiB顺序写fio运行仅3ms，尤其不适合用于精确比例判断。32MiB随机写含最终sync：EXT4两档均71.1MiB/s，rNVPCe分别42.7/40.0MiB/s；该指标受短测试和同步固定开销影响，不代表迁出稳态速度。

## 已确认的代码事实与未证实的推断

1. **容量回压缺少等待重试，已确认。** `kernel/src/fs/rnvpc/capacity.rs:70` 到数据硬上限返回EAGAIN；`frame.rs:982`明确要求调用方锁外重试。`kernel/src/fs/fs_impls/ext4/inode/mod.rs:1705` 的 `write_rnvpce_at` 持inode写锁调用commit，并在错误分支直接返回；`write_at:1861`也直接传播。它与四个2GiB点的约64MiB终止相吻合。
2. **后台迁出仍存在固定节拍，已确认；实际占比尚未测量。** `kernel/src/fs/fs_impls/ext4/migration.rs:49` 每轮预算64页，`:58` 每轮等待10ms。忽略其他开销，其配置节拍只相当于25MiB/s（64×4KiB/10ms），不是实测磁盘吞吐。新文件还在`:116`附近逐页分配块并在`:122`持久化分配。需要后续延迟分解再确定主要瓶颈，不能用本轮数据给各项分摊百分比。
3. **rNVPCe前台有持久元数据成本，已确认路径；带宽影响未量化。** `write_rnvpce_at`并非只向PageCache memcpy；它执行持久extent更新和模拟persist。因此不能直接假设它必然等于native EXT4仅缓存数据的write成本。
4. **新文件顺序写后的卸载EINVAL已复现，根因尚未定位。** 32MiB两个点fio error=0、写满，但umount_status=1。`fs.rs:2296` 的sync调用 `export_nv_for_sync()`，后者包含迁出与NV extent导出，是后续排查入口；当前证据不足以断言具体哪个函数返回EINVAL。

后置只读e2fsck能检查SSD上EXT4结构，不能证明NV权威元数据已完整导出或文件内容已持久化。本轮未做逐文件读回校验、故障注入或恢复验证。早期两个rNVPCe顺序写2GiB点在卸载失败后脚本提前退出，没有后置e2fsck；其余14点均有e2fsck且exit=0。随后脚本已改为即使guest失败也采集后置检查，并明确记录umount状态。

建议下一步先补齐锁外容量等待/唤醒、定位新文件sync/卸载错误，再用相同2GiB配置重测；当前不宜依据失败测试调整性能结论。

## 复现与原始证据

脚本见 `../os_com_implementation/test/rnvpce-buffer-compare/`。该目录README列出准备和执行命令。

[结果CSV](/home/zn/asterinas-build-env/rnvpce-buffer-20261018/results.csv)；[主机运行状态](/home/zn/asterinas-build-env/rnvpce-buffer-20261018/suite-status.json)。同目录保留16份guest日志、每点ISO/提交/设备配置manifest，以及已有后置e2fsck日志。host_exit=0仅代表旧版脚本拿到guest结束标记且e2fsck通过，**不代表fio或卸载通过**，以CSV outcome和原始BUFFER_COMPLETE为准。

完整运行目录（含ISO和辅助磁盘）位于 `/home/zn/asterinas-build-env/rnvpce-buffer-20261018/`。
