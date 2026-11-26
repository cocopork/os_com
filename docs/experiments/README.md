# 实验目录

本目录统一保存仓库相关实验的测试脚本、运行日志、原始结果、聚合数据、报告和绘图。
新的实验不得再把这些材料拆分到独立的 `scripts/`、`reports/` 或 `test_results/`
目录中。

## 目录约定

```text
docs/experiments/
  <测试类型>/
    formal/
      <实验名称>-<YYYYMMDD>/
        README.md
        scripts/
        logs/
        raw/
        plots/
    temporary/
      <诊断名称>-<YYYYMMDD>/
        README.md
        scripts/
        logs/
        raw/
        plots/
```

- 一级目录表示测试类型。
- 二级目录只能是 `formal` 或 `temporary`。
- 三级目录表示一次具体实验，名称必须包含日期。
- `formal` 保存完整测试矩阵；`temporary` 保存问题定位、消融和少量坐标复测。
- 每次实验只在自己的三级目录内保存材料；不存在的材料无需创建空目录。
- `README.md` 记录被测提交、镜像哈希、设备、缓存配置、完整命令、结果结论和已知异常。
- `scripts/` 保存可复现脚本，`logs/` 保存必要运行日志，`raw/` 保存机器可读原始数据，
  `plots/` 仅保存最终汇总图。

## 测试类型

| 一级目录 | 内容 |
|---|---|
| `raw-device` | 无文件系统的块设备与 virtio 驱动性能 |
| `fio` | 文件系统 fio 基础性能和 full-e2e 矩阵 |
| `filebench` | Filebench 应用负载 |
| `mixed-sync` | 同一文件内同步/非同步混合写专用负载 |
| `sqlite` | SQLite 负载 |
| `leveldb` | LevelDB 负载 |
| `validation` | 新功能、重构和优化的逻辑正确性及局部性能验证 |

## 保留规则

正式实验保留复现脚本、最终原始结果、聚合表、最终汇总图和必要日志。临时实验在结论写入
设计/实现文档后，只保留复现该结论所需的脚本、关键数据和报告。以下内容不进入仓库：

- Python `__pycache__` 和 `*.pyc`；
- 容器 ID、QMP 握手转储、重复的 QEMU inspect 信息；
- 已被相同配置的新实验完整替代的旧 CSV 和旧图；
- 可由保留的原始数据重新生成的中间图；
- 仅记录失败重试过程、且不影响最终结论的临时输出。

若旧实验与新实验的配置不同，不得仅因日期较早而删除；应放入对应测试类型的
`temporary/`，并在报告中说明其诊断目的。
