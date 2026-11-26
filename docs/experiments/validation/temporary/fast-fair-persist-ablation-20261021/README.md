# FAST-FAIR persist/fence 空操作消融与阶段诊断（2026-10-21）

本实验使用既有 `fast-no-persist` 模式，把 FAST-FAIR rNVPCe 的 NVDIMM `persist`、`persist_control` 和 `fence` 暂时变为空操作，并与正常 fast 模式进行同源码、同探针对照。结果不支持“模拟持久化屏障本身是当前主要瓶颈”：三个稳定测试点的 fio 带宽变化均在 +/-1.3% 内。高频逐页事务仍是重要结构性成本，因为空操作只跳过编译器屏障和计数器更新，没有消除每字段存储、节点读取、路由、校验和发布流程。

## 实验方法

- 分支/提交：`recovery/rnvpce-fast-fair`，`862fa8eb7`。
- 设备：真实 NVMe `/dev/nvme1n1p1`，沿用用户清空授权；每点重新格式化，结束后卸载并运行 e2fsck。
- guest：64 vCPU、8 GiB；virtio queue 1024；沿用 fio、virtio 和 rTIO CPU 隔离。
- rNVPCe：nojournal，NVPool 128 MiB，metadata reserve 64 MiB，data limit 64 MiB，orderless migration。
- workload：每点 2 GiB，buffered、psync、iodepth=1；4 KiB randwrite 与 128 KiB sequential write，各测 1/16 线程。
- 正常组使用 `rnvpc_sim_mode=fast`；消融组使用既有 `rnvpc_sim_mode=fast-no-persist`。该模式仍验证地址范围并记录调用次数，随后直接返回；SSD flush 不受影响。
- 两组使用同一份开启 `LATENCY`、`PIPELINE` 和 `NVPAGE-BATCH` 的诊断源码分别构建。诊断开关在实验后已恢复为默认关闭。

## 带宽结果

单位 MiB/s。`含排空` 从 fio 开始计时到文件系统后台数据排空完成；每点均 fio error=0、卸载成功、e2fsck clean。

| workload | 线程 | 正常 fio | 空操作 fio | 变化 | 正常含排空 | 空操作含排空 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 4 KiB randwrite | 1 | 11.14 | 11.00 | -1.3% | 11.02 | 10.93 |
| 128 KiB write | 1 | 37.86 | 37.97 | +0.3% | 34.51 | 34.50 |
| 128 KiB write | 16 | 256.67 | 260.00 | +1.3% | 157.42 | 162.67 |

4 KiB/16线程存在明显的后台调度方差，因而单独列出两轮原始结果：

| 轮次 | 正常 fio | 空操作 fio | 正常含排空 | 空操作含排空 |
| --- | ---: | ---: | ---: | ---: |
| 1 | 96.60 | 59.87 | 91.35 | 56.83 |
| 2 | 88.26 | 93.70 | 83.05 | 88.70 |

首轮空操作慢 38%，第二轮却快 6%。两次运行形成了不同的迁移批次：首轮正常/空操作分别产生 8,140/3,863 个 flush 批次，第二轮为 14,784/10,924 个。累计容量等待、completion 队列驻留和 SSD 长尾也随之大幅变化。因此该点只能证明运行方差较大，不能归因于 NVDIMM fence。

## 4 KiB 单线程阶段分解

该点没有线程间并发放大，且正常/空操作吞吐稳定重合，适合定位每页软件成本。时间为所有调用累计值除以调用数；相邻阶段可能嵌套，不能直接相加。

| 阶段 | 正常 | 空操作 | 解释 |
| --- | ---: | ---: | --- |
| 前台总计 | 348.35 us/page | 352.76 us/page | 无改善 |
| 前台 body | 226.00 us/page | 229.18 us/page | 无改善 |
| 前台 extent | 159.12 us/page | 161.31 us/page | body 的约 70% |
| 前台 lookup | 57.68 us/page | 58.15 us/page | body 的约 25% |
| 前台 data | 5.22 us/page | 5.63 us/page | 次要 |
| completion leaf total | 74.62 us/call | 74.33 us/call | 无改善 |
| completion leaf update | 51.86 us/call | 51.40 us/call | leaf total 的约 69% |
| completion leaf read | 10.61 us/call | 10.70 us/call | 单次完整节点读取 |
| completion entry scan | 10.03 us/call | 10.14 us/call | 单次 entry 处理 |

锁等待不是这个单线程点的主要成本：前台 write-inner 锁累计约 36 ms，即约 0.07 us/page；publication wait 约 0.19 us/page；migration leaf lock wait 正常/空操作约 0.43/0.32 us/call。

后台累计阶段也基本不随 NVDIMM 空操作变化：正常/空操作的 extent publish 为 17.69/17.72 s，completion total 为 20.36/19.51 s。SSD device flush 为 2.26/1.42 s，属于未被本实验关闭的真实设备路径及运行波动。

## 调用放大

4 KiB 单线程对 524,288 个逻辑页，正常模式记录 7,230,163 次 data persist、1,640,071 次 control persist、7,802,994 次 fence，分别约为 13.79、3.13、14.88 次/page。空操作组调用次数几乎相同，因为计数发生在提前返回之前。

128 KiB 单线程虽然只有 16,384 次 fio 写请求，仍处理 524,288 个 4 KiB 页，并记录 1,278,560 次 data persist、3,855,577 次 control persist、929,070 次 fence。换算为每个 128 KiB 请求约 78 次 data persist、235 次 control persist 和 57 次 fence。这说明大块写请求在持久化元数据路径上仍被拆成大量逐页/逐字段事务。

## 结论与下一步

1. 当前 fast 模拟里的 persist/fence 本体只是编译器屏障和原子计数；将其变为空操作没有带宽收益，不能解释 FAST-FAIR 相对 repair-image 版本的严重回退。
2. FAST-FAIR 的问题不是“屏障单次太慢”，而是取消批处理后恢复了逐页、逐字段的完整事务流程。即使屏障免费，字段更新之前仍读取并解码完整 4 KiB extent node，随后执行身份校验、exclusive update 状态切换、pin/unpin 和通知。
3. 最突出的前台成本是 extent 路径与 lookup；completion 的主要 CPU 成本是 leaf update。多线程下还会由 64 MiB data limit 触发显著容量等待，并在单 completion 消费者前形成很长的队列驻留。
4. 后续优化应保留 FAST/FAIR 的崩溃顺序语义，同时恢复“一个逻辑写区间/一个 migration batch 只读取和锁定节点一次”的批处理：在内存中准备多项 entry mutation，按依赖阶段成组 store/persist，阶段间 fence，一次验证并发布叶节点。仅合并 fence 而保留每字段完整节点读取，预计收益有限。

## 证据

- 完整镜像、guest 日志和 fio/e2fsck 结果：`/home/zn/asterinas-build-env/fast-fair-persist-ablation-20261021/`。
- 正常组：`run-2-fast/`；空操作组：`run-2-no-persist/`；4 KiB/16线程重复点：`repeat-fast/`、`repeat-no-persist/`。
- 两个 ISO SHA-256：正常 `6783d0b1ac332b9f818bf5b2820979250539d8fd92f9fb44d8ebdec31f25c6c1`；空操作 `2ec700b53394e1eafae24d6593d460696c34904372c14aaa33f86b044d1b2ab3`。
- `fast-no-persist` 只跳过 NVDIMM 模拟持久化本体；调用计数和参数校验仍执行。源码位置见 `kernel/src/fs/rnvpc/dram.rs`。
