# Nix initramfs 构建阻塞（真实 VM 实验不可用）核查文档

> **状态：已恢复（2026-09-23）**。§10 记录了实际跑通的完整构建与根因修正；
> §11 是修正后的行动项，**已替代 §5/§6/§7 的处方**；本文 §1/§2/§3.3/§4 的
> "不可用 / 闭包被 GC" 判断已被 §10 证伪，阅读时以 §10/§11 为准。

> 记录时间：2026-09-23（宿主时区 UTC+8；容器内为 UTC）
> 环境：宿主 `zn@r760-5th-1`，容器 `asterinas-dev`（`-w /root/asterinas` = 宿主 `/home/zn/Ant/Rust_EXT4/os_com`）
> 原文（已修正）的一句话结论：容器内可从 substituter 取包（**经 IPv6**）、initramfs 闭包并未被 GC 剪掉；当时的阻塞主因是 substituter 查询/TLS 失败与 IPv4 路径异常。**内核开发与 ktest 自始至终不受影响。**

---

## 1. 影响面

> 本表已按 §10 修正：下列通道现在**可用**（依赖 IPv6 出网，见 §11.2）。

| 通道 | 命令 | 现状 |
|---|---|---|
| 内核编译 | `cargo osdk check --package aster-kernel` | ✅ 正常（0 error / 0 warning） |
| 单测/全量回归 | `cargo osdk test [<EXACT_NAME>]` | ✅ 正常（**不经过** `test/initramfs`） |
| `make ktest` | 同上但走 make | ✅ 可用（会触发 nix-build，但相同派生已是 store 缓存，实测 ~1 s） |
| 真实 VM | `make kernel/run_kernel BENCHMARK=fio/<job>` | ✅ 已跑通（ISO 已生成，镜像内含 `benchmark/bin/fio`） |
| 依赖 VM 的实验 | 011 式带宽/延迟分解、fio 基线、P6 消融、rTIO 多 worker 吞吐/in-flight | ✅ 恢复 |
| 新增 guest 脚本/fio job | 任何改动落在 `test/initramfs/src/**` | ✅ 会触发 4 个派生的正常重建（约 1 分钟），不再阻塞 |
| **风险** | IPv4 下载路径 | ❌ `119.42.146.198:443` connection refused；宿主代理连不上。当前只能靠 IPv6，见 §11.2 |

---

## 2. 依赖链与触发点

1. initramfs **不是仓库文件**，是 Nix 派生（derivation）的产物：
   - `test/initramfs/Makefile` 的 `$(INITRAMFS_IMAGE)` 规则 = `nix-build … nix -A initramfs-image`；
   - 它的输入是**整个 `test/initramfs/src/**` 目录树**（busybox 脚本、fio、静态 `mke2fs`、guest 脚本…）+ `test/initramfs/nix/default.nix` 里 **pin 死的 nixpkgs**。
2. 外层 `Makefile`：`kernel` / `run_kernel` / `ktest` 都依赖 `initramfs`；而 `initramfs` 与 `$(INITRAMFS_IMAGE)` 都被声明为 `.PHONY`，所以**每次都会执行 nix-build**。
   - 证据：`make -n kernel BENCHMARK=fio/aster_ext4_baseline` 输出里含 `nix-build \`。
3. 产物软链接（`--out-link`，**普通符号链接，不是 gcroot**）：
   - `test/initramfs/build/initramfs` → `/nix/store/q05q5f7wrk4srwfk0wjnzp4bj9s1nahk-initramfs`（存在）
   - `test/initramfs/build/initramfs.cpio.gz` → `/nix/store/w05w41hw56h77vd9420c5wv1b6axchn3-initramfs-image`（存在，mtime 2026-09-23 01:50 UTC）
   - 旧的 `/nix/store/jmqgwviwzcl55mm269bd749wy0mkyr7n-initramfs-image`（Jun 17）**已缺失**

---

## 3. 现状证据（逐条可复现）

### 3.1 当前源树需要 198 个派生才能 realize

```bash
docker exec -w /root/asterinas/test/initramfs asterinas-dev bash -c '
nix-build --dry-run --tarball-ttl 2592000 \
  --argstr target x86_64 --arg enableBenchmarkTest true \
  --arg enableConformanceTest false --arg enableRegressionTest false \
  --argstr conformanceTestSuite ltp --argstr conformanceTestWorkDir /tmp \
  --argstr regressionTestPlatform asterinas --argstr dnsServer none \
  --arg initramfsCompressed false --arg smp 1 \
  --out-link /tmp/dryrun.cpio nix -A initramfs-image'
