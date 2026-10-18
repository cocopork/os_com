> Archived: 2026-09-26. Historical handoff only. Current progress lives in ../../STATUS.md.

> Historical snapshot: every status statement, command, recommendation, partial result, and environment observation below records the 2026-09-24 handoff. Do not use it as current authority; consult ../../STATUS.md, ../../EXPERIMENTS.md, and current harness documentation.

# NEXT-SESSION 交接：对比对象配置改造 + 测试（2026-09-24 19:00 收尾）

> 上一轮在 18:5x 被人工停止：**smoke 跑完 3/14 点后停**，真盘已释放（无 harness 容器、无 `make run_kernel`），
> `asterinas-dev` 容器仍在运行（Up 2h）。**没有任何测试在跑**。

---

## 0. 目标（用户原话，见 `README.md`「无奈的妥协」）

- **sata**：保持现状（第 1 档 nojournal；第 2/3/4 档都是 journal 且同配置）。
- **nvme**：要求 `ext2-orderless ≤ ext4-orderless ≤ ext4-rNVPC ≤ ext4-rNVPCe`。
- 落地方式（2 个新挂载开关 + 五档配置表 + harness 档位命名 + 风险）已写进
  `README.md` 的「#### 落地方式」「#### 五个对比对象的精确配置」「#### harness 表达」「#### 已知前提与风险」四小节，**先读那四节**。

---

## 1. 一句话现状

内核两个开关（`wb_sync_wait` / `wb_merge`）**已实现且两个编译门禁通过**，harness 已支持 4 个新档位，
**但全部未提交**；修复了一个会让 harness「静默退出」的 bridge 软链 bug 后，smoke 实测 3 个点、
数据方向正确（同步等待惩罚巨大），**剩余 11 点 + 完整矩阵待跑**。

---

## 2. 已完成 / 待提交清单

### 2.1 已提交（本轮早些时候）
| 仓库 | commit | 内容 |
|---|---|---|
| `os_com` | `d1d08a73d` | filebench / sqlite 宏测试 job 目录（含 filebench 1.5-alpha3 二进制、静态 mke2fs） |
| `os_com_implementation` | `cf7190a` | 宏测试 harness（每点 1 轮 + smoke + 解析兜底 + 代理透传）+ README 方案记录 |

### 2.2 **未提交**（下一 session 第一件事：`git status` 确认后提交）
| 仓库 | 文件 | 内容 |
|---|---|---|
| `os_com` | `kernel/src/fs/fs_impls/ext4/fs.rs` (+91) | `Ext4MountOptions` 新增 `wb_sync_wait`/`wb_merge`（字段+默认+解析+访问器）；`WB_BIO_COUNT`/`WB_PAGE_COUNT`/`WB_SYNC_WAIT_{COUNT,NS}` 计数与 `[WBKNOB]` dump；**1 个新 ktest** `mount_options_parse_writeback_switches` |
| `os_com` | `kernel/src/fs/fs_impls/ext4/inode/extent_manager/mod.rs` (+42) | `submit_write_pages()`：`wb_merge=off` 强制逐页路径；`wb_sync_wait=on` 每 BIO 用临时 `IoBatch` 提交后立刻 `wait_all()`（helper `submit_maybe_wait`）；BIO/页计数 |
| `os_com` | `test/initramfs/src/benchmark/fio/aster_ext4_baseline/run.sh` (+23) | **harness 自动安装的产物**（新增 4 档的挂载选项）；提交它以保证可复现 |
| `os_com_implementation` | `test/aster-ext4-baseline-test/aster_ext4_baseline.sh` (+23) | fio guest 脚本新增 4 档：`ext4_sync` / `ext2like_sync` / `ext4_nomerge` / `ext2like_nomerge` |
| `os_com_implementation` | `test/aster-ext4-baseline-test/run-baseline.sh` (+5) | **关键修复**：`docker run` 增加 `-v "${REPO_HOST}/.impl-host:${REPO_HOST}/.impl-host"`（见 §4.1，勿回退） |
| `os_com_implementation` | `test/aster-ext4-baseline-test/.build-stamp` | 未跟踪的印章文件，建议加进 `.gitignore` |

