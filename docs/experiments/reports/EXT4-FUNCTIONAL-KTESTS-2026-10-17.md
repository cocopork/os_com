# EXT4 功能级 ktest 合并记录（2026-10-17）

## 结果与边界

将 570 个 EXT4 独立 ktest 入口归并为 108 个功能级入口，减少 462 个（81.1%）。570 个原始场景函数及其断言、夹具均保留，只移除原注册属性，改由功能入口调用。23 个 PageCache 测试维持原结构。因此，原 EXT4 + PageCache 的 593 个入口变为 131 个。

这是测试组织重构；没有新增 EXT4 功能，也不承诺缩短测试计算时间。独立夹具保留各场景状态隔离，未把不同操作强行拼成共享文件系统状态的巨型测试。单个功能测试中的某个场景 panic 后，剩余场景不会继续；其他功能测试仍由 ktest runner 运行。

## 分组

按操作和行为分组，而非整个模块合成一个测试。例如 truncate、fallocate 的各操作、目录增删与遍历、extent 插入/拆分/删除、journal capture/commit/checkpoint/replay 分开验证各自的正常、边界、错误及恢复场景。CRC 性能测试单独保留，x86 专用条件不变。

[完整映射](/home/zn/asterinas-build-env/repository-docs-artifacts-20261019/top-level-results/EXT4-FUNCTIONAL-KTESTS.json) 记录每个功能入口及其全部原场景；路径使用 Rust 完整模块名。修改测试时应同步维护映射。

| EXT4 文件 | 原场景/入口数 | 合并后入口数 |
| --- | ---: | ---: |
| `allocator/free_space.rs` | 2 | 1 |
| `allocator/mod.rs` | 4 | 1 |
| `block_group.rs` | 10 | 2 |
| `checksum.rs` | 9 | 3 |
| `fs.rs` | 73 | 16 |
| `impl_for_vfs/inode.rs` | 3 | 1 |
| `inode/dir/hash.rs` | 5 | 1 |
| `inode/dir/htree.rs` | 10 | 1 |
| `inode/dir/mod.rs` | 44 | 9 |
| `inode/extent_manager/es.rs` | 6 | 1 |
| `inode/extent_manager/mod.rs` | 5 | 2 |
| `inode/extent_manager/node.rs` | 6 | 1 |
| `inode/extent_manager/tree.rs` | 72 | 11 |
| `inode/mod.rs` | 111 | 21 |
| `inode/symlink.rs` | 5 | 1 |
| `journal/checkpoint.rs` | 15 | 1 |
| `journal/commit.rs` | 15 | 2 |
| `journal/format.rs` | 39 | 4 |
| `journal/mod.rs` | 41 | 11 |
| `journal/recovery.rs` | 34 | 4 |
| `journal/revoke.rs` | 3 | 1 |
| `journal/transaction.rs` | 25 | 5 |
| `nv_tests.rs` | 5 | 1 |
| `super_block.rs` | 23 | 5 |
| `writeback.rs` | 5 | 2 |
| **合计** | **570** | **108** |

## 定位失败与选择测试

每个功能入口使用 `test_utils::scenarios!`，由公共函数输出 `EXT4_SCENARIO_BEGIN <原完整路径>` 和 `EXT4_SCENARIO_PASS <原完整路径>`。宏只负责从函数标识符生成场景名，避免名字与实际调用脱节；执行和日志逻辑留在普通函数中。失败断言仍保留原文件和行号，最后一个 BEGIN 没有对应 PASS 的场景就是当前失败位置。

原场景已不再独立注册。OSDK 过滤应使用映射中的新入口，例如 `ext4_free_space_index`，而不是旧场景名。可在开发容器内的 `/root/asterinas/kernel` 执行：

```sh
CONSOLE=ttyS0 SMP=1 MEM=4G cargo osdk test ext4_free_space_index --qemu-args='-enable-kvm -cpu host -display none'
```

## 验证

- 保存任务开始时的全部 40 个 EXT4 Rust 源文件及原 570 项清单，审计先在重构前失败，再在合并后通过。
- 静态审计：570 个原场景恰好各归属一次；原函数体逐字节一致；108 个注册入口与映射一致。
- 独立只读审查：剥离注册、功能入口及测试辅助代码后，40 个文件的非空行与任务基线完全一致；生产代码、原断言与 cfg 均未改变。最终宏形式审查无发现。
- 首轮聚焦测试：131 passed，0 failed；日志确认全部 570 个原场景各开始并通过一次。
- 最终宏版本完整 kernel ktest：**289 passed，0 failed，0 filtered out**，进程退出码 0（108 EXT4 + 23 PageCache + 158 其他内核测试）。日志审计确认全部 570 个原 EXT4 场景恰好各开始并通过一次。
- `cargo fmt --all --check` 和 `git diff --check` 通过。任务外已有修改的 diff 与任务开始时一致。

全部开发及 QEMU 测试使用现有 Docker 容器，SMP=1、内存 4 GiB；没有访问或格式化真实 NVMe 测试分区。这里的“完整”指 kernel crate 所注册的 ktest，不包含其他 crate 或用户态回归测试。

证据目录：`/home/zn/asterinas-build-env/ext4-test-consolidation-20261017/`。包含 `before/`、`inventory.json`、`groups.json`、`audit.py`、`focused.log`、`full.log` 与对应命令。可执行 `python3 <证据目录>/audit.py <证据目录>/full.log` 复核函数保留和实际场景覆盖。

## 后续 delalloc 增量

上述108/570/289是测试合并任务完成时的历史结果。后续 delalloc 实现在保留全部570个原场景函数体的基础上，增加10个功能入口，并向容量与ES功能组各增加1个命名场景；当前映射为118个 EXT4 入口。`kind: inline_functional_test` 的新增入口直接包含操作断言，`scenarios` 为空。最终完整 kernel 为299项；详见 [delalloc 实现及验证](EXT4-DELALLOC-2026-10-17.md)。旧证据目录的原始审计针对历史基线；当前映射/覆盖使用新证据目录的 `audit.py`。