```

输出（节选）：

```
warning: unable to download 'https://cache.nixos.org/nix-cache-info': SSL peer certificate or SSH remote key was not OK (60)
         SSL: no alternative certificate subject name matches target hostname 'cache.nixos.org'; retrying … (attempt 1/5 … 5/5)
these 198 derivations will be built:
  /nix/store/0m4y3j4pnivlhhpr5yqdvlly86p93fwc-busybox.drv
  /nix/store/xjkydxc0n24mwxp8kh4wn5jq0fppga9k-bootstrap-tools.tar.xz.drv
  …
  /nix/store/l24i7dp3bv8k3qy54bhvv7rzljy0fjjy-sqlite-3.48.0.drv
  /nix/store/19ydajj45zp11ya4hz6slz6vzkmkaxxw-benchmark-0.1.0.drv
  /nix/store/j19zyadi6fy3qgs5fix8np8rlwh8lyh0-runtime-deps.drv
  /nix/store/984dk6i45mj9f7rdmlsxd758jd56vfly-initramfs.drv
  /nix/store/giswq7nkwa434h0wpwzyym7wzy8h2z8c-initramfs-image.drv
```

注意 `bootstrap-tools.tar.xz.drv` —— **固定输出派生（FOD）**，必须联网下载，无法离线构造。

### 3.2 网络/TLS 两条路都不通

```bash
# 直连
docker exec asterinas-dev bash -c 'curl -sSIL --max-time 8 https://cache.nixos.org/nix-cache-info'
#   → curl: (60) SSL certificate problem: certificate has expired

# 走宿主代理
docker exec asterinas-dev bash -c 'https_proxy=http://10.249.46.37:7890 curl -sSIL --max-time 8 https://cache.nixos.org/nix-cache-info'
#   → HTTP/1.1 200 Connection established
#   → curl: (35) Recv failure: Connection reset by peer
```

nix 自身的报错更明确：`SSL: no alternative certificate subject name matches target hostname 'cache.nixos.org'`
（典型的 **TLS 拦截 / 证书被替换或过期**：证书链不可信、且 CN/SAN 与目标主机名不匹配）。

### 3.3 store 里闭包不完整，且没有 gcroot 保护（**已被 §10.2/§10.3 修正**）

```bash
docker exec asterinas-dev bash -c 'ls /nix/store | wc -l'                    # 2513
docker exec asterinas-dev bash -c 'ls -l /nix/var/nix/gcroots/auto | head'   # 只有 Jun 18 的 x86_64-pkgs-*
```

- `build/initramfs*` 是 `nix-build --out-link` 产生的普通软链，**不在 `/nix/var/nix/gcroots`**；
- 因此一次 `nix-collect-garbage` 就会把整条闭包收走（已验证旧的 `jmqgw…-initramfs-image` 缺失）；
- store 里虽然还能看到 `busybox-1.37.0`、`fio-3.39` 之类的条目，但**当前树评估出的那套派生**（cross 到 `x86_64-unknown-linux-gnu`）输出不齐，所以 dry-run 仍报"198 个待构建"。

### 3.4 与本轮新增的 benchmark job 无关

把 `test/initramfs/src/benchmark/fio/dio_coherence/` 移走后重跑 3.1 的 dry-run，**仍是 "these 198 derivations will be built"** ⇒ 阻塞源于环境（闭包/网络），不是新增文件。

---

## 4. 根因（三条，缺一不可）（**已被 §10.5 修正**：未发生闭包 GC；主因是 substituter 查询/TLS 失败 + IPv4 路径异常）

1. **替换源不可达**：`cache.nixos.org` 的 TLS 握手校验失败（证书过期/主机名不匹配），容器内无可用代理。
2. **闭包被剪**：`build/initramfs*` 未注册 gcroot，GC 已把大部分依赖输出清掉（198 个待构建即是证据）。
3. **闭包含 FOD**：`bootstrap-tools.tar.xz`、若干源码/补丁 tarball 必须下载，无法在离线环境"算出来"。

⇒ 结论：**只要 1 或 2 之一不解决，initramfs 就构建不出来**；而 3 决定了"完全离线"这条路必须靠**离线导入**（见方案 B）。

---

## 5. 需要核查的点（建议顺序）（**已被 §11.2 取代**）

1. 容器出网策略：`cache.nixos.org:443` 是否被拦？是否是 MITM 代理（证书签发者是谁）？
   ```bash
   docker exec asterinas-dev bash -c 'openssl s_client -connect cache.nixos.org:443 -servername cache.nixos.org </dev/null 2>&1 | head -20'
   ```
2. 宿主代理 `http://10.249.46.37:7890` 是否能访问 HTTPS 站点（本轮表现为 CONNECT 后立刻被 reset，需要确认代理是否只放白名单）。
3. GitHub 可达性（pin 的 nixpkgs tarball）：
   ```bash
   docker exec asterinas-dev bash -c 'curl -sSIL --max-time 15 https://github.com/NixOS/nixpkgs/archive/c0bebd16e69e631ac6e52d6eb439daba28ac50cd.tar.gz'
   ```
