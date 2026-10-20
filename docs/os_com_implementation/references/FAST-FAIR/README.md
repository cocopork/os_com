# FAST / FAIR 论文提取

- **题目：** Endurable Transient Inconsistency in Byte-Addressable Persistent B+-Tree
- **作者：** Deukyeon Hwang、Wook-Hee Kim、Youjip Won、Beomseok Nam
- **会议：** FAST 2018，pp.187–200
- **来源：** [本地 PDF](../FAST&FAIR.pdf)，15 页（含 1 页封面）；提取日期 2026-10-20。

## 阅读入口

1. [中文速读 TLDR](results/tldr.md)：仅基于摘要与引言。
2. [核心内容拆解 L0](results/hls-L0.md)：背景、问题、根因、关键思想、机制与实验。
3. [章节逻辑 L1](results/hls-L1.md)：论证链、图表定位和 Mermaid 依赖图草稿。
4. [批判性分析](analysis/critical-assessment.md)：强项、限制、研究空白与相关方案定位。

## 原文材料

- [按页与双栏整理的全文](analysis/full-text.md)
- [完整页面文本备用版本](analysis/full-text-layout.md)
- [8 张图及说明](analysis/figures.md)，另保留 [页面渲染目录](analysis/pages/)
- [49 条参考文献](analysis/references.md)
- [元信息与摘要](analysis/metadata.json)

## 核心提醒

FAST 通过重复指针让读者忽略未完成的键更新；FAIR 通过先发布 sibling link 再完成结构调整保持可达性。无日志不等于无 flush/fence，无锁读不等于完整事务隔离，恢复可用性也不等于论文已经完成系统化真实掉电验证。

## 提取与质量说明

自动脚本最初出现双栏交错、误识别 12 张图和遗漏参考文献，现已改为分栏读取、手工定位 8 张图并恢复 49 条引用。PDF 的跨栏图、算法和数学符号仍以原图为准；保留完整页面提取以避免跨栏裁切信息丢失。中文分析的每个主要结论均给出章节或图号，评价与原文描述分别保存。L1 是待读者核对的论证结构重建稿。
