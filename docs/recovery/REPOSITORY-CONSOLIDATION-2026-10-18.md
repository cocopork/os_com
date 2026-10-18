# 代码与设计文档工作区合并（2026-10-18）

## 当前维护入口

唯一的 os_com 开发工作区为：

```text
/home/zn/zn/recovery_Rust_EXT4/os_com_rnvpc_rebuild/
├── .git/                       # 独立仓库，不再依赖其他 worktree
├── kernel/
├── docs/recovery/               # rebuild 当前验证与实验记录
└── os_com_implementation/       # 设计、实现计划、历史实验与脚本
```

分支 `recovery/rnvpc-rebuild`，远端 `https://github.com/cocopork/os_com.git`。
在代码仓库根目录统一执行 `git status/add/commit/push`。设计文档为普通文件，
没有嵌套 `.git`，也不是 submodule；相关代码与文档应在同一提交中维护。

旧文档入口 `/home/zn/zn/Rust_EXT4/os_com_implementation` 保留为指向新位置的
兼容软链接，不包含另一套文件或仓库。主 README 已增加设计和恢复报告入口。
当前研究状态以 rebuild 的代码与实测报告为准，迁入的历史状态不会自动成为
当前实现能力的证明。

## 迁移与清理

- 原文档目录整体迁入，迁移后、路径修订前核对 417 个非 Git 元数据文件，SHA256 全部一致。
- 原仓库已跟踪文件及未忽略的新文件共 168 个，全部纳入本次统一版本管理；原本忽略的日志、结果与缓存继续留在本地且不入库。
- 备份后移除文档目录的嵌套 `.git` 和旧 `.worktrees` 元数据。
- 将 rebuild 从 linked worktree 提升为 standalone repository：保留当前 HEAD、index、分支、对象、远端和跟踪设置；HEAD 的原基点为 `d7de4e42c`。
- 对旧 `/home/zn/zn/Rust_EXT4/os_com` 与原主工作区 `/home/zn/zn/recovery_Rust_EXT4/os_com_reconstructed` 完整备份后移除。
- 原 `os-com-recovery-dev` 容器只运行 sleep，停止后移除；当前 `os-com-rnvpc-rebuild` 构建容器继续使用同一代码路径。

## 历史备份

归档目录：

```text
/home/zn/zn/recovery_Rust_EXT4/retired_workspace_backups/20261018-consolidation/
```

- `design-workspace.tar.gz`：迁移前的完整文档工作区，包括原始 Git 元数据、未提交文件和原来忽略的文件。
- `legacy-os_com.tar.gz`：旧 os_com 目录，包括其原始 Git 元数据与构建/实验文件。
- `reconstructed-workspace.tar.gz`：旧 reconstructed 主工作区的完整备份，包含 Git 元数据和 root 所有的构建文件。
- `archives.json`：三个压缩包的 SHA256、大小和成员数；压缩完整性均已验证。
- `design-files-before.json`、`design-versioned-paths.json`、`design-original-status.txt`：迁移前文件与版本管理清单。
- `design-git-fsck.txt`：旧文档 Git 的原有缺失对象记录。它无法导出完整历史 bundle，因此保留原始元数据归档；本次快照导入不依赖该损坏历史。
- `consolidate.py`、`update-paths.py`、`path-updates.json`：本次执行与路径调整记录。

备份为取证和回退用的压缩文件，不作为第二个活动开发仓库。

## 路径调整与验证

新增 `os_com_implementation/test/host-paths.sh`，从自身位置解析文档根与代码根。
15 个宿主 shell 入口改用该解析器；一个 Python 源码检查脚本改为定位父仓库。
旧容器在 `/root/impl` 单独挂载文档时，解析器可识别 `/root/asterinas` 代码根。
历史计划中的旧路径和提交号保留为来源记录；脚本中的历史设备、容器名和负载
默认值仍需结合当前实验配置核对，迁移不等于重新验证所有历史实验。

验证：

- 29 个 shell 脚本通过 `bash -n`，58 个 Python 文件通过语法解析。
- 宏测试 profile 的 5 项 unittest 通过。
- 基线 harness `--dry-run` 通过，无编译、QEMU 或设备写入操作。
- 从其他工作目录加载路径解析器成功定位当前代码仓库；构建容器可读取独立 Git 元数据。
- 移除旧主工作区后再次通过 `git fsck --connectivity-only --no-dangling`；`git worktree list` 仅列出 rebuild。
- Git 提交检查确认没有嵌套仓库或 gitlink；内核与 OSTD 内容没有迁移引起的改动。
- 迁移新增入口文件通过 whitespace 检查；历史原始日志与快照保留其原有 ANSI 输出、行尾空白及内容，不对历史证据批量格式化。