4. 若确有内部镜像/代理 CA：拿到 CA 证书后给 nix 指定
   `NIX_SSL_CERT_FILE=/path/ca.crt`（或 `--option ssl-cert-file /path/ca.crt`）。

命令入口（全部在容器内执行）：

```bash
docker exec asterinas-dev bash -c '<cmd>'
docker exec -w /root/asterinas/test/initramfs asterinas-dev bash -c '<cmd>'
```

---

## 6. 修复方案（**已被 §11 取代**；A/B/C 仅作“再次断网”时的备选）

### A. 修网络（最省事，推荐先试）
让容器能访问 substituter（直连或可用代理 + 正确 CA），然后一次构建补齐：

```bash
# 1) 先 dry-run 看规模
docker exec -w /root/asterinas/test/initramfs asterinas-dev bash -c '
nix-build --dry-run --tarball-ttl 2592000 --argstr target x86_64 \
  --arg enableBenchmarkTest true --arg enableConformanceTest false \
  --arg enableRegressionTest false --argstr conformanceTestSuite ltp \
  --argstr conformanceTestWorkDir /tmp --argstr regressionTestPlatform asterinas \
  --argstr dnsServer none --arg initramfsCompressed true --arg smp 1 \
  --out-link build/initramfs.cpio.gz nix -A initramfs-image'

# 2) 去掉 --dry-run 真下（建议 timeout 3600）
#    或等价地走 Makefile（注意 BENCHMARK 必须显式给，否则 fio 不打包）：
docker exec -w /root/asterinas asterinas-dev bash -c 'make initramfs BENCHMARK=fio/ext4_journal_compare'
```

### B. 离线导入（最可控）
在有网机器上用**同一个 nixpkgs rev** 评估后导出整条闭包：

```bash
# 有网机器（同一 commit 的工作区）
nix-store --export $(nix-store -qR /nix/store/<initramfs-image 的路径>) > /tmp/initramfs-closure.nar
# 容器
docker cp /tmp/initramfs-closure.nar asterinas-dev:/tmp/
docker exec asterinas-dev bash -c 'nix-store --import < /tmp/initramfs-closure.nar'
```
注意：还要把 pin 的 `c0bebd1…tar.gz` 一并导入（否则评估阶段就会重新下载）。

