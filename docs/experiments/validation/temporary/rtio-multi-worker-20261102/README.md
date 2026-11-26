# rTIO Multi-worker Migration Validation (2026-11-02)

## Result

The multi-worker mechanism is functionally complete: rNVPCe workers claim
disjoint dirty extent entries, submit BIOs concurrently, share one flush per
ready durability epoch, and are quiesced by sync. rNVPC WAL remains
single-worker. However, four submitters regress every tested workload on the
current single-queue virtio-blk path. Therefore the production default remains
one worker; `rtio_workers=2..16` is retained as an experimental override.

## Configuration

- Source base: `6835a793af0dc8474125a74f7d6a485a2003e3a2`, plus the final default-policy
  and documentation change in this report's commit.
- Authorized target: `/dev/nvme1n1p1`, reformatted as 4 KiB no-journal EXT4
  before every point; every post-run `e2fsck -fn` completed without errors.
- VM: 64 vCPU, 8 GiB RAM; virtio-blk single queue, depth 1024.
- rNVPCe: 128 MiB pool, 64 MiB metadata reserve, 64 MiB data limit, fast
  simulated persistence, CPU isolation enabled.
- fio: buffered `psync`, depth 1 per thread, 2 GiB total. Random write was
  prepared with a direct sequential write. Each point is one run.
- Images: worker-1 SHA-256
  `d5a348b9d1b0a6b060a4d9e91fa73a9f192db4cfd0a177bebfe6bebfcc02d299`;
  worker-4 SHA-256
  `4276560b531221a1a637ad7e36c0e01c221add85589d8badc9a110bc3a492e50`.

## Release Performance

MiB/s; `including sync` measures 2 GiB from fio start through final sync.

| Workload | Jobs | 1 worker fio | 4 workers fio | 1 worker including sync | 4 workers including sync |
| --- | ---: | ---: | ---: | ---: | ---: |
| 4 KiB random write | 1 | 298.37 | 33.37 | 273.80 | 33.21 |
| 4 KiB random write | 16 | 307.83 | 225.01 | 283.26 | 210.92 |
| 128 KiB sequential write | 1 | 2602.29 | 1257.99 | 1402.74 | 989.37 |
| 128 KiB sequential write | 16 | 870.38 | 562.17 | 731.43 | 479.63 |

## Diagnostic Comparison

The diagnostic point is 128 KiB sequential first write, 16 jobs, 2 GiB.

| Metric | 1 worker | 4 workers | Interpretation |
| --- | ---: | ---: | --- |
| Peak active migration workers | 1 | 4 | Parallel admission is real |
| Productive migration batches | 533 | 1530 | Four workers fragment ready work into smaller rounds |
| Candidate-selection calls | 542 | 4884 | Empty/racing scans increase about 9x |
| Candidate-selection total | 7.44 ms | 117.12 ms | Index contention is measurable but not dominant |
| BIO submission total | 0.89 s | 4.74 s | Dominant regression: concurrent producers contend on one queue |
| Durability epochs / flushes | 487 | 207 | Epoch coalescing works; flush count decreases |
| Batches per epoch | 1.09 | 7.39 | Ready batches share a flush |
| Maximum in-flight pages | 10,368 | 16,384 | Parallelism increases queue pressure |
| Capacity-wait calls | 698 | 1261 | More foreground stalls |
| Capacity-wait cumulative time | 1.26 s | 18.16 s | Submission contention delays capacity release |

Evidence shows the regression is not caused by extra SSD flushes. It comes
from multiple migration producers competing in the single virtio submission
path and consuming dirty entries before they form large batches. A useful
multi-worker default needs either block multi-queue with worker-to-queue
affinity, or multiple scanners feeding one centralized BIO submitter.

## Correctness

- Focused worker policy, quiescence gate, disjoint claim, durability epoch,
  mixed successful/failed epoch isolation, diagnostic accounting, and the
  `ext4_research_routes` aggregate passed.
- The complete 412-test kernel run was not green. It reached the rNVPC suite
  and reported pre-existing standalone failures including
  `rnvpce_allocation_cache_recovers_selected_image`, slot-reuse generation,
  persistence-fence count, and PageCache pressure-accounting tests. The first
  listed test also fails when run alone, so it is not attributed to this
  multi-worker change. This report does not claim a clean full regression.

Raw release and diagnostic logs are under
`/home/zn/asterinas-build-env/rtio-multi-worker-20261102/` and are intentionally
not tracked by Git.
