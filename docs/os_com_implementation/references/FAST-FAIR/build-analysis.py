from pathlib import Path
import pymupdf as fitz
import json,re
root=Path(r'E:\GGBoy\HSSL\Project\Ant\AchillesFS\relate_works')
out=root/'FAST-FAIR-extracted'; a=out/'analysis'; r=out/'results'
doc=fitz.open(root/'FAST&FAIR.pdf')
figs=[
(1,4,(313,65,542,220),'FAST Inserting (25, Ptr) into B-tree node','§3.2–3.3：重复指针使尚未完成的键更新可被读取识别并忽略。'),
(2,6,(69,65,543,196),'FAIR Node Split','§4.1：先持久化兄弟节点，再发布 sibling pointer，最后截断原节点并更新父节点。'),
(3,10,(70,65,300,183),'Linear vs. Binary Search','§6.2：节点大小与线性/二分查找、插入开销的权衡。'),
(4,10,(69,184,300,331),'Range Query Speed-Up from using SkipList with Varying Selection Ratio (AVG. of 5 Runs)','§6.3：相对 SkipList 的范围查询加速，横轴为选择率。'),
(5,10,(313,65,543,230),'Breakdown of Time Spent for B-tree Insertion (AVG. of 5 Runs)','§6.4：分解 clflush、search 和 node update，包含 FAST+Logging 消融。'),
(6,11,(68,65,544,202),'Performance Comparison of Single-Threaded Index (AVG. of 5 Runs)','§6.4–6.5：(a) 读延迟、(b) TSO 写延迟、(c) non-TSO 写延迟。'),
(7,12,(69,65,300,240),'TPC-C benchmark (AVG. of 5 Runs)','§6.6：W1–W4 改变事务类型配比；完整配比保留在图片中。'),
(8,13,(68,65,544,205),'Performance with Varying Number of Threads (AVG. of 5 Runs)','§6.7：search、insert、mixed 随线程数变化；注意隔离语义和编译设置。')]
# Remove only the generated, incorrectly detected figure artifacts.
for p in (a/'figures').glob('fig-*.png'): p.unlink()
fd=['# Figures\n\n8 figures, 0 tables. PDF page = printed page − 185. Crops include original captions; full pages are retained in `pages/`.\n']
for n,pg,box,cap,ctx in figs:
 doc[pg-1].get_pixmap(matrix=fitz.Matrix(3,3),clip=fitz.Rect(*box)).save(a/f'figures/fig-{n:02d}.png')
 fd.append(f'## Figure {n} — PDF p.{pg} / printed p.{pg+185}\n\n**Caption:** {cap}\n\n![Figure {n}](figures/fig-{n:02d}.png)\n\n**Context:** {ctx}\n')