### C. 防复发
```bash
docker exec -w /root/asterinas/test/initramfs asterinas-dev bash -c 'make x86_64_pkgs'   # 写入 /nix/var/nix/gcroots/auto/x86_64-pkgs-*
# 或对最终产物建 gcroot
docker exec asterinas-dev bash -c 'nix-store --add-root /nix/var/nix/gcroots/auto/initramfs-image --indirect -r $(readlink -f /root/asterinas/test/initramfs/build/initramfs.cpio.gz)'
```

---

## 7. 恢复后的验收清单

```bash
# 1) 产物指向新 store 路径，且 benchmark/bin 里有 fio
docker exec asterinas-dev bash -c 'ls -l /root/asterinas/test/initramfs/build/initramfs*'
docker exec asterinas-dev bash -c 'ls $(readlink -f /root/asterinas/test/initramfs/build/initramfs)/benchmark/bin | head'

# 2) 既有 job 能跑
docker exec -w /root/asterinas asterinas-dev bash -c 'make run_kernel BENCHMARK=fio/aster_ext4_baseline'

# 3) 端到端验证 51e60ae20（O_DIRECT 一致性；job 目前仍是未跟踪文件）
docker exec -w /root/asterinas asterinas-dev bash -c 'make run_kernel BENCHMARK=fio/dio_coherence'
```

---

## 8. 附录：确切参数与环境事实

- 容器：`asterinas-dev`；工作目录 `/root/asterinas`（= 宿主 `/home/zn/Ant/Rust_EXT4/os_com`）。
- pin 的 nixpkgs（`test/initramfs/nix/default.nix`）：
  `https://github.com/NixOS/nixpkgs/archive/c0bebd16e69e631ac6e52d6eb439daba28ac50cd.tar.gz`
  `sha256 = 1fbhkqm8cnsxszw4d4g0402vwsi75yazxkpfx3rdvln4n6s68saf`
  （注释标注：nix 2.29.1 / channel nixos-25.05 / release 2025-07-01）
- `crossSystem.config = x86_64-unknown-linux-gnu`（target=x86_64）。
- `NIXPKGS_CACHE_TTL = 2592000`（30 天，`--tarball-ttl`）。
- 宿主代理环境变量：`http(s)_proxy = http://10.249.46.37:7890`（本轮实测：CONNECT 成功后 reset）。
- 关键时间戳：`build/initramfs*` 软链 mtime = 2026-09-23 01:50 UTC（= 09:50 宿主），早于新增 job 文件 `dio_coherence/run.sh`（11:29 宿主）。
- 本轮 dry-run 使用的完整参数见 §3.1（`enableBenchmarkTest true`、`initramfsCompressed false`；harness 走的是 `true`，即 `.gz`）。

---

## 9. 附注：同一天发现的两个 ktest 问题（与本网络问题无关，单独记录）

全量 ktest（720 个）本次跑到第 337 个卡住，现场日志已保存：
`results/20260923_ktest_full_hang/qemu-serial-full-720.log`

1. **2 个此前从未跑到过的 DIO 测试 FAILED**：
   `write_tests::direct_and_buffered_interleaved_round_trip`、
   `write_tests::direct_write_invalidates_stale_cached_page`
   （来自更早的提交 `f58d16f87`，不是本轮的 B1 修复；此前全量跑在更早的 `direct_write_fills_hole` 就卡死，从未执行到它们）。
2. **第 337 个测试卡死**：`write_tests::dirty_watermark_throttles_foreground_write`，串口被写回 daemon 的
   `ERROR: ext4: [WBIDLE] dirty=115 dirty_inodes=0 high=51 low=25 bg=25` 刷屏（236 行）。
   初步判断：全局脏页计数为 115 但 dirty inode 集合为空 ⇒ 后台 daemon 无活可干却持续被唤醒，前台写被水线节流后无人解除。
   可能与上一条"失败的 DIO 测试留下孤立脏页（其 inode 已不在 dirty 集合）"有关，待单独定位。

---

## 10. 2026-09-23 后续复核：完整构建结果与根因修正

