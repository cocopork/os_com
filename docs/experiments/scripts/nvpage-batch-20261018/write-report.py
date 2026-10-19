import json,shutil,hashlib,csv,statistics
from pathlib import Path
base=Path('/home/zn/asterinas-build-env/nvpage-batch-20261018')
root=Path('/home/zn/zn/Rust_EXT4/os_com_rnvpc_rebuild')
summ=json.loads((base/'throughput/summary.json').read_text());assert len(summ)==8 and all(s['repeats']==3 for s in summ)
diag=json.loads((base/'profile/diagnostics.json').read_text());assert len(diag)==8
raw=base;raw.mkdir(parents=True,exist_ok=True)
for name in ['run-ab.py','aggregate-ab.py','write-report.py','switch-profile.py','freeze-source.py','throughput.log','profile.log','descriptor-red.log','batch-allocation-red2.log','reclaim-red.log','batch-green2.log','batch-green3.log','crash-debug.log','batch-green4.log','release-build.log','profile-before-build.log','profile-after-build.log']:
    if (base/name).exists():shutil.copy2(base/name,raw/name)
for mode in ['throughput','profile']:
    for f in (base/mode).rglob('*'):
        if f.is_file() and f.suffix in ['.csv','.json','.txt','.log']:
            dest=raw/mode/f.relative_to(base/mode);dest.parent.mkdir(parents=True,exist_ok=True);shutil.copy2(f,dest)
for family in ['release-images','profile-images']:
    for version in ['before','after']:
        dest=raw/family/version;dest.mkdir(parents=True,exist_ok=True)
        shutil.copy2(base/family/version/'kernel-source.diff',dest/'kernel-source.diff')
        hashes={name:hashlib.sha256((base/family/version/name).read_bytes()).hexdigest() for name in ['kernel-source.diff','selected.iso']}
        (dest/'hashes.json').write_text(json.dumps(hashes,indent=2)+'\n')