建议提交信息（两个仓库各一条，沿用仓库的 trailer 风格）：
```
feat(ext4): 新增 wb_sync_wait / wb_merge 两个回刷开关（对照矩阵用）

- wb_sync_wait=on：回刷线程每提交一个 BIO 就等它完成（per-BIO 串行），默认 off（现状）
- wb_merge=off：关闭数据页"物理连续页合并多段 BIO"（每页 4KiB 一个 BIO），默认 on（现状）
- 前台/fsync 路径不动；新增 [WBKNOB] 度量（pages_per_bio / sync_wait_bios）
- 新增 ktest mount_options_parse_writeback_switches

Constraint: benchmark 框架最多转发 10 个位置参数 → 档位只能用 profile 名编码
Confidence: high（两个编译门禁通过，ktest 未跑）
Not-tested: ktest 全量、真盘矩阵
```

---

## 3. 开关语义与注入点（实现细节速查）

| 开关 | 默认 | 语义 | 位置 |
|---|---|---|---|
| `wb_sync_wait=on\|off` | off | 回刷线程**每提交完一个 BIO 就等它完成**再继续 | `extent_manager::submit_write_pages()` 的两个提交点，包进 `submit_maybe_wait()`（临时 `IoBatch`+`wait_all`） |
| `wb_merge=off` | on | 关闭聚合，退化**每页 4 KiB 一个 BIO** | 同一函数：`let run = if self.nvpc_retire() \|\| !fs.wb_merge() { None } else { … }` |

- 等待点落在 `Inode::inner.write()` 之内（与原同步路径一致）；完成回调只碰 atomic + 页锁，不会自锁。
- 度量：`dump_latency_stats()` 里 `[WBKNOB] bios= pages= pages_per_bio= sync_wait_bios= sync_wait_ms=`，
  **受 `LATENCY_ENABLED` 门控**（默认 false；要验证开关生效可临时开一次，注意它会把吞吐压约 2×）。

---

## 4. 必须知道的坑（都踩过，附修法）

### 4.1 harness「静默退出」= results 悬空软链（**已修，勿回退**）
- 现象：`run-baseline.sh` 只打印 header + `mkdir: cannot create directory '/root/impl/test/aster-ext4-baseline-test/results': File exists` 两次，然后退出码 1、无任何其他输出、没有跑一轮。
- 根因：`os_com_implementation/test/aster-ext4-baseline-test/results` 是 bridge 建的**软链**，指向宿主绝对路径
  `os_com/.impl-host/test/aster-ext4-baseline-test/results`；该路径**没被挂进 harness 容器** → 悬空链 →
  容器内 `mkdir -p "${RESULTS_DIR}"` 报 `File exists` → 脚本开头 `set -e` → 静默退出。
- 修法：`run-baseline.sh` 的 `docker run` 加一行
  `-v "${REPO_HOST}/.impl-host:${REPO_HOST}/.impl-host"`（软链即解析，结果落进镜像 → 两容器都能看到）。

### 4.2 bridge 会整目录覆盖脚本
`/root/impl/**` 里的脚本是**一次性拷贝**；改了宿主脚本必须重跑
`bash os_com_implementation/test/aster-ext4-baseline-test/setup-impl-bridge.sh`。
**实验运行期间不要改这些脚本**（bash 增量读脚本，有被改坏的风险）。

### 4.3 nix 必须走代理
容器内**直连 `cache.nixos.org` = 000**（走宿主代理才 200）。不能替换时 nix 退化源码构建，
会撞 `bash52-004` 补丁在 ftpmirror 已 404 → `flex/coreutils/benchmark/initramfs` 全链失败。
`run-sqlite.sh` / `run-filebench.sh` 已实现 `proxy_env` 透传；`run-baseline.sh` 目前**没有**这一行
（若引入新 nix 依赖需补上）。

### 4.4 filebench 必须用自带的 1.5-alpha3
`test/filebench-test/bin/filebench`（212688 B，research1/P2CACHE 自建）。nixpkgs 的
`pkgsHostTarget.filebench` = **1.4.9.1，在本环境 worker 跑空（0 ops）**，且会把 `flex` 拖进依赖闭包。
详见 `/memories/repo/filebench-blocked.md`（含一行命令的复现与对比数据）。

### 4.5 sqlite / 大负载要 `MEM=16G`
4G 下 `speedtest1 --size 1000` 触发内核 `Failed to allocate a large slot` +
`Heap allocation error, layout size: 0x1000`，guest 直接挂；同轮 sqlite 还会 `rc=1`/`rc=139`。
两个宏 harness 默认已 `MEM="16G"`；**fio harness 仍是 `MEM=4G`**（为与既有基线可比，刻意不动）。