### 10.1 完整构建已实际跑通

在 `asterinas-dev` 容器的 `/root/asterinas` 中执行：

```bash
make initramfs BENCHMARK=fio/dio_coherence
make kernel BENCHMARK=fio/dio_coherence
```

两条命令均以退出码 `0` 完成。首次 initramfs 构建实际执行了以下工作：

- 从 `https://cache.nixos.org` 下载 `sqlite-3.48.0-bin`；
- 构建 `benchmark-0.1.0`、`runtime-deps`、`initramfs` 和 `initramfs-image`；
- 生成新的 initramfs 和可启动 ISO。

生成结果：

```text
test/initramfs/build/initramfs
  -> /nix/store/ig7r6i0xsp3xshadh0r7hw6nanwrw9xb-initramfs

test/initramfs/build/initramfs.cpio.gz
  -> /nix/store/g1y5mf8ikxr6hycy6yibsj20g68hsp63-initramfs-image

target/osdk/aster-kernel-osdk-bin.iso
  188 MiB, bootable ISO 9660 image
```

initramfs 镜像大小约为 36 MiB，解压前原始 cpio 大小约为 100 MiB。镜像内容中已确认存在：

```text
benchmark/bin/fio
benchmark/fio/dio_coherence/run.sh
```

紧接着重复执行同一条 `make initramfs` 命令仅耗时约 1 秒，没有重新构建 derivation，证明 Nix store 缓存能够正常复用。

构建日志保存在容器中：

```text
/tmp/initramfs-full-build.log
/tmp/initramfs-repeat-build.log
/tmp/kernel-full-build.log
```

内核构建日志中没有 `error` 或 `failed`。`grub-mkrescue` 报告缺少 `/usr/local/grub/share/locale`，但这是非致命警告，ISO 随后成功生成。

### 10.2 `--out-link` 与 GC root 的原结论不正确

后续通过 `nix-store -q --roots` 和 `/nix/var/nix/gcroots/auto` 核查确认，`nix-build --out-link` 创建的工作区符号链接已经被 Nix 注册为间接 GC root：

```text
/root/asterinas/test/initramfs/build/initramfs
  -> /nix/store/ig7r6i0xsp3xshadh0r7hw6nanwrw9xb-initramfs

/root/asterinas/test/initramfs/build/initramfs.cpio.gz
  -> /nix/store/g1y5mf8ikxr6hycy6yibsj20g68hsp63-initramfs-image
```

因此，本文 §2.3、§3.3 和 §4 中“普通 `--out-link` 不是 gcroot”“当前闭包因没有 gcroot 被 GC 剪掉”的判断不成立。旧 store 路径消失只能证明旧产物后来不再被任何活动 root 引用，不能证明当前 `build/initramfs*` 没有 GC root 保护。

`.PHONY` 会使 Make 每次调用 `nix-build`，但不会强制 Nix 重新构建已存在于 store 中的相同 derivation。重复构建约 1 秒完成也验证了这一点。

### 10.3 “198 个 derivation”不是闭包被 GC 的证据

网络恢复后，以 §3.1 的同类参数重新执行 dry-run，只显示 4 个本地 derivation 和 1 个可从 substituter 下载的路径，而不是 198 个 derivation：

```text
these 4 derivations will be built:
  benchmark-0.1.0
  runtime-deps
  initramfs
  initramfs-image

this path will be fetched:
  sqlite-3.48.0-bin
```

这 4 个 derivation 是新增 `test/initramfs/src/benchmark/fio/dio_coherence/` 改变输入后的正常重建。先前的 198 个 derivation 主要是 substituter 查询失败时，Nix 无法确认可下载的二进制缓存，因而将依赖显示为需要本地构建；该数量本身不能证明 store 闭包已被 GC。

### 10.4 当前仍存在 IPv4 网络故障

容器采用 `network=host`，与宿主共用 systemd-resolved。宿主当前使用的 DNS 上游为：

```text
DNS server: 10.248.98.30
interface:  br0
```