(a/'figures.md').write_text('\n'.join(fd),encoding='utf-8')
(a/'full-text-layout.md').write_text('# Full-page text extraction (layout fallback)\n\n'+'\n\n'.join(f'<!-- PAGE {i+1} -->\n\n'+p.get_text(sort=True) for i,p in enumerate(doc)),encoding='utf-8')
title='Endurable Transient Inconsistency in Byte-Addressable Persistent B+-Tree'
meta={'title':title,'authors':['Deukyeon Hwang','Wook-Hee Kim','Youjip Won','Beomseok Nam'],'venue':'16th USENIX Conference on File and Storage Technologies (FAST 2018)','year':2018,'pages':'187–200','pdf_pages':15,'source':str(root/'FAST&FAIR.pdf'),'paper_url':'https://www.usenix.org/conference/fast18/presentation/hwang','figure_count':8,'table_count':0,'reference_count':49,'extracted_at':'2026-10-20','notes':['PDF includes a cover page.','Column extraction can split wide figure captions; full-page text and rendered pages are retained.','Analysis is based on the supplied PDF, not an external literature survey.']}
# Abstract extracted from the original first body-page left column.
p=doc[1]; t=p.get_text(clip=fitz.Rect(65,100,305,730),sort=True)
meta['abstract']=t.split('Abstract',1)[-1].split('1  Introduction')[0].strip()
(a/'metadata.json').write_text(json.dumps(meta,ensure_ascii=False,indent=2),encoding='utf-8')
def front(level,desc):
 return f'''---
type: Research
title: "{title}"
description: "{desc}"
tags: [paper-extract, persistent-memory, B-tree, crash-consistency]
status: draft
level: {level}
generated: {{ by: paper-extract, at: "2026-10-20" }}
sources:
  - id: src-1
    resource: "E:/GGBoy/HSSL/Project/Ant/AchillesFS/relate_works/FAST&FAIR.pdf"
    title: "{title}"
    retrieved: "2026-10-20"
---

'''
l0='''# 1. Overview

本文为字节可寻址持久内存上的 B+ 树设计 FAST（Failure-Atomic ShifT）和 FAIR（Failure-Atomic In-place Rebalance），通过有序原地修改与能够容忍中间状态的读取协议，实现无需更新日志的移位和结构调整，并支持无锁读取与惰性恢复。**Evidence:** Abstract、§1，PDF pp.2–3 / 论文 pp.187–188。

# 2. Big Background

持久内存将访问单位从块设备页面转向 CPU load/store 与 cache line，但论文采用的 x86 平台只保证 8-byte 写原子性，cache line 则为 64 bytes。较大的树节点更新无法自然获得整体原子性；CPU 可见顺序和真正落入 PM 的持久化顺序也必须区分。**Evidence:** §1、§2.2，PDF pp.2–4。

# 3. Small Background

既有 wB+-tree 通过追加写、slot-array 和 bitmap 避免有序数组大规模移位，代价是元数据和间接访问；NV-Tree、FP-tree 将部分索引置于易失内存以减少持久化维护，重启时需要重建；B-link tree 的 sibling pointer 为延迟父节点更新提供结构基础。本文保留有序数组，借用 sibling-link 思路，将问题转化为中间状态如何被读者解释。**Evidence:** §1、§4.1、§6 的基线说明，PDF pp.3、6、8–9；论文参考文献 [5][25][34][49]。这些比较是本文自身的定位，非独立复现结论。

# 4. Problems

1. **节点内移位昂贵。** 插入或删除需要移动多个 key-pointer 项，直接逐项持久化会带来大量 flush/fence。**Evidence:** §2.1、§3.1，PDF pp.3–4。
2. **跨节点重平衡难以原子完成。** 分裂涉及新节点、原节点及父节点，日志/COW 引入额外写入与同步。**Evidence:** §4.1，PDF pp.6–7。
3. **读者如何访问尚未完成的更新。** 避免日志后，中间状态可能在并发执行或崩溃后可见；直接使用普通遍历会漏读或错误解释条目。**Evidence:** §3.2、§5，PDF pp.4、7–8。

# 5. Root Cause

B+ 树的逻辑修改横跨多个原子写和缓存行，而传统方案要求整个修改表现为一致状态；这个要求将多字更新与日志、复制和读锁绑定。论文将不变量从“物理布局始终完整”改为“每个可见中间状态都有可识别、可正确读取的逻辑解释”。该方法仍依赖受控持久化顺序，不能容忍任意写重排或任意损坏。**Evidence:** §1 的贡献陈述、§3.2–3.3、§4.1、§7，PDF pp.3–7、13。

# 6. Key Idea

通过安排原子写顺序，让每个中间状态都能由读取协议识别并正确解释，从而在无需更新日志的情况下执行持久 B+ 树的原地修改。**Evidence:** Abstract、§1、§3.2，PDF pp.2–4。

# 7. Design Points

## 7.1 FAST：用重复指针标识暂时无效的键

插入时从右往左移动条目，保留相邻重复指针，使两个相同指针之间的键被读者忽略；写入新的目标指针成为该插入的逻辑提交点。删除时先用相邻指针覆盖目标指针，使键失效，再反方向压缩数组。跨缓存行时有序 flush，并执行必要 fence；这不是“完全不刷缓存”。**Evidence:** §3.1–3.5、Algorithm 1、[Figure 1](../analysis/figures/fig-01.png)，PDF pp.4–6。

## 7.2 FAIR：先保证 sibling 可达，再修改节点边界和父指针

分裂先分配并填充新节点、持久化其内容，再发布并持久化原节点的 sibling pointer，随后将 median pointer 置 NULL 截断原节点，最后向父节点插入分隔键。父节点尚未更新时可以沿 sibling link 查找，重叠数据按范围规则解释。合并/重新分配沿相反思路执行。**Evidence:** §4.1–4.2、Algorithm 2、[Figure 2](../analysis/figures/fig-02.png)，PDF pp.6–7。这里避免的是额外恢复日志，并非分裂时完全不复制条目或不分配节点。

## 7.3 无锁读取：扫描方向、重复指针检查和版本复查

插入阶段读者从左向右扫描，删除阶段从右向左扫描；switch counter 的奇偶指示方向，扫描前后检查计数器是否改变，改变则重试。检查 key 与 pointer 的一致性，并利用 sibling pointer 跨越未完成的结构更新。写线程仍需要写锁。**Evidence:** §5、Algorithm 3、§6.7，PDF pp.7–8、12。

## 7.4 惰性恢复与隔离级别分离

读者跳过可容忍的不一致，不负责修复；后续写者清理重复条目或补全父节点指针。论文的无锁读取模式属于 read uncommitted，可能出现脏读与幻读；为提高隔离，作者另给出叶节点加读锁的版本。**Evidence:** §5.1–5.2，PDF p.8；§6.7，PDF p.12。因此“崩溃后可读”不等于完整数据库事务语义或所有查询均串行化。

# 8. System & Experiment

## 实现与平台

- x86：双 Intel Xeon Haswell-EX E7-4809 v3、64 GB DDR3；使用 Quartz 在 DRAM 上模拟 PM 延迟，假设带宽同 DRAM。§6.1 报告 g++ 4.8.2 与 -O3；§6.7 多线程实验另说明禁用 -O3，理由是编译器重排会影响无锁实现正确性。**Evidence:** PDF pp.9、12。
- non-TSO：Nexus 5 / Snapdragon 800 / 2 GB DDR，使用 dmb 和额外指令模拟 PM 延迟；该平台原子写粒度为 4 bytes，不能把所有实验都描述成 8-byte 写。**Evidence:** §6.5，PDF p.11。
- 基线：wB+-tree、FP-tree、WORT、SkipList；并发实验加入 B-link tree，没有实现 WORT/wB+-tree 的多线程版。**Evidence:** §6、§6.7，PDF pp.8–9、12。

## 实验与主要结果

| 实验 | 论文报告的结果 | 证据及边界 |
|---|---|---|
| 节点大小、查找方式 | 小节点上线性搜索可优于二分；后续默认 512 B，部分实验另有设置 | §6.2 / Fig.3；范围查询使用 1 KB |
| 范围查询 | 相对 SkipList 最高约 20×；比 FP-tree 快 6–27%，比 wB+-tree 快 25–33% | §6.3 / Fig.4；10M keys、PM read 300 ns、1 KB nodes |
| FAIR 的增量效果 | FAST+Logging 比 FAST+FAIR 慢 7–18% | §6.4 / Fig.5；不要误写成所有工作负载的整体收益 |
| PM 延迟变化 | FAST+FAIR 多数配置较强，但 WORT 在高写延迟插入中更快，FP-tree 在较高读延迟下点查询略快 | §6.4 / Fig.6 |
| non-TSO | 相对 wB+-tree 最高 1.61×；低延迟时可能逊于 FP-tree，原因是更多 barrier | §6.5 / Fig.6(c) |
| TPC-C | 在所测 W1–W4 配比中优于比较的索引，作者归因于插入与有序范围访问 | §6.6 / Fig.7；不能推导为完整数据库系统吞吐优势 |
| 多线程 | 16 threads 相对自身单线程：search 约 11.7×、insert 约 12.5×；mixed 最高约 11.44× | §6.7 / Fig.8；不是相对其他树的倍率 |
| 恢复证据 | 通过并发访问中间状态论证即时可用性 | §6.7；未提供系统化掉电注入与重启恢复时间曲线 |

**阅读定位：** 全文见 [full-text.md](../analysis/full-text.md)，图表见 [figures.md](../analysis/figures.md)，评价与未解决问题见 [critical-assessment.md](../analysis/critical-assessment.md)。
'''
(r/'hls-L0.md').write_text(front('L0','FAST/FAIR 的问题、根因、机制与实验拆解')+l0,encoding='utf-8')
tldr='''# TLDR

**Source:** FAST 2018, pp.187–200。此文件仅依据 Abstract 与 §1（PDF pp.2–3），不引入后续设计或实验细节。

## Overview
(1) 本文让持久内存上的 B+ 树通过 FAST 与 FAIR 执行原地移位和重平衡，避免昂贵的更新日志与整块复制。
(2) 关键在于接受可识别、可容忍的中间状态，让读取在更新未完成时仍能解释树内容。

## Big Background
(1) 字节可寻址 PM 允许使用内存访问方式实现持久数据结构，传统磁盘页面级假设需要重审。
(2) 8-byte 原子写与 cache-line 传输粒度不一致，使多字段树更新可能在崩溃后只保留一部分。

## Small Background
(1) 既有 PM B+ 树使用追加布局、额外元数据，或将内部节点保留在 DRAM 来降低维护成本。
(2) 这些选择分别牺牲有序访问的局部性，或需要重启后重建；作者希望保留 B+ 树本身的访问优势。

## Key Idea
(1) 将“更新期间必须维持完整物理一致性”改为“每一步都保留可由读取正确解释的状态”。
(2) 相邻重复指针提供识别未完成更新的线索，使中间状态的处理进入读协议，而不完全依赖日志恢复。

## Design Points
(1) FAST 将节点内有序数组移位组织成可容忍的状态转换，降低持久化控制开销。
(2) FAIR 将这一思路扩展到原地重平衡，减少结构变化对日志的依赖。
(3) 通过让读者容忍写者的中间状态，支持不等待写者完成的读取；这不是对完整事务隔离的声明。

## Implications
(1) 可持久化数据结构的设计空间不仅包括日志和 COW，也包括对更新顺序与读者解释规则进行共同设计。
(2) 本文摘要报告相对已有持久索引的性能优势，但量化结果与适用条件应查阅 L0 和原文实验部分。
'''
(r/'tldr.md').write_text(front('TLDR','仅基于摘要与引言的中文速读')+tldr,encoding='utf-8')
l1='''# Section Dependency Graph

以下为从原文重建的论证结构草稿，不表示作者提供了此图。

```mermaid
graph TD
 A["§1 问题与洞见"] --> B["§2 原子性与顺序约束"]
 B --> C["§3 FAST 节点内更新"]
 C --> D["§4 FAIR 跨节点更新"]
 C --> E["§5 无锁读取与惰性恢复"]
 D --> E
 C --> F["§6 性能与并发评估"]
 D --> F
 E --> F
 B --> G["§7 相关工作与模型边界"]
 F --> H["§8 结论"]
 G --> H
```

# Section-by-Section Flow

| 章节 / PDF 页 | 目的与 A→B→C 论证链 | 图片及依赖 |
|---|---|---|
| Abstract / p.2 | PM 粒度不匹配 → 中间状态可容忍 → 无日志更新与非阻塞读 | 无；总览 |
| §1 Introduction / pp.2–3 | PM 改变存储接口 → 现有优化牺牲局部性或恢复速度 → 提出 FAST/FAIR | 无；引出 §2 |
| §2 B+-tree for Persistent Memory / pp.3–4 | 移位与 COW 成本 → volatile/persist order 不同 → 明确设计必须控制的写顺序 | 无；为 §3 提供硬件前提 |
| §3 Failure-Atomic ShifT / pp.4–6 | 有序移位 → 重复指针标识无效键 → 插入/删除可恢复，并讨论 non-TSO | [Fig.1](../analysis/figures/fig-01.png)，Algorithm 1；依赖 §2 |
| §4 Failure-Atomic In-place Rebalancing / pp.6–7 | 分裂涉及多节点 → sibling 先可达、再截断与更新父节点 → 分裂/合并无需额外更新日志 | [Fig.2](../analysis/figures/fig-02.png)，Algorithm 2；依赖 FAST 和 B-link 思路 |
| §5 Lock-Free Search / pp.7–8 | 并发扫描可能漏过移动条目 → 方向控制与 counter 复查 → 无锁读、隔离级别讨论及写者惰性修复 | Algorithm 3；依赖 §3–4 |
| §6 Experiments / pp.8–12 | 建立基线与 PM 模拟配置 → 分项解释查找/持久化开销 → 混合应用及并发扩展性 | Fig.3–8；检验 §3–5 |
| §7 Related Work / pp.12–13 | 与其他无锁树、persistency 模型、HTM 对比 → 明确写锁和持久化顺序仍必要 → 给出适用边界与未来工作 | 无；补充 §2、§5 的限定 |
| §8 Conclusion / p.13 | 汇总 FAST/FAIR → 强调中间态可读 → 总结无日志、原地更新与无锁搜索 | 无；依赖 §3–7 |

# Evaluation Figure Placement

- [Fig.3](../analysis/figures/fig-03.png) / §6.2：先确定节点大小与扫描方式，解释为什么采用小节点线性扫描。
- [Fig.4](../analysis/figures/fig-04.png) / §6.3：验证保留有序叶节点的范围查询收益。
- [Fig.5](../analysis/figures/fig-05.png) / §6.4：分解插入成本，通过 FAST+Logging 体现 FAIR 的增量。
- [Fig.6](../analysis/figures/fig-06.png) / §6.4–6.5：改变 PM 延迟和 CPU ordering 条件，展示性能边界。
- [Fig.7](../analysis/figures/fig-07.png) / §6.6：在 TPC-C 不同事务配比中观察综合效果。
- [Fig.8](../analysis/figures/fig-08.png) / §6.7：比较线程扩展性，区分无锁读版本与 LeafLocks 版本。

# 论证衔接中应保留的限定

1. §3 的可恢复中间态依赖 §2/§7 的顺序假设，不能把它推广为任意乱序下自动安全。
2. §5 的读取正确性不直接等价于完整事务隔离，§5.1 明确承认 read-uncommitted 的限制。
3. §6.7 对并发中间态的实验支撑并不能替代所有持久化故障模式的检验；该判断属于分析，详见 E3。
'''
(r/'hls-L1.md').write_text(front('L1','章节论证链、配图与依赖关系草稿')+l1,encoding='utf-8')
crit='''# Critical Assessment

**范围：** 仅基于所提供的 FAST 2018 原文；下列评价是分析者判断，不是外部复现实验或截至今日的完整创新性调查。页码均指 PDF 页码。

## Innovation Level

**判断：Novel（已知问题上的新机制组合与不变量设计）。** 主要亮点不是第一次使用 sibling pointer 或 lazy update，而是把每个原子写后的可读语义、持久化顺序和并发遍历统一设计。作者明确联系 soft updates / NoFS，也采用 B-link tree 的 sibling 结构，故不能把全部组成机制视为首次提出。**Evidence:** §1 p.3、§3 pp.4–6、§4 pp.6–7、§5 pp.7–8。

## Strengths

1. **根因与方案对应明确。** 对多字逻辑更新和原子写粒度的差异，设计可由重复指针辨认的中间态，避免把问题全交给日志/COW。**Evidence:** §2.1、§3.2–3.3、Fig.1。
2. **节点内与跨节点更新形成完整路径。** FAST 配合 FAIR 的 sibling 发布顺序，使父节点更新可滞后；后续写者还能补全结构。**Evidence:** §4.1–4.2、§5.2、Fig.2。
3. **实验包含机制分析与边界。** 有节点大小、范围查询、延迟变化、non-TSO、FAST+Logging 消融及多线程比较；正文也报告 WORT/FP-tree 在部分配置下领先。**Evidence:** §6.2–6.7、Figs.3–8。

## Weaknesses / Evidence Limits

1. **恢复正确性的实验支撑不够直接。** §6.7 用并发读取部分更新节点解释即时恢复能力，但没有给出系统化掉电注入、实际持久化镜像检查与重启恢复曲线。并发可见状态与崩溃后的持久化状态不必完全相同，仍需要单独验证。**Evidence:** §6.7 p.12；§2.2 与 §7 区分 volatile/persist order。这是对证据覆盖面的评价，不是已发现反例。
2. **平台与编译器前提影响可移植性。** x86 使用 Quartz / DRAM 模拟 PM、带宽固定为 DRAM；non-TSO 采用手机与延迟注入。多线程实现还禁用了 -O3 以避免重排影响正确性，不能据此认定已具备生产级语言内存模型保证。**Evidence:** §6.1 p.9、§6.5 p.11、§6.7 p.12。对于真实 PM、现代编译器和 relaxed persistency，需要额外证据；后者也被 §7 列为未来工作。
3. **无锁模式的语义较弱，且并非整棵树操作均无锁。** 默认读模式存在脏读/幻读，写者仍持锁；叶节点加锁方案需要单独说明上层事务协议，不能由单点索引测试推导任意范围事务串行化。**Evidence:** §5.1 p.8、§6.7 p.12、§7 p.13。

## Gap Analysis

| 未解决或证据有限的问题 | 原文依据 | 可进一步验证的方向（分析建议） |
|---|---|---|
| 每个持久化切点的崩溃一致性 | §3/§4 的顺序推理；§6.7 以并发实验作支撑 | 在 store/flush/barrier 边界构造故障与重启检查，验证键集合及 reachability |
| 上层事务与强隔离 | §5.1 明确 read-uncommitted 与叶锁方案 | 分开测量结构正确性、范围查询隔离与事务原子性 |
| 真实 PM 和不同 persistency 模型 | §6.1 的模拟配置；§7 留待 future work | 给出硬件模型、持久化域和编译器排序保证，再测试 |
| 分配、回收及长时间运行的完整生命周期 | §4.1 使用外部 PM heap manager [33]；§5.2 修复结构 | 检查未发布节点、合并节点的回收与并发读者安全，避免把树算法等同整个存储栈 |

## Positioning Map

横轴是更新/恢复策略，纵轴是索引结构与持久化范围。以下按本文介绍归纳，并非独立查验每个基线。

| 系统 | 更新/恢复策略（横轴） | 结构与持久化范围（纵轴） | 文内定位依据 |
|---|---|---|---|
| FAST+FAIR | 有序原地修改、可容忍中间态、写者惰性修复 | 有序 B+ 树，叶与内部节点持久化 | §3–5 |
| wB+-tree | 追加写 + slot-array/bitmap，结构变更需日志 | B+ 树，全树 PM | §6，参考文献 [5] |
| FP-tree | 叶持久化 + fingerprint，内部节点重建 | SCM/DRAM 混合 B+ 树 | §1、§6，参考文献 [34] |
| WORT | 避免排序/重平衡，少量原子写 | 持久 radix tree | §6，参考文献 [23] |
| SkipList | 多层链表指针更新，底层需 failure atomicity | 概率型有序索引，本文对照实现 | §6，参考文献 [16]；原引用中的使用方式与本文持久对照不能简单混同 |

## 与文件系统研究的关联（分析推论）

值得借鉴的是“修改顺序 + 读取解释 + 惰性修复”的联合设计，而不是直接把 FAST/FAIR 当成通用无日志文件系统。其证明基础包括字节可寻址介质、原子指针写、受控持久化顺序和树结构不变量；迁移到块设备文件系统，需要重新定义故障原子粒度、跨块依赖以及并发/事务语义。**Evidence:** §2–5、§7。这里是适用边界分析，不是对 AchillesFS 当前实现作出的判断。
'''
(a/'critical-assessment.md').write_text(crit,encoding='utf-8')
readme='''# FAST / FAIR 论文提取

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
'''
(out/'README.md').write_text(readme,encoding='utf-8')
(out/'bibentry.bib').write_text('''@inproceedings{hwang2018endurable,
  title={Endurable Transient Inconsistency in Byte-Addressable Persistent B+-Tree},
  author={Hwang, Deukyeon and Kim, Wook-Hee and Won, Youjip and Nam, Beomseok},
  booktitle={16th USENIX Conference on File and Storage Technologies (FAST 18)},
  pages={187--200},
  year={2018},
  publisher={USENIX Association},
  url={https://www.usenix.org/conference/fast18/presentation/hwang}
}
''',encoding='utf-8')
print('Created analysis bundle: 8 figures, 49 references, L0, TLDR, L1, critical assessment, README, BibTeX.')