(raw/'harness').mkdir(exist_ok=True)
for name in ['run.py','guest.sh','summarize.py','README.md']:shutil.copy2(root/'docs/os_com_implementation/test/rnvpce-buffer-compare'/name,raw/'harness'/name)
lookup={(s['rw'],s['jobs'],s['version']):s for s in summ};dl={(d['rw'],d['jobs'],d['version']):d for d in diag}
labels={'randwrite':'4KiB随机写','write':'128KiB顺序写'}
lines=['# 010 NVPage批量事务实现与验证（2026-10-18）','','010已接入双副本描述符、区间目标分配、批量额度阶段和批量回收。307项内核回归通过；真实盘前后各三轮、共24个release性能点及8个独立诊断点均通过。本次未提交或推送。','','## 实现与依赖顺序','','- 控制池格式版本4，320字节/项：选择字独占缓存线，两个描述符副本各占128字节。旧格式池拒绝；这不是旧池在线迁移工具。','- 所有非活动描述符副本persist→批次fence→各选择字persist→批次fence；目标与额度更新不再重刷未变化的4KiB数据。部分选择字发布失败阻止继续修改，恢复后再启用，避免重试覆盖唯一持久有效副本。','- 候选仍来自脏NVPage队列，按inode/逻辑连续性申请SSD区间，消费实际返回长度；前置分配失败回滚私有目标，可能已发布引用时保守保留。','- 既有extent按叶发布保持；成功集合批量提交额度，汇总更新每inode账本和运行时容量。批量清描述符并fence后才归还页，allocation锁最多处理32页一个窗口，批末通知。','- 保持SSD分配持久化→目标发布→数据BIO→SSD flush→NVM SSD extent发布→额度阶段→NV失效持久化→页复用；MAX_IN_FLIGHT仍为128。','','## 正确性与故障测试','','- RED：旧描述符更新在AfterContentPersist崩溃后读到None；双副本实现恢复旧选择版本。','- RED：连续32页触发第二次allocator调用ENOSPC；新区间分配只用一次。','- RED：4页回收使用4次fence；批量回收只用1次。','- 批量描述符测试：3页更新恰好6次control persist、2次fence；故意修改但未persist的数据在崩溃后仍是旧值，证明字段更新未重刷数据。覆盖内容准备、选择字store/persist/fence、部分发布和重复恢复。','- 覆盖重复槽位、旧allocation_seq、旧池版本拒绝、失效前容量保留及既有单页错误路径。','- NVM图恢复覆盖目标准备中断/选择字部分发布、SSD extent发布但NV仍存活、部分描述符清除和完整回收；每种情况连续恢复两次，检查权威映射、NV字节和额度。该测试模拟SSD成功前提，不声称包含真实SSD掉电。','- 碎片集成场景强制每次只返回1块，第二次申请失败后验证free计数恢复且没有目标发布；重试成功后逐逻辑页检查不同内容对应的原始SSD块。','- 最终完整kernel ktest（SMP=1）：**307 passed / 0 failed**，包含原生EXT4和rNVPC WAL。中间306/1是新测试错误要求恢复后零额度账本必须存在；恢复实际可回收零账本，修正为检查语义额度0，保留中间日志。','- 独立只读审查无明确新增实现缺陷，指出的碎片/部分回滚覆盖缺口已补齐；release构建、格式与diff检查见验证摘要。','','## 性能配置','','真实NVMe `/dev/nvme1n1p1`，沿用用户清空授权，每点重新格式化且无HAS_JOURNAL。两组均为rNVPCe nojournal；旧版ISO为009最终版，新版为010。每组3轮，第二轮反向执行；同一时刻只跑一点，测量期间无编译负载。','','每点总量2GiB、psync/direct=0/fsync=0/iodepth=1、每线程独立文件；随机写先原生direct写初始化再卸载。64vCPU/8GiB，virtio单队列1024；fio0–31、virtio62、rTIO沿用Kernel32–59；NVPool128MiB、metadata reserve64MiB、数据上限64MiB、fast模拟。含sync带宽不包括umount。release关闭诊断计数；下面单位MiB/s，均为3次中位数。','','| 工作负载 | 线程 | 009 fio | 010 fio | 变化 | 009含sync | 010含sync | 变化 |','| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |']
for rw in ['randwrite','write']:
    for jobs in [1,16]:
        b=lookup[rw,jobs,'before'];a=lookup[rw,jobs,'after']
        lines.append(f"| {labels[rw]} | {jobs} | {b['fio_mib_s']:.1f} | {a['fio_mib_s']:.1f} | {(a['fio_mib_s']/b['fio_mib_s']-1)*100:+.1f}% | {b['drained_mib_s']:.1f} | {a['drained_mib_s']:.1f} | {(a['drained_mib_s']/b['drained_mib_s']-1)*100:+.1f}% |")
lines+=['','各点完整写满2GiB，fio error=0、sync返回、umount成功、e2fsck exit=0。三轮范围见summary.json，不据三次观测推导统计显著性。','','## 独立诊断：调用次数','','下面每点一次，使用开启batch_metrics的独立镜像；只用于计数和延迟分解，不作为release带宽。表中数值按009→010列出。persist/fence计数来自整个NVM后端，包含前台、元数据和迁出，不能全部解释为rTIO开销。叶发布次数沿用既有叶级合批，本次未将其宣称为新能力。','','| 工作负载 | 线程 | 分配调用 | 新分配页 | 叶发布调用 | data persist调用 | control persist调用 | fence调用 |','| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |']
for rw in ['randwrite','write']:
    for jobs in [1,16]:
        b=dl[rw,jobs,'before'];a=dl[rw,jobs,'after']
        vals=[(b['phases']['allocate']['calls'],a['phases']['allocate']['calls']),(b['events']['allocated_pages'],a['events']['allocated_pages']),(b['phases']['leaf_publish']['calls'],a['phases']['leaf_publish']['calls'])]+[(b['events'][k],a['events'][k]) for k in ['data_persist','control_persist','fence']]
        lines.append(f"| {labels[rw]} | {jobs} | "+' | '.join(f'{x:,}→{y:,}' for x,y in vals)+' |')