该 DNS 对 `cache.nixos.org` 返回：

```text
IPv4: 119.42.146.198
IPv6: 2a04:4e42:5c::347
CNAME: dualstack.n.sni.global.fastly.net
```

分别强制地址族测试的结果不同：

```text
IPv6:
  TLS certificate CN/SAN = cache.nixos.org
  certificate verification = OK
  HTTP status = 200

IPv4:
  connect 119.42.146.198:443
  Connection refused
  curl exit code = 7
```

宿主配置的代理 `http://10.249.46.37:7890` 在本次复核时也无法建立连接，因此不能作为可靠的备用路径。

当前构建成功是因为 IPv6 路径可用。IPv4 路径仍然异常；如果 IPv6 临时不可用或 DNS 返回发生变化，Nix 下载仍可能再次失败。需要从宿主网络侧继续检查内部 DNS `10.248.98.30` 返回的 IPv4 CDN 地址及其路由/服务状态。

### 10.5 修正后的结论

本次阻塞不是 initramfs 源码、Nix derivation 或缺少 GC root 导致的。现有证据支持以下结论：

1. 先前确实发生过 substituter TLS 证书过期或主机名不匹配，但该状态现在无法复现；当时的证书链和 DNS 回包未保存，因此不能进一步确定具体的错误节点。
2. 当前 IPv6 下载路径正常，完整 initramfs 和 kernel 构建均已跑通。
3. 当前 IPv4 下载路径持续失败，这是能够复现的遗留网络问题，也是该故障可能再次出现的风险点。
4. `build/initramfs*` 已被 Nix 自动注册为间接 GC root，原文关于当前闭包无 GC root 保护的判断应作废。

建议后续再次出现 TLS 问题时立即同时保存以下现场信息，以区分 DNS、IPv4/IPv6 路由和 TLS 拦截问题：

```bash
date -u
resolvectl query cache.nixos.org
curl -4 -v --connect-timeout 10 https://cache.nixos.org/nix-cache-info
curl -6 -v --connect-timeout 10 https://cache.nixos.org/nix-cache-info
openssl s_client -connect cache.nixos.org:443 -servername cache.nixos.org </dev/null
```

---

## 11. 行动项（2026-09-23 修正后）

### 11.1 已具备的能力：这些验证现在可以真做

- `make initramfs BENCHMARK=fio/<job>`、`make kernel/run_kernel BENCHMARK=fio/<job>` 均已跑通（§10.1）；
  镜像内已确认有 `benchmark/bin/fio` 与 `benchmark/fio/dio_coherence/run.sh`。
- **立刻可做的端到端验证**（`51e60ae20` 的 O_DIRECT 一致性，此前只能用 ktest 侧证明）：
  正确调用方式见 §11.4（必须挂盘），实测结果（两个变体均 PASS）：
  ```text
  === DIO_COHERENCE: PASS (journal_mode=nojournal, size=4M) ===
  === DIO_COHERENCE: PASS (journal_mode=journal,   size=4M) ===
  coherence-verify: err= 0 … issued rwts: total=1024,0,0,0 short=0,0,0,0
  ```
  判定标记打在 **`/root/asterinas/qemu.log`**（不是 `qemu-serial.log`）；job 跑完后 guest 不退出，
  由外层 `timeout` 收尾，所以 `EXIT=124` 属正常。
- 随后可跑既有基线（**每个配置 ≥3 次**，nj16 orderless 有 3–10× 双稳）：
  ```bash
  docker exec -w /root/asterinas asterinas-dev bash -c 'make run_kernel BENCHMARK=fio/aster_ext4_baseline'
  ```

### 11.2 必须加固：目前构建全程靠 IPv6

成功构建依赖 **IPv6 可用**（IPv4 的 `119.42.146.198:443` connection refused，宿主代理
`10.249.46.37:7890` 也连不上）。任一项变化就会复现今早的阻塞。按侵入度排序：