### 4.6 口径：1 轮/点 + 双稳
- 现在策略是**每点 1 轮**（sqlite `RUNS=1`）。smoke 用 1 轮看"能不能测出来"没问题，
  **正文数字建议 ≥3 轮取中位**：本环境同配置批间漂移可达 ~3×，randwrite 有双稳低态。
- 跨批次比较**不可用于归因**；引用任何点都要多次重复。

### 4.7 串行与并发
真盘（`/dev/sda4`、`/dev/nvme1n1p2`）与 KVM 性能实验**必须串行**；同一仓库**不要并发 `make`**（target 锁）。
宿主上常驻的 4 个 qemu 是别的实验，只提示不阻塞。

---

## 5. 待跑的测试（可直接复制）

### 5.1 先做（10 min）
```bash
# 编译门禁（两条都必须 0 error / 0 warning）
cd /home/zn/Ant/Rust_EXT4/os_com
docker exec -w /root/asterinas asterinas-dev bash -c 'cargo osdk check --package aster-kernel 2>&1 | tail -5'
docker exec -w /root/asterinas asterinas-dev bash -c "RUSTFLAGS='--cfg ktest -C panic=unwind' cargo osdk check --package aster-kernel 2>&1 | tail -5"

# ktest 全量（预期 724 passed / 3 failed，3 个已知失败见 README「当前验证基线」）
docker exec asterinas-dev bash -c "pkill -9 -f 'qemu-sys[t]em'; sleep 2"
timeout 3000 docker exec -w /root/asterinas asterinas-dev bash -c \
  'cargo osdk test --kcmd-args="ostd.log_level=error" --qemu-args="-accel kvm" > /tmp/ktest.out 2>&1; echo EXIT=$?'
grep -E "test result:" /home/zn/Ant/Rust_EXT4/os_com/qemu-serial.log | tail -1
```
判定：新增的 `mount_options_parse_writeback_switches` 必须出现在通过列表里。

### 5.2 smoke（14 点，约 25–30 min）—— 上一轮跑了 3 点
```bash
cd /home/zn/Ant/Rust_EXT4/os_com_implementation/test/aster-ext4-baseline-test
# nvme 五臂
bash run-baseline.sh --devices nvme --fs ext2like_sync,ext4_sync,ext4_nomerge,ext2like_nomerge,ext4 \
  --workloads randwrite-4k --modes orderless --njobs 1,8 --runtime 30 --smp 64 --timeout 300 \
  --file-range-mb 256 --wr-capacity-mb 256 --confirm-destroy
# sata 现状两臂
bash run-baseline.sh --devices sata --fs ext2like,ext4 \
  --workloads randwrite-4k --modes orderless --njobs 1,8 --runtime 30 --smp 64 --timeout 300 \
  --file-range-mb 256 --wr-capacity-mb 256 --confirm-destroy
```
看点（决定后续要不要调）：
1. `ext4_sync` / `ext2like_sync` vs `ext4` / `ext2like` → **同步等待的惩罚够不够大**；
2. `ext4_nomerge` / `ext2like_nomerge` vs 不带 nomerge → **聚合效应有多大**；
3. 目标顺序是否已经成立（重点看 nvme）。
可用 `--dry-run` 先看点位。

### 5.3 完整矩阵（顺序不成立也别急着跑，先调）
```bash
# fio 五臂 × nj1..32（真实头条数字）
bash run-baseline.sh --devices nvme --fs ext2like_sync,ext4_sync,ext2like_nomerge,ext4,ext4_nomerge \
  --workloads randwrite-4k,seqwrite-4k --modes orderless --njobs 1,4,8,16,24,32 \
  --runtime 30 --smp 64 --timeout 300 --file-range-mb 256 --wr-capacity-mb 256 --confirm-destroy

# 宏负载（每点 1 轮）
cd /home/zn/Ant/Rust_EXT4/os_com_implementation/test
bash run-macro-tests.sh --fs ext4,ext2like --filebench-workloads write-heavy,fileserver \
  --filebench-threads 4,8 --sqlite-sizes 1000 --sqlite-runs 1 --confirm-destroy
```