lines+=['','## 独立诊断：阶段时间','','单位秒，为所有调用累计耗时（009→010）。不同后台阶段可能跨批次重叠，不能直接相加当作墙钟总时间；未包含的前台执行、排队、等待唤醒等也不能由差值精确归因。每阶段调用数、平均值可由sum_ns/calls计算，最大值见diagnostics.json；未采集直方图/分位数。','','| 工作负载 | 线程 | 目标分配 | 分配持久化屏障 | 目标描述符发布 | 数据完成屏障 | 叶发布 | 额度提交 | NV回收 |','| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |']
for rw in ['randwrite','write']:
    for jobs in [1,16]:
        b=dl[rw,jobs,'before'];a=dl[rw,jobs,'after']
        vals=[(b['phases'][k]['sum_ns']/1e9,a['phases'][k]['sum_ns']/1e9) for k in ['allocate','allocation_barrier','target_publish','data_barrier','leaf_publish','credit_publish','reclaim']]
        lines.append(f"| {labels[rw]} | {jobs} | "+' | '.join(f'{x:.3f}→{y:.3f}' for x,y in vals)+' |')
lines+=['','## 解释与限制','','- 批量接口是否生效以分配次数、无数据重persist测试和fence计数判断；总带宽是否提升以release三轮结果判断，两者不能互相替代。','- 双副本读写带来新控制面成本；本轮没有声称消除全部逐页工作、全部共享锁或实现前台NVPage批量写入。实际页身份校验仍必须保留。','- fast后端persist/fence主要为编译器屏障和共享原子计数，不等于真实NVDIMM的CLWB/SFENCE延迟。真实硬件收益不能从这里直接外推。','- 完整SSD孤儿分配、命名空间及全部EXT4元数据掉电一致性仍是009未完成边界。e2fsck成功也不能替代CRC、跨VM重启逐文件读回或硬件掉电实验。','- 完整回归为SMP1，真实盘普通启动为64vCPU；未宣称修复历史SMP2 RCU测试问题。','','## 证据与复现','','- [验证摘要](nvpage-batch-20261018/validation.json)、[release逐点CSV](nvpage-batch-20261018/throughput/all-results.csv)、[release统计](nvpage-batch-20261018/throughput/summary.json)、[诊断计数与阶段时间](nvpage-batch-20261018/profile/diagnostics.json)、[最终kernel日志](nvpage-batch-20261018/batch-green4.log)。','- raw目录按throughput/profile、round、before/after保存guest日志、manifest、磁盘特性和e2fsck；release-images/profile-images保存完整kernel补丁（包含新增源文件）及镜像hash，不拷贝大型ISO。','- 大型镜像与编排脚本：`/home/zn/asterinas-build-env/nvpage-batch-20261018/`。`run-ab.py`运行三轮，`--profile`运行单轮诊断；两者必须串行。','- `run.py --kernel-source-diff`绑定所选历史ISO对应的冻结补丁，同时保留启动时工作树补丁hash。源码、镜像和脚本快照均在raw目录；当前工作树恢复为诊断关闭的最终实现。','']
(root/'docs/experiments/reports/NVPAGE-BATCH-TRANSACTIONS-2026-10-18.md').write_text('\n'.join(lines))
validation=dict(kernel_ktest_passed=307,kernel_ktest_failed=0,kernel_smp=1,throughput_cases=24,profile_cases=8,bytes_per_case=2147483648,fio_errors=0,e2fsck_failures=0,journal=False,max_in_flight=128,descriptor_image_version=4,full_ssd_namespace_crash_consistency_proven=False,real_nvdimm_tested=False,independent_review='no concrete production defect; fragmented allocation coverage gap fixed')
(raw/'validation.json').write_text(json.dumps(validation,indent=2)+'\n')
print(root/'docs/experiments/reports/NVPAGE-BATCH-TRANSACTIONS-2026-10-18.md')