1. **让 glibc 优先 IPv6**（推荐，非硬编码）：`/etc/gai.conf` 里若存在
   `precedence ::ffff:0:0/96  100`（Debian 默认附加行，它把 IPv4 排在前面），注释掉即可让
   解析优先选可用的 IPv6 路径。
2. **临时锉死地址**（仅救急；CDN 会换地址，用完就要删）：
   ```bash
   docker exec asterinas-dev bash -c 'echo "2a04:4e42:5c::347 cache.nixos.org" >> /etc/hosts'
   ```
3. **报给网络侧**：内部 DNS（`10.248.98.30`，br0）返回的 IPv4 CDN 地址连不上，需确认该地址
   是否已被 CDN 下线 / 出网策略是否封了该段 443。
4. **失败现场立即留证**：下次出问题时按 §10.5 的四条命令同时保存（`date -u` / `resolvectl` /
   `curl -4 -v` / `curl -6 -v` / `openssl s_client`），再回来对照本文。

### 11.3 不再需要做的事

- 不需要给 `build/initramfs*` 手工建 gcroot：`nix-build --out-link` 已被 Nix 注册为间接 GC root（§10.2）。
- 不需要离线导入清单：substituter 可用（§10.3）。
- 不需要因为“`.PHONY` 会重建”而担心：相同派生命中 store 缓存，实测重复 `make initramfs` 约 1 s（§10.2）。

### 11.4 VM job 的正确调用方式（2026-09-23 实测）

`make run_kernel BENCHMARK=fio/<job>` **必须带上挂盘环境变量**，否则 QEMU 不挂 benchmark 盘、
guest 会报 `The file /dev/vdc does not exist and no size was specified`（内核与 initramfs 都正常，
只是没有数据盘）。harness（`test/aster-ext4-baseline-test/run-baseline.sh:328`）的约定是：

```bash
XFSTESTS_TEST_IMAGE=<raw 镜像路径> XFSTESTS_TEST_CACHE=none XFSTESTS_TEST_AIO=threads \
ATTACH_XFSTESTS_IMAGES=true NETDEV=none \
make run_kernel BENCHMARK=fio/<job> BENCHMARK_ARGS="<job 自己的参数>" [RELEASE=1 SMP=1 MEM=...]
```

其中 `ATTACH_XFSTESTS_IMAGES=true` 才让它挂成 `/dev/vdc`；`BENCHMARK_ARGS` 会经
`--init-args="/benchmark/common/bench_runner.sh fio/<job> asterinas <ARGS>"` 传给 guest 的
`run.sh`。可直接照抄的一次实测（O_DIRECT 一致性 job，参数为 `nojournal 4`）：

```bash
docker exec -w /root/asterinas asterinas-dev bash -c '
truncate -s 1G test/initramfs/build/dio_coherence.img
XFSTESTS_TEST_IMAGE=/root/asterinas/test/initramfs/build/dio_coherence.img \
XFSTESTS_TEST_CACHE=none XFSTESTS_TEST_AIO=threads \
ATTACH_XFSTESTS_IMAGES=true NETDEV=none \
timeout 1200 make run_kernel BENCHMARK=fio/dio_coherence BENCHMARK_ARGS="nojournal 4"'
# guest 判定看串口：`=== DIO_COHERENCE: PASS|FAIL (journal_mode=.., size=..) ===`
```

### 11.5 与实现计划的关系

- 因 VM 阻塞而搁置的部分现在恢复：**P6 消融**、以及任何依赖真实设备队列的实验。
  R5 仍按既定决定取消（其“深度是否有收益”的疑问改由 P5 保留“inode 内合并大 I/O”可选项覆盖）。
- 主线顺序不变：**P1 ∥ P2 → P3 → P4 → P5 → P6**；P1 已在实现中（ktest 验收）。
- P1 的 ns 级计时默认编译期关闭（`WAL_LATENCY_ENABLED=false`）：ktest 下 TSC 未标定会让
  `read_monotonic_time()` 直接 panic；要在 VM 上采集延迟时再开启并确认时钟已标定。

---

## 12. 2026-09-23 晚：同一故障复发——这次用「禁用 substituter」绕开