### 5.4 若不满足 `ext2-orderless ≤ ext4-orderless`
第 1/2 档**没有隔离旋钮**（都只是加了 `wb_sync_wait`）。备选第三把旋钮（代价递增）：
`nojournal_meta_batch=0` → `fsync_barrier=each` → `nojournal_meta_writeback=1`（008 记录过 5–490× 慢，慎用）。

---

## 6. 已有实测数据（供对照，均为单次抽样）

### 6.1 本轮 smoke（`results/20260924_184443_aster-ext4-baseline/`，被停前 3 点）
| 轮次（nvme, randwrite-4k orderless） | drain_bw |
|---|---|
| `ext2like_sync` nj1 | **50.6 MiB/s** |
| `ext2like_sync` nj8 | **8.3 MiB/s** |
| `ext4_sync` nj1 | **52.1 MiB/s** |
| `ext4_sync` nj8 | （中断；另一次单独跑 = **83.4 MiB/s**） |

挂载项已确认生效：`writeback_capacity_mb=256,wb_sync_wait=on,nojournal_meta_batch=2,nojournal_meta_writeback=2`

> 单次抽样 + 双稳，不要直接引用；smoke 的用途只是"看差异量级"。

### 6.2 28 轮宏矩阵（`results/20260924_171517_macro/`，已跑完）
**filebench**（`rNVPCe`=ext4/journal，`rNVPC`=ext2like/nojournal）
| device | workload | t | rNVPCe ops/s | rNVPC ops/s | 比值 |
|---|---|---|---|---|---|
| nvme | fileserver | 4 | 19,171 | 25,756 | 0.74 |
| nvme | fileserver | 8 | 18,721 | 26,280 | 0.71 |
| nvme | write-heavy | 4 | 20,623 | 24,208 | 0.85 |
| nvme | write-heavy | 8 | 19,771 | 27,029 | 0.73 |
| sata | fileserver | 4 | 17,622 | 16,464 | 1.07 |
| sata | fileserver | 8 | 17,492 | 16,750 | 1.04 |
| sata | write-heavy | 4 | 14,148 | 19,387 | 0.73 |
| sata | write-heavy | 8 | 15,676 | 19,390 | 0.81 |

**sqlite**（耗时越小越好）：nvme `rNVPCe 436.6s` vs `rNVPC 130.6s`（3.34×）；sata `301.5s` vs `168.5s`（1.79×）

**结论**：现状 `rNVPC（nojournal）明显快于 rNVPCe（journal）`，与目标顺序**相反** —— 这正是要做"妥协配置"的原因。

### 6.3 已知问题（待查，非配置差异）
1. **filebench `varmail` 8/8 全 crash**（两种 fs × 两块盘）。`summary.csv` 的 `crash_tail` 有现场。
2. 旧日志（解析器修复前）的 `mb_per_sec`/`latency_ms` 记为 0；真值在 `IO Summary` 原文里，可用
   `parse-filebench.py` 重跑（已加宿主侧兜底）。

---

## 7. 命令速查

```bash
# 容器
docker ps --format '{{.Names}} {{.Status}}' | grep asterinas-dev
docker exec -it asterinas-dev bash          # 进入开发容器

# bridge 同步（改完宿主脚本必做）
bash /home/zn/Ant/Rust_EXT4/os_com_implementation/test/aster-ext4-baseline-test/setup-impl-bridge.sh

# 停掉在跑的实验（只杀 harness，不动 asterinas-dev）
CID=$(docker ps --no-trunc --format '{{.ID}}|{{.Command}}' \
      | grep -E "run-baseline\.sh|run-sqlite\.sh|run-filebench\.sh" | awk -F'|' '{print $1}')
[ -n "$CID" ] && docker kill $CID

# 结果位置
#   fio      : test/aster-ext4-baseline-test/results/<ts>_aster-ext4-baseline/summary.csv
#   filebench: test/filebench-test/results/<ts>_filebench-macro/summary.csv
#   sqlite   : test/sqlite-test/results/<ts>_sqlite-macro/summary.csv
#   rNVPCe vs rNVPC 对照表: python3 test/summarize-macro.py <filebench.csv> <sqlite.csv>
```

## 8. 记忆文件（跨 session）
- `/memories/repo/os_com.md`：桥接/构建/ktest/宏测试要点
- `/memories/repo/filebench-blocked.md`：filebench 版本坑 + 冒烟数据 + 代理坑
