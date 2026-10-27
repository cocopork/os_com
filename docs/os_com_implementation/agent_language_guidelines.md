---
name: filesystem-language-guidelines
description: "Guide agents to discuss filesystem and operating-system research with precise technical language instead of informal metaphors, unexplained code names, or locally invented jargon."
scope: "docs/**"
---

# Filesystem Language Guidelines

Use this document when discussing, drafting, reviewing, or rewriting research files under `docs/`.
The goal is to communicate filesystem, storage, NVDIMM, crash-consistency, and OS design ideas with precise technical language that a systems researcher can audit without decoding private shorthand.
This guide applies to both English writing and Chinese conversations with Agents. Chinese discussion may be informal in tone, but technical terms, claims, and mechanism descriptions must remain precise.

## Use This Guide When

- explaining filesystem research motivation, design, evaluation, or implementation
- editing `core.md`, `route_discussion.md`, `motivation/`, `design/`, or related notes
- converting discussion notes into paper-style prose
- reviewing Agent output for informal language, hidden assumptions, or unexplained terms
- introducing Rust, typestate, persistent-memory, or crash-consistency mechanisms

## Non-Negotiable Contract

- Prefer standard filesystem, storage, operating-system, and Rust terminology over metaphors.
- In Chinese conversations, prefer precise Chinese technical terms plus the original English term when helpful.
- Define every project-local term the first time it appears.
- Do not use oral shorthand as if it were a technical concept.
- Do not introduce named mechanisms unless the problem, invariant, and enforcement point are stated.
- Do not let code identifiers replace explanation.
- Separate mechanism, policy, invariant, and evaluation claim.
- State the failure model before discussing crash consistency or recovery.
- State whether a claim is evidence, inference, design intent, or open hypothesis.

## Chinese Communication Rules

When communicating with the user in Chinese, Agents must still use research-grade terminology.
Chinese is the normal discussion language for this project, so the same rigor applies before text is translated into English.

- Use Chinese technical terms such as "写放大", "持久化顺序", "崩溃一致性", "恢复过程", "脏元数据", "块设备快照", "类型化持久对象", and "不变量".
- Keep the original English term in parentheses when the Chinese term may be ambiguous, for example "持久化屏障(persistence barrier)" or "类型状态(typestate)".
- Do not use "这个东西", "搞一下", "兜住", "打穿", "卡死", or similar vague verbs as the final description of a mechanism.
- For every mechanism, name the subject, operation, object, guarantee, and limit.
- For every performance claim, name the metric, workload, baseline, and evidence status.
- For every recovery claim, state which state survives the crash and which state is reconstructed.
- Prefer "该机制保证/不保证..." over slogan-like summaries.
- Prefer "证据显示/可以推断/设计意图是/尚未验证" over "实锤/应该/感觉".

Good Chinese technical prose can still be concise.
The requirement is not stiff wording; it is that every important noun and verb maps to a real filesystem, OS, Rust, or storage concept.

## Banned or Risky Language

Avoid metaphorical or conversational phrases in research-facing text unless they are immediately rewritten into precise terms.

Examples to avoid:

- "写侧踩爆邻居"
- "钉死尺寸"
- "读漏斗"
- "物化"
- "收编"
- "兜底"
- "判官"
- "盲判绿"
- "假绿"
- "永眠"
- "防挂"
- "实锤"
- "盘面"
- "红线"
- "铁律"
- "镜头"
- "信封"
- "手术"
- "双调"
- "排干"
- "播种"

When such a phrase appears in notes, treat it as a drafting marker, not final terminology.
Rewrite it into a concrete statement about data structures, persistence ordering, allocation state, recovery state, validation criteria, or test behavior.

## Preferred Rewrites

Use precise replacements like these:

| Informal phrase | Preferred technical language |
| --- | --- |
| 写侧踩爆邻居 | write amplification or writeback interference across unrelated objects |
| 钉死尺寸 | fixed-size layout, statically bounded object size, or compile-time size constraint |
| 读漏斗 | centralized read path, validation pipeline, or staged parsing path |
| 物化 | instantiate, materialize a typed in-memory object, or reconstruct persistent state |
| 收编 | integrate into the persistent object pool or migrate into the managed representation |
| 兜底 | fallback path, recovery fallback, or conservative validation path |
| 判官 | verifier, validator, checker, or recovery-time consistency checker |
| 盲判绿 | false pass without sufficient evidence |
| 假绿 | false positive test result or unsound verification pass |
| 永眠 | permanently unreachable state, unrecoverable blocked state, or quiescent state |
| 防挂 | prevent crash, avoid panic, bound failure propagation, or preserve recovery progress |
| 实锤 | direct evidence, reproduced result, or measured evidence |
| 盘面 | on-disk layout, persistent media state, or block-device state |
| 红线 | safety invariant, correctness requirement, or non-negotiable constraint |
| 铁律 | invariant, design rule, or required ordering constraint |
| 镜头 | observation point, evaluation scenario, or analysis perspective |
| 信封 | wrapper object, envelope metadata, or serialized record header |
| 手术 | localized refactoring, targeted rewrite, or migration step |
| 双调 | two-phase protocol, dual-path coordination, or paired state transition |
| 排干 | drain dirty data, flush pending writes, or complete outstanding writeback |
| 播种 | initialize state, seed test data, or create initial persistent records |