### 12.1 现场（按 §10.5 采集，宿主与容器结果一致）

```text
date -u                              → Wed 23 Sep 2026 01:30:32 PM UTC
resolvectl query cache.nixos.org     → 2406:cb42:0:2018::2 (br0), 8.134.128.179
                                       （注意：与 §10.4 记录的地址都不同，DNS 会变）
curl -4 …                            → Recv failure: Connection reset by peer
curl -6 …                            → SSL certificate problem: certificate has expired
openssl s_client（宿主与容器 모두） →
    issuer = C = US, O = Let's Encrypt, CN = YE1
    notAfter = Sep 20 23:13:27 2026 GMT
    Verify return code: 10 (certificate has expired)
```

**根因**：网络侧 **TLS 拦截**（中间证书名 `YE1` 非真实 Let's Encrypt 中间名），且该证书
已于 2026-09-20 过期。宿主与容器**都**命中同一拦截 ⇒ 与本仓库配置无关。

### 12.2 为什么这次比 §10 那次更麻烦

§10.4 的成功依赖 IPv6 可用；现在 IPv6 也被拦截（过期证书），IPv4 被 reset，宿主代理
`10.249.46.37:7890` CONNECT 后 reset ⇒ **三条路都不通**，没有任何可用的 substituter。

### 12.3 绕行：`substitute = false`（已验证可完整离线构建）

关键判断：**这次要构建的派生，其全部输入都已经在 `/nix/store` 里**——nix 只是在查询
"这次要产出的 initramfs 是否已有人做好"（narinfo），该查询因 TLS 报错被当成**致命错误**。

```bash
# 1) 先确认无需下载：只列本地派生、且没有 "will be fetched"
docker exec -w /root/asterinas/test/initramfs asterinas-dev bash -c '
  nix-build --dry-run --option substitute false --tarball-ttl 2592000 \
    --argstr target x86_64 --arg enableBenchmarkTest true --arg enableConformanceTest false \
    --arg enableRegressionTest false --argstr conformanceTestSuite ltp \
    --argstr conformanceTestWorkDir /tmp --argstr regressionTestPlatform asterinas \
    --argstr dnsServer none --arg initramfsCompressed true --arg smp 1 \
    --out-link /tmp/dry.cpio nix -A initramfs-image'

# 2) 真构建（去掉 --dry-run）
```

实测输出：`these 4 derivations will be built:`（`benchmark-0.1.0` / `runtime-deps` /
`initramfs` / `initramfs-image`）→ 全部本地构建成功，产物
`/nix/store/adasd2qjk42gbfxkrhzb6nkx94rkv65g-initramfs-image`，退出码 0。

`NIX_CONFIG="substitute = false"` 等效（Nix 2.34.7 实测可用），差不需要改 Makefile。

### 12.4 为什么"每次改 guest 脚本都要重建"

harness 的 `install_job`（`run-baseline.sh:258`）把
`os_com_implementation/test/aster-ext4-baseline-test/aster_ext4_baseline.sh`
拷进 `os_com/test/initramfs/src/benchmark/fio/aster_ext4_baseline/run.sh`。
该文件是 initramfs 派生输入 ⇒ **改 guest 脚本就会触发这 4 个派生重建**（内容不变时
派生哈希不变，实测约 1 s 空跑）。

### 12.5 harness 已加的透传（本次）

`run-baseline.sh` 在 `docker run` 时按 `-e` 透传宿主 `NIX_CONFIG`（宿主未设则不传，
保持原行为）。用法：

```bash
NIX_CONFIG="substitute = false" bash run-baseline.sh --confirm-destroy --fs <…> …
```

### 12.6 仍属遗留风险

IPv4/IPv6/代理三条路径均不可用，**当前完全依赖"store 里已有全部输入"**。一旦某个依赖
不在 store（换 nixpkgs、加新依赖、store 被 GC），这套绕行就不再成立，必须等网络侧修好
（§11.2.3）或走 §6-B 的离线导入。
