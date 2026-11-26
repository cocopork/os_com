# EXT4基线测试CPU隔离（2026-10-18）

恢复依据：`research_recovery_20261016/full-versions/kernel/src/thread/io_test_affinity.rs/ef971736d924c2e04dfe069196fefa90ffff4b2b.rs`，与同批block.rs、posix_thread/builder.rs、kernel_thread.rs、journal/mod.rs的调用点交叉印证。原件和SHA256保存在快速测试目录historical-io_test_affinity.rs及isolation-provenance.json。

历史64 vCPU布局是User 0–31、Kernel 32–59、Flush 60、Completion 61、Virtio 62、Journal 63。当前仍使用16 vCPU以加快迭代，按原来“用户占一半、尾部预留四个服务CPU”的方式缩放：

| 角色 | 来宾CPU | 当前接入 |
|---|---|---|
| 用户线程（含fio） | 0–7 | POSIX线程创建时设置；fio预填充与测量显式cpus_allowed=0-7、policy=shared |
| 普通内核线程 | 8–11 | ThreadOptions默认值；明确指定亲和性的idle/启动等线程保留其原有设置 |
| EXT4 native writeback | 12 | 新增对应历史Flush角色接入 |
| Completion预留 | 13 | 当前基线未接入独立完成线程，未声称IRQ也隔离到这里 |
| virtio-blk提交线程 | 14 | 所有virtio块设备提交线程均使用此CPU |
| EXT4 journal提交线程 | 15 | 对应历史Journal接入 |

沿用启动参数rtio_cpu_isolation=on，独立于rTIO功能开关，rTIO=off的EXT4基线也生效。不启用时所有默认亲和性仍为全CPU集合。启用至少需要10个CPU，避免产生空的普通内核线程集合；64 CPU时保留原布局。

rtio_cpu_trace=on在用户、virtio、writeback、journal线程入口断言初始落核并输出一次日志（每个线程一次）。这验证初始放置，CPU亲和性提供后续调度约束；用户仍可调用sched_setaffinity改变自己的集合，本实验fio设置保持0–7。本改动不修改系统调用语义。

仅隔离来宾vCPU。宿主QEMU vCPU线程、aio=threads后端和IRQ没有新增物理CPU绑定；因此不能声称整机物理CPU或中断完全隔离。fio16任务共享8个用户vCPU，比较性能需使用相同隔离配置，不能直接与原来全16CPU的结果归因比较。

快速测试目录：`/home/zn/asterinas-build-env/ext4-quick-2g-cache64m-20261018/`。build-release.sh已启用隔离和入口trace；测试总量2 GiB，native缓存/脏页上限64 MiB、低水位32 MiB。旧测试目录保持原状。

## 验证

- RED：未实施分区时，角色重叠测试实际失败：CPU roles must not overlap（0 passed、1 failed、301 filtered）。
- GREEN：完整kernel ktest **302 passed、0 failed、0 filtered out**，包含分区不重叠及64CPU历史布局、16CPU缩放布局检查；未启用隔离时的SMP1常规启动与原有测试均通过。
- release构建成功；cargo fmt --all --check、git diff --check通过。
- 开启隔离和入口trace，在授权分区/dev/nvme1n1p1重新格式化journal EXT4，16任务buffered随机覆盖写，总量精确2 GiB，预填充也为2 GiB；fio错误0，最终sync与卸载完成，QEMU退出33，e2fsck -fn退出0。
- 初始落核日志：3个Virtio线程均CPU14、Flush线程CPU12、Journal线程CPU15，所有观测到的User入口均CPU0–7。fio JSON确认cpus_allowed=0-7、policy=shared。
- 本轮仅作隔离与配置的冒烟验证：含最终sync为477.4 MiB/s。不同数据量/缓存预算/用户CPU数量，且单次短样本，不能与旧16 GiB结果直接比较或据此量化隔离收益。

证据目录同上：red.log、full.log、build.log、isolation-rand4k-j16-2g-r1.log及e2fsck日志、isolation-results.csv、isolation-verification.json、source-manifest.json。入口日志可能插入fio JSON串行输出，当前目录的解析器会仅剔除完整CPU-ISOLATION标记后再解析，原始日志保留。

镜像SHA256：`c60c9b9d1a118d9deae2cd0f0aeae51a2aec4486939b51601519d36287cf73c1`。