Pick the replacement that matches the actual mechanism.
If none fits, explain the mechanism directly instead of coining a new label.

## Chinese Preferred Rewrites

Use this table during Chinese discussion and Chinese document drafting.

| 口语/黑话表达 | 推荐中文技术表达 |
| --- | --- |
| 写侧踩爆邻居 | 写放大；无关对象之间的写回干扰；数据写回阻塞元数据持久化 |
| 钉死尺寸 | 固定大小布局；静态有界对象大小；编译期大小约束 |
| 读漏斗 | 集中的读取路径；分阶段解析路径；恢复时的统一校验流程 |
| 物化 | 构造内存对象；重建类型化状态；将持久化记录恢复为可访问对象 |
| 收编 | 纳入持久对象池管理；迁移到受管理表示；接入统一分配/回收协议 |
| 兜底 | 回退路径；保守恢复路径；最后一致性校验 |
| 判官 | 校验器；验证器；恢复期一致性检查器 |
| 盲判绿 | 缺少证据的通过判定；未覆盖关键场景的验证通过 |
| 假绿 | 假阳性测试结果；不可靠的验证通过 |
| 永眠 | 永久不可达状态；不可恢复阻塞状态；静止但未完成的状态 |
| 防挂 | 避免 panic；限制故障传播；保证恢复流程继续推进 |
| 实锤 | 直接证据；可复现实验结果；已测量证据 |
| 盘面 | 磁盘布局；持久介质状态；块设备上的状态 |
| 红线 | 安全不变量；正确性约束；必须满足的设计条件 |
| 铁律 | 不变量；设计规则；必要的顺序约束 |
| 镜头 | 观察点；评估场景；分析视角 |
| 信封 | 包装对象；记录头；序列化记录的元数据头部 |
| 手术 | 局部重构；目标明确的改写；迁移步骤 |
| 双调 | 两阶段协议；双路径协调；成对状态转换 |
| 排干 | 排空脏数据；完成待处理写回；刷新未落盘更新 |
| 播种 | 初始化状态；构造测试数据；创建初始持久记录 |

When rewriting Chinese notes, preserve the user's intended idea but replace private shorthand with auditable wording.

Bad:

```text
这里先把读漏斗做出来，防止恢复时盲判绿。
```

Better:

```text
这里先实现恢复期的统一读取与校验路径，确保每个持久对象在重新引用前都经过格式检查和版本检查，避免在缺少证据的情况下将损坏状态判定为可恢复。
```

Bad:

```text
这个设计把写侧踩爆邻居的问题兜住。
```

Better:

```text
该设计将元数据持久化路径与数据块写回路径分离，降低无关数据块写回对元数据 durability latency 的影响。该结论目前属于设计意图，仍需要通过写回 workload 的延迟和吞吐测量验证。
```

## Code Identifier Discipline

Identifiers such as `Pod`, `parse-once`, `typestate`, `WriteAccess`, `Dirty<T>`, `MultiInodeInnerGuards`, `Unwritten-first`, and `B-1 stale-seed` are not explanations.

When using a code or design identifier, include four pieces of context:

1. Problem: what ambiguity, failure mode, or unsafe behavior it addresses.
2. Mechanism: what state, type, API, or protocol it introduces.
3. Enforcement: where the guarantee is checked or made impossible to violate.
4. Limit: what the mechanism does not guarantee.

Example:

```text
`Dirty<T>` represents a typed metadata object whose in-memory contents have diverged from the block-device snapshot. It addresses the need to track pending metadata writeback without losing the object's Rust type. The design relies on typed cache ownership and explicit writeback transitions; it does not by itself prove crash consistency unless the writeback ordering and persistence barriers are specified.
```

Chinese example:

```text
`Dirty<T>` 表示一个类型化元数据对象，其内存内容已经相对于块设备快照发生修改。它解决的问题是：在保持 Rust 类型信息的同时跟踪待写回的元数据状态。该机制依赖类型化缓存所有权和显式写回状态转换；如果没有进一步说明写回顺序和持久化屏障，它本身不构成崩溃一致性证明。
```

## Local Concept Discipline

Project-local concepts are allowed only when they compress a repeated, well-defined idea.
Before using a local concept such as "四铁律", "破坏模式降级", "性能税", or "设计纪律+崩溃harness", expand it into standard language.

Required form:

```text
Concept name: <short local name, optional>
Standard description: <one sentence using filesystem/OS terminology>
Purpose: <the design or analysis problem it solves>
Invariant or claim: <what must hold>
Evidence or validation: <test, proof obligation, measurement, or citation>
```

Do not keep the local name if the standard description is clear enough by itself.

## Filesystem and OS Vocabulary

Prefer terms from these families when they match the design:

- Persistence ordering: write ordering, cache-line writeback, persistence barrier, flush, fence, force unit access, ordering point, durability point.
- Crash consistency: failure model, crash state, recovery procedure, replay, idempotence, atomicity, prefix property, reachable state, orphaned object, leaked allocation.
- Filesystem metadata: inode, directory entry, extent tree, block bitmap, inode bitmap, journal descriptor, commit record, checkpoint, superblock.
- Writeback: dirty metadata, dirty data, writeback queue, flush boundary, block-device snapshot, delayed allocation, unwritten extent conversion.
- NVDIMM or persistent memory: persistent object, typed persistent object, persistent pointer, rehydration, reconstruction, durable allocation, media-resident state.
- Rust safety: ownership, borrowing, lifetime, aliasing, typestate, unsafe boundary, invariant, trait bound, construction protocol, destructor behavior.
- Evaluation: workload, metric, baseline, controlled variable, measured overhead, throughput, latency, bandwidth saturation, ablation, sensitivity analysis.

## Failure Model Rules

Every discussion of crash recovery must state the assumed failure model.

Use this form:

```text
Failure model: The system may crash after any persistence operation that has reached NVDIMM or the block device. DRAM state is lost. NVDIMM contents survive. The design does not model NVDIMM media failure, power-loss corruption, or SSD firmware reordering beyond the stated barrier semantics.
```

Adjust the sentence to match the actual assumption.
Do not use phrases such as "硬件绝对安全" without specifying what failures are excluded.

Chinese form:

```text
故障模型：系统可能在任意已经到达 NVDIMM 或块设备的持久化操作之后崩溃。DRAM 状态在崩溃后丢失，NVDIMM 内容保留。本文不建模 NVDIMM 介质损坏、电源故障导致的介质损坏，或超出已声明 barrier 语义之外的 SSD 固件重排序。
```

## Evidence and Claim Labels

Label claims when drafting or reviewing:

- Evidence: directly supported by a file, experiment, trace, code path, or cited paper.
- Inference: reasoned from evidence but not directly observed.
- Design intent: what the proposed mechanism is intended to guarantee.
- Open hypothesis: plausible but not yet measured or proved.

Example:

```text
Evidence: The writeback trace shows metadata writes waiting behind data flushes in workload X.
Inference: This suggests that metadata persistence ordering contributes to tail latency.
Design intent: Moving typed metadata objects to NVDIMM removes this block-device ordering point for metadata.
Open hypothesis: The remaining cost is dominated by data writeback rather than metadata synchronization.
```

Chinese example:

```text
证据：写回 trace 显示，在 workload X 中，元数据写入等待数据 flush 完成。
推断：这说明元数据持久化顺序可能是尾延迟来源之一。
设计意图：将类型化元数据对象放入 NVDIMM 后，元数据更新不再依赖块设备上的该顺序点。
开放假设：剩余开销是否主要来自数据写回，还需要通过实验验证。
```

## Rewrite Procedure

When reviewing Agent-generated text:

1. Identify informal metaphors, unexplained identifiers, and local concepts.
2. Replace each with standard filesystem, OS, Rust, or storage terminology.
3. Add first-use definitions for any remaining project-local terms.
4. Check that each mechanism has a problem, invariant, enforcement point, and limit.
5. Check that crash-consistency text states the failure model.
6. Check that evaluation claims name the metric, workload, baseline, and evidence status.

## Output Contract For Agents

When asked to write or revise research text, produce language that satisfies this contract:

- Clear subject: what component, data structure, or protocol is being discussed.
- Precise action: allocate, persist, flush, fence, recover, validate, reconstruct, or write back.
- Explicit object: inode, extent, bitmap, journal record, typed persistent object, or block-device snapshot.
- Explicit guarantee: safety invariant, ordering guarantee, type-safety property, or recovery property.
- Explicit evidence status: evidence, inference, design intent, or open hypothesis.

Bad:

```text
这个机制防止写侧踩爆邻居，是四铁律之一。
```

Better:

```text
This mechanism bounds writeback interference by separating metadata persistence from data-block writeback. The intended invariant is that metadata updates resident in NVDIMM do not require unrelated data blocks to be flushed before the metadata object becomes durable.
```

Chinese bad:

```text
这个机制防止写侧踩爆邻居，是四铁律之一。
```

Chinese better:

```text
该机制通过分离元数据持久化路径和数据块写回路径，降低无关对象之间的写回干扰。其设计不变量是：驻留在 NVDIMM 中的元数据更新不需要等待无关数据块完成块设备写回即可达到持久化状态。
```

## Quality Bar

A good Agent response for this project is:

- understandable to a filesystem or OS researcher without private context
- explicit about persistence and recovery assumptions
- precise about whether an object lives in DRAM, NVDIMM, or the block device
- careful about Rust type-safety claims and unsafe boundaries
- conservative about performance claims until measurements are cited
- free of unexplained metaphors, private shorthand, and slogan-like concepts
- equally precise in Chinese discussion and English research prose
