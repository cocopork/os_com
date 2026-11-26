# 证据目录

- development：开发阶段RED/中间失败/GREEN及最终完整kernel日志；包含仅关闭JBD2、尚未修复native导出时的单个2GiB点。
- final-export：最终镜像四个主测试点，磁盘保留干净journal、运行时无JBD2。
- no-disk-journal：最终镜像在磁盘无HAS_JOURNAL格式下的额外2GiB点。
- harness：最终脚本快照。运行时使用原始路径 docs/os_com_implementation/test/rnvpce-buffer-compare/，快照脚本的ROOT计算依赖该路径，不能直接从证据目录执行。
- validation.json：核对5个真实盘点、最终302/0内核回归及ISO/source哈希。

完整元数据掉电一致性与NVPage批量事务未据此认定为完成。
