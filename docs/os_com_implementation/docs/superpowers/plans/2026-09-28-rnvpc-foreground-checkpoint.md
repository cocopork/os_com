# rNVPC Foreground WAL Checkpoint Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace rNVPC's DRAM-only, SSD-completion-retired WAL with a persistent per-inode WAL and B-link B+tree whose full-log checkpoint is executed synchronously by the application thread.

**Architecture:** Every rNVPC inode owns a double-imaged anchor, sixteen persistent WAL frames holding 1024 fixed-size records, and a persistent B-link B+tree. A write persists its NVPage, appends and commits one WAL record, advances the anchor, then publishes the DRAM read index; the write needing record 1025 replays all 1024 records into the tree under the inode mutation lock and atomically advances the checkpoint boundary. rTIO only drains checkpointed NVPages to SSD and never advances or retires the WAL.

**Tech Stack:** Rust `no_std`, Asterinas kernel/ktest, EXT4, DRAM-backed NVPool model, virtio-blk QEMU validation, existing `NvMemoryBackend`/`PersistEngine` persistence primitives.

**Spec:** `rNVPC-rTIO/rnvpc-foreground-checkpoint-design.md`

## Global Constraints

- rNVPC is the Corundum-style comparison object; rNVPCe remains WAL-free and its soft-update path must not change.
- WAL capacity is exactly 1024 records per inode; record 1025 checkpoints the full WAL in the calling application thread.
- Replay records strictly by increasing LSN, including repeated updates to the same page; do not coalesce.
- Checkpoint performs no SSD I/O and does not wait for rTIO.
- SSD BIO completion changes only writeback/reclamation state; it never advances WAL head, releases WAL capacity, or defines checkpoint completion.
- Persistent tree updates are local; never flatten or rebuild the complete tree and never copy the complete root-to-leaf path.
- Persistent payloads contain only fixed-width scalar fields and raw NV frame/page identities; never persist Rust pointers, locks, `Arc`, or runtime counters.
- Use existing dependencies only; do not add a crate.
- Detailed latency timing remains compile-time disabled for normal throughput runs.
- Crash tests use `DramNvImage::simulate_crash()` and must be described as modeled persistence, not VM power-loss proof.
- Before implementation, preserve the user's uncommitted `fs.rs`, `extend_cow.rs`, and benchmark changes. Create the implementation worktree/branch from a reviewed commit containing any required deferred-completion fixes; never overwrite those files from `HEAD` blindly.

---

## File Structure

### New source files in `../os_com/kernel/src/fs/rnvpc/`

- `pcache_layout.rs`: byte-exact anchor, WAL-record, WAL-frame, and dual-image B+tree-node layouts; checksums and encode/decode validation.
- `pcache_store.rs`: typed persistent-metadata frame allocation, generation-checked local-image updates, and fault-injection boundaries built on `Rnvpc`.
- `persistent_btree.rs`: per-inode B-link B+tree lookup, local upsert, split, root publication, validation, and replay counters.

### Modified source files

- `mod.rs`: register new modules, key rNVPC contexts by `(fs_id, inode)`, remove WAL-full rTIO pressure, expose recovery catalog and new statistics.
- `wal.rs`: replace `InodeWalRuntime`'s DRAM ring with persistent anchor/WAL runtime and foreground checkpoint state.
- `cow.rs`: enforce NVPage -> WAL -> anchor -> DRAM publication order; invoke foreground checkpoint; track `wal_pinned` pages.
- `frame.rs`: expose generation-checked typed metadata access and hold the recovered persistent-page-cache catalog.
- `capacity.rs`: remove synthetic WAL-page charging; real anchor/WAL/tree frames count as metadata capacity.
- `recover.rs`: classify typed metadata payloads, rebuild inode catalogs, replay committed records, and retain reachability information for orphan collection.
- `reclaim.rs`: protect objects reachable from anchors, B+tree nodes, or committed WAL records.
- `tests.rs`: layout, persistence-ordering, checkpoint, split, recovery, concurrency, rTIO-boundary, and orphan tests.
- `../os_com/kernel/src/fs/fs_impls/ext4/inode/extent_manager/mod.rs`: pass `rnvpc_fs_id` into rNVPC contexts on read and write paths.
- `../os_com/kernel/src/fs/fs_impls/ext4/fs.rs`: consume the revised rTIO candidate/completion contract and print the revised counters; preserve existing deferred completion queue behavior.

### Modified documentation after verification

- `rNVPC-rTIO/implementation.md`: replace the old completion-retired WAL description with implemented behavior and verification evidence.
- `STATUS.md`: mark foreground checkpoint tasks complete only after ktest and QEMU smoke pass.

---

### Task 1: Freeze the Baseline and Add Persistent Payload Layouts

**Files:**
- Create: `../os_com/kernel/src/fs/rnvpc/pcache_layout.rs`
- Modify: `../os_com/kernel/src/fs/rnvpc/mod.rs`
- Modify: `../os_com/kernel/src/fs/rnvpc/tests.rs`

**Interfaces:**
- Consumes: `PAGE_SIZE`, raw `NvFrameId::index()` and `NvPageId::index()` values.
- Produces: `PcObjectType`, `PersistentPageRef`, `InodePcAnchor`, `PersistentWalRecord`, `WalFrameImage`, `BtreeNodeFrame`, `BtreeNodeImage`, `LeafEntry`, `InternalEntry`, and validated `encode_page`/`decode_page` methods.

- [ ] **Step 1: Isolate the implementation branch without losing current work**

Run:

```bash
cd /home/zn/Ant/Rust_EXT4/os_com
git status --short
git diff -- kernel/src/fs/fs_impls/ext4/fs.rs kernel/src/fs/rnvpc/extend_cow.rs
git worktree add ../os_com-rnvpc-foreground-checkpoint -b rnvpc-foreground-checkpoint HEAD
```

Expected: the original worktree remains dirty and unchanged; the new worktree is clean. If either dirty source file contains a required fix, commit that fix on its owning branch first and recreate the worktree from that commit rather than copying an unreviewed diff.

- [ ] **Step 2: Write failing layout validation tests**

Add ktests that exercise real page-sized encodings:

```rust
#[ktest]
fn rnvpc_pc_layout_rejects_torn_and_wrong_inode_images() {
    let anchor = InodePcAnchor::empty(7, 11, PersistentPageRef::new(3, 9));
    let mut bytes = [0u8; PAGE_SIZE];
    anchor.encode_page(&mut bytes).unwrap();
    assert_eq!(InodePcAnchor::decode_page(&bytes, 7, 11).unwrap(), anchor);

    bytes[96] ^= 0x80;
    assert_eq!(
        InodePcAnchor::decode_page(&bytes, 7, 11).unwrap_err().error(),
        Errno::EINVAL
    );
}

#[ktest]
fn rnvpc_pc_wal_layout_holds_exactly_1024_records() {
    assert_eq!(WAL_RECORDS_PER_FRAME, 64);
    assert_eq!(WAL_FRAME_COUNT, 16);
    assert_eq!(WAL_RECORDS_PER_FRAME * WAL_FRAME_COUNT, 1024);
}
```

- [ ] **Step 3: Run the ktest compile gate and confirm RED**

Run:

```bash
cd /home/zn/Ant/Rust_EXT4/os_com-rnvpc-foreground-checkpoint/kernel
cargo osdk check --ktests --package aster-core
```

Expected: compilation fails because the persistent layout types and constants do not exist.

- [ ] **Step 4: Implement fixed-width layouts and checksum validation**

Use explicit disk identities and no Rust enum representation in persisted bytes:

```rust
pub(crate) const WAL_CAPACITY_PER_INODE: usize = 1024;
pub(crate) const WAL_OP_UPSERT: u16 = 1;
pub(crate) const WAL_RECORD_BYTES: usize = 64;
pub(crate) const WAL_RECORDS_PER_FRAME: usize = PAGE_SIZE / WAL_RECORD_BYTES;
pub(crate) const WAL_FRAME_COUNT: usize =
    WAL_CAPACITY_PER_INODE.div_ceil(WAL_RECORDS_PER_FRAME);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistentPageRef {
    pub(crate) slot: u32,
    pub(crate) generation: u64,
}

impl PersistentPageRef {
    pub(crate) const NONE: Self = Self { slot: u32::MAX, generation: 0 };
    pub(crate) const fn new(slot: u32, generation: u64) -> Self {
        Self { slot, generation }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub(crate) enum PcObjectType {
    Anchor = 1,
    WalFrame = 2,
    BtreeNode = 3,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistentWalRecord {
    pub(crate) lsn: u64,
    pub(crate) operation: u16,
    pub(crate) reserved: u16,
    pub(crate) page_index: u64,
    pub(crate) old_page: PersistentPageRef,
    pub(crate) new_page: PersistentPageRef,
    pub(crate) ssd_backing_block: u64,
    pub(crate) checksum: u32,
    pub(crate) commit_word: u64,
}

pub(crate) trait PcPageCodec: Sized {
    const OBJECT_TYPE: PcObjectType;
    fn encode_page(&self, output: &mut [u8; PAGE_SIZE]) -> Result<()>;
    fn decode_page(input: &[u8; PAGE_SIZE]) -> Result<Self>;
}
```

Implement explicit little-endian field encoding and CRC32C over the encoded bytes with the checksum field zeroed. Define constants for every field offset and assert encoded lengths at compile time. A WAL record is valid only when magic/version, checksum, and `commit_word == wal_commit_word(lsn)` all match.

For anchors and tree nodes, store two complete images plus a stable header and one `u64` selector word. Decode the selected valid image; if it is torn, choose the other valid image with the highest generation.

- [ ] **Step 5: Run formatting and ktest compile gate**

Run:

```bash
cd /home/zn/Ant/Rust_EXT4/os_com-rnvpc-foreground-checkpoint
cargo fmt --all -- --check
cd kernel && cargo osdk check --ktests --package aster-core
```

Expected: both commands pass; corruption tests compile against concrete layouts.

- [ ] **Step 6: Commit the layout contract**

```bash
git add kernel/src/fs/rnvpc/pcache_layout.rs kernel/src/fs/rnvpc/mod.rs kernel/src/fs/rnvpc/tests.rs
git commit -m "feat(rnvpc): define persistent page-cache layouts"
```

---

### Task 2: Add Typed Persistent Metadata Storage

**Files:**
- Create: `../os_com/kernel/src/fs/rnvpc/pcache_store.rs`
- Modify: `../os_com/kernel/src/fs/rnvpc/frame.rs`
- Modify: `../os_com/kernel/src/fs/rnvpc/capacity.rs`
- Modify: `../os_com/kernel/src/fs/rnvpc/mod.rs`
- Modify: `../os_com/kernel/src/fs/rnvpc/tests.rs`

**Interfaces:**
- Consumes: Task 1 payload encoders and existing `Rnvpc::{alloc_frame_seq,pin_frame,replace_metadata_frame,frame_seq}`.
- Produces: `Rnvpc::begin_metadata_update`, `PersistentFrameUpdate`, `PersistentMetadataStore::{allocate,read,update,validate_ref}`, and `PcFaultPoint` persistence cut points.

- [ ] **Step 1: Write failing typed-store persistence tests**

```rust
#[ktest]
fn rnvpc_pc_store_recovers_only_committed_typed_frames() {
    let (image, config, pool) = restartable_pool(40);
    let store = PersistentMetadataStore::new(pool.clone());
    let anchor = InodePcAnchor::empty(5, 17, PersistentPageRef::NONE);
    let anchor_ref = store.allocate(5, 17, PcObjectType::Anchor, 0, &anchor, None).unwrap();
    drop(store);
    drop(pool);

    image.simulate_crash();
    let recovered = Arc::new(Rnvpc::recover(Arc::new(image.attach()), config, None).unwrap());
    let store = PersistentMetadataStore::new(recovered);
    assert_eq!(store.read::<InodePcAnchor>(anchor_ref).unwrap(), anchor);
}
```

```rust
#[ktest]
fn rnvpc_pc_store_does_not_recover_uncommitted_typed_frame() {
    let (image, config, pool) = restartable_pool(40);
    let store = PersistentMetadataStore::new(pool.clone());
    let key = PageKey::new(5, 18, typed_object_index(PcObjectType::Anchor, 0));
    let anchor = InodePcAnchor::empty(5, 18, PersistentPageRef::NONE);
    assert!(store
        .allocate(
            5,
            18,
            PcObjectType::Anchor,
            0,
            &anchor,
            Some(PcFaultPoint::BeforeDescriptorCommit),
        )
        .is_err());
    drop(store);
    drop(pool);

    image.simulate_crash();
    let recovered = Rnvpc::recover(Arc::new(image.attach()), config, None).unwrap();
    assert!(recovered.committed_metadata_slot(key).is_none());
}
```

- [ ] **Step 2: Run the ktest compile gate and confirm RED**

Run `cargo osdk check --ktests --package aster-core` from `kernel/`.

Expected: missing `PersistentMetadataStore` and `PcFaultPoint` symbols.

- [ ] **Step 3: Implement the typed store over existing metadata frames**

```rust
pub(crate) struct PersistentMetadataStore {
    pool: Arc<Rnvpc>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PersistPlan {
    pub(crate) image_range: core::ops::Range<usize>,
    pub(crate) selector_range: core::ops::Range<usize>,
}

impl PersistentMetadataStore {
    pub(crate) fn new(pool: Arc<Rnvpc>) -> Self;

    pub(crate) fn allocate<T: PcPageCodec>(
        &self,
        fs_id: u64,
        inode: u64,
        object_type: PcObjectType,
        object_index: u64,
        value: &T,
        fault: Option<PcFaultPoint>,
    ) -> Result<PersistentPageRef>;

    pub(crate) fn read<T: PcPageCodec>(&self, page: PersistentPageRef) -> Result<T>;

    pub(crate) fn update<T: PcPageCodec>(
        &self,
        page: PersistentPageRef,
        fault: Option<PcFaultPoint>,
        mutate: impl FnOnce(&mut T) -> Result<PersistPlan>,
    ) -> Result<()>;

    pub(crate) fn validate_ref(&self, page: PersistentPageRef) -> Result<NvFrameId>;
}
```

`allocate` must allocate a real `PoolClass::Metadata` frame, write a full encoded page, then commit an `Nvbh` descriptor whose `PageKey` is `(fs_id, inode, typed_object_index(object_type, object_index))`. Do not use `PinnedFrame::write` or `replace_metadata_frame` for local-image updates: the former rejects committed frames and the latter changes physical identity.

Add a generation-checked update handle in `frame.rs`:

```rust
pub(crate) struct PersistentFrameUpdate {
    pool: Arc<Rnvpc>,
    id: NvFrameId,
    active: bool,
}

impl Rnvpc {
    pub(crate) fn begin_metadata_update(
        self: &Arc<Self>,
        page: PersistentPageRef,
    ) -> Result<PersistentFrameUpdate>;
}

impl PersistentFrameUpdate {
    pub(crate) fn read(&self, output: &mut [u8; PAGE_SIZE]) -> Result<()>;
    pub(crate) fn write_page(&self, input: &[u8; PAGE_SIZE]) -> Result<()>;
    pub(crate) fn persist_range(&self, offset: usize, len: usize) -> Result<()>;
    pub(crate) fn fence(&self) -> Result<()>;
    pub(crate) fn finish(mut self);
}
```

`begin_metadata_update` verifies `ObjectKind::Metadata`, `committed == true`, `persisting == false`, and allocation sequence equality before setting `persisting` and pinning the frame. `write_page` writes backend working bytes even though the descriptor is already committed; durability changes only through explicit `persist_range` and `fence`. `Drop` clears `persisting` and the pin on every error path. `PersistentMetadataStore::update` reads and decodes the page, applies `mutate`, writes the page with the inactive image changed, persists/fences the image range, writes the selector into a second full-page working copy, persists/fences only the selector range, and then calls `finish`.

- [ ] **Step 4: Remove synthetic WAL capacity charging**

Delete `charge_wal_pages`, `release_wal_pages`, and the `PoolClass::Wal` allocation path. Anchor, WAL, and tree pages are real metadata frames and therefore already consume capacity exactly once. Preserve a diagnostic `wal_pages` count by deriving it from the number of registered `WalFrame` payloads rather than decrementing `free_pages` again.

- [ ] **Step 5: Run layout/store tests and static checks**

Run:

```bash
cargo fmt --all -- --check
cd kernel && cargo osdk check --package aster-core
cargo osdk check --ktests --package aster-core
```

Expected: all pass and the restart test reads only durable bytes.

- [ ] **Step 6: Commit typed metadata persistence**

```bash
git add kernel/src/fs/rnvpc/{pcache_store.rs,frame.rs,capacity.rs,mod.rs,tests.rs}
git commit -m "feat(rnvpc): persist typed page-cache metadata"
```

---

### Task 3: Replace the DRAM WAL with a Persistent Per-Inode WAL

**Files:**
- Modify: `../os_com/kernel/src/fs/rnvpc/wal.rs`
- Modify: `../os_com/kernel/src/fs/rnvpc/mod.rs`
- Modify: `../os_com/kernel/src/fs/rnvpc/tests.rs`

**Interfaces:**
- Consumes: `PersistentMetadataStore`, `InodePcAnchor`, `PersistentWalRecord`, `WAL_FRAME_COUNT`.
- Produces: `WalUpsert`, `PersistentInodeWal::{create,recover,append,records_after,publish_checkpoint,pending,is_full}`, and `WalAppendOutcome`.

- [ ] **Step 1: Replace old WAL tests with persistent-ring tests**

```rust
fn test_wal_upsert(page_index: u64) -> WalUpsert {
    WalUpsert {
        page_index,
        old_page: PersistentPageRef::NONE,
        new_page: PersistentPageRef::new((page_index + 1) as u32, page_index + 10),
        ssd_backing_block: page_index + 100,
    }
}

#[ktest]
fn rnvpc_persistent_wal_accepts_1024_and_rejects_1025_without_checkpoint() {
    let (_image, _config, pool) = restartable_pool(80);
    let store = PersistentMetadataStore::new(pool.clone());
    let root = store
        .allocate(3, 9, PcObjectType::BtreeNode, 0, &BtreeNodeFrame::empty_leaf(3, 9, 0), None)
        .unwrap();
    let wal = PersistentInodeWal::create(pool, 3, 9, root, 0).unwrap();
    for page_index in 0..WAL_CAPACITY_PER_INODE as u64 {
        wal.append(test_wal_upsert(page_index), None).unwrap();
    }
    assert_eq!(wal.pending(), 1024);
    assert_eq!(wal.append(test_wal_upsert(1024), None).unwrap_err().error(), Errno::ENOSPC);
}

#[ktest]
fn rnvpc_persistent_wal_recovery_ignores_uncommitted_tail_record() {
    let (image, config, pool) = restartable_pool(80);
    let store = PersistentMetadataStore::new(pool.clone());
    let root = store
        .allocate(3, 9, PcObjectType::BtreeNode, 0, &BtreeNodeFrame::empty_leaf(3, 9, 0), None)
        .unwrap();
    let wal = PersistentInodeWal::create(pool.clone(), 3, 9, root, 0).unwrap();
    wal.append(test_wal_upsert(1), None).unwrap();
    assert!(wal
        .append(test_wal_upsert(2), Some(PcFaultPoint::AfterWalRecordPersist))
        .is_err());
    let anchor_ref = wal.anchor_ref();
    drop(wal);
    drop(store);
    drop(pool);

    image.simulate_crash();
    let pool = Arc::new(Rnvpc::recover(Arc::new(image.attach()), config, None).unwrap());
    let anchor = PersistentMetadataStore::new(pool.clone())
        .read::<InodePcAnchor>(anchor_ref)
        .unwrap();
    let wal = PersistentInodeWal::recover(pool, anchor_ref, anchor).unwrap();
    let records = wal.records_after(0).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].lsn, 1);
}
```

- [ ] **Step 2: Run the ktest compile gate and confirm RED**

Run `cargo osdk check --ktests --package aster-core`.

Expected: old `InodeWalRuntime` API no longer satisfies the tests.

- [ ] **Step 3: Implement WAL creation and durable append ordering**

```rust
pub(crate) struct PersistentInodeWal {
    fs_id: u64,
    inode: u64,
    store: PersistentMetadataStore,
    anchor: PersistentPageRef,
    wal_frames: [PersistentPageRef; WAL_FRAME_COUNT],
    state: Mutex<WalRuntimeState>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WalUpsert {
    pub(crate) page_index: u64,
    pub(crate) old_page: PersistentPageRef,
    pub(crate) new_page: PersistentPageRef,
    pub(crate) ssd_backing_block: u64,
}

impl PersistentInodeWal {
    pub(crate) fn create(
        pool: Arc<Rnvpc>,
        fs_id: u64,
        inode: u64,
        root: PersistentPageRef,
        tree_height: u16,
    ) -> Result<Arc<Self>>;
    pub(crate) fn recover(
        pool: Arc<Rnvpc>,
        anchor: PersistentPageRef,
        anchor_image: InodePcAnchor,
    ) -> Result<Arc<Self>>;
    pub(crate) fn append(
        &self,
        upsert: WalUpsert,
        fault: Option<PcFaultPoint>,
    ) -> Result<WalAppendOutcome>;
    pub(crate) fn records_after(&self, checkpointed_lsn: u64) -> Result<Vec<PersistentWalRecord>>;
    pub(crate) fn publish_checkpoint(
        &self,
        checkpointed_lsn: u64,
        fault: Option<PcFaultPoint>,
    ) -> Result<()>;
    pub(crate) fn anchor_ref(&self) -> PersistentPageRef;
    pub(crate) fn pending(&self) -> usize;
    pub(crate) fn is_full(&self) -> bool;
}
```

The caller supplies an already-persisted empty root. Create the remaining objects in this exact order: sixteen zeroed WAL frames, then an anchor that names the supplied root and WAL geometry. Append one record in this order: write record with zero commit word, persist record cache line/page, fence, write commit word, persist it, fence, then update and select the inactive anchor image containing the new tail/used/next LSN. Update in-memory head/tail only after anchor selection succeeds.

- [ ] **Step 4: Implement durable checkpoint-boundary publication**

`publish_checkpoint(lsn)` writes an inactive anchor image with `checkpointed_lsn = lsn`, `wal_head = wal_tail`, and `wal_used = 0`; it persists and fences that image before switching/persisting/fencing the anchor selector. Only then update DRAM state. Any error leaves the old anchor selected and all records discoverable.

- [ ] **Step 5: Verify crash boundaries**

Add a table-driven test over:

```rust
[
    PcFaultPoint::AfterWalRecordPersist,
    PcFaultPoint::AfterWalCommitPersist,
    PcFaultPoint::AfterWalAnchorImagePersist,
    PcFaultPoint::AfterWalAnchorSelectorPersist,
]
```

For each point, call `simulate_crash()`, recover the WAL, and assert the recovered set equals the records whose anchor-visible commit completed.

- [ ] **Step 6: Run static checks and commit**

```bash
cargo fmt --all -- --check
cd kernel && cargo osdk check --ktests --package aster-core
git add src/fs/rnvpc/{wal.rs,mod.rs,tests.rs}
git commit -m "feat(rnvpc): add persistent per-inode WAL"
```

---

### Task 4: Implement Persistent B-link B+tree Lookup and Local Upsert

**Files:**
- Create: `../os_com/kernel/src/fs/rnvpc/persistent_btree.rs`
- Modify: `../os_com/kernel/src/fs/rnvpc/mod.rs`
- Modify: `../os_com/kernel/src/fs/rnvpc/tests.rs`

**Interfaces:**
- Consumes: `PersistentMetadataStore`, `BtreeNodeFrame`, `PersistentWalRecord`.
- Produces: `PersistentBtree::{create,recover,lookup,replay_upsert,validate}` and `BtreeReplayStats`.

- [ ] **Step 1: Write failing lookup, repeated-key, and idempotency tests**

```rust
fn test_persistent_record(lsn: u64, page_index: u64, slot: u32) -> PersistentWalRecord {
    PersistentWalRecord {
        lsn,
        operation: WAL_OP_UPSERT,
        reserved: 0,
        page_index,
        old_page: PersistentPageRef::NONE,
        new_page: PersistentPageRef::new(slot, lsn + 100),
        ssd_backing_block: page_index + 1000,
        checksum: 0,
        commit_word: wal_commit_word(lsn),
    }
}

#[ktest]
fn rnvpc_btree_replays_repeated_keys_in_lsn_order() {
    let (_image, _config, pool) = restartable_pool(96);
    let tree = PersistentBtree::create(pool, 8, 13).unwrap();
    tree.replay_upsert(&test_persistent_record(1, 7, 21), None).unwrap();
    tree.replay_upsert(&test_persistent_record(2, 7, 22), None).unwrap();
    tree.replay_upsert(&test_persistent_record(3, 7, 23), None).unwrap();
    assert_eq!(tree.lookup(7).unwrap().unwrap().page.slot, 23);
    assert_eq!(tree.stats().records_replayed, 3);

    tree.replay_upsert(&test_persistent_record(3, 7, 23), None).unwrap();
    assert_eq!(tree.stats().records_replayed, 3);
}
```

- [ ] **Step 2: Run the ktest compile gate and confirm RED**

Run `cargo osdk check --ktests --package aster-core`.

Expected: `PersistentBtree` does not exist.

- [ ] **Step 3: Implement single-node lookup and inactive-image updates**

```rust
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistentMapping {
    pub(crate) page: PersistentPageRef,
    pub(crate) ssd_backing_block: u64,
    pub(crate) lsn: u64,
}

pub(crate) struct PersistentBtree {
    fs_id: u64,
    inode: u64,
    store: PersistentMetadataStore,
    state: Mutex<BtreeRuntimeState>,
    counters: Mutex<BtreeReplayStats>,
}

impl PersistentBtree {
    pub(crate) fn create(pool: Arc<Rnvpc>, fs_id: u64, inode: u64) -> Result<Arc<Self>>;
    pub(crate) fn recover(
        pool: Arc<Rnvpc>,
        fs_id: u64,
        inode: u64,
        root: PersistentPageRef,
        height: u16,
    ) -> Result<Arc<Self>>;
    pub(crate) fn root_ref(&self) -> PersistentPageRef;
    pub(crate) fn height(&self) -> u16;
    pub(crate) fn lookup(&self, page_index: u64) -> Result<Option<PersistentMapping>>;
    pub(crate) fn replay_upsert(
        &self,
        record: &PersistentWalRecord,
        fault: Option<PcFaultPoint>,
    ) -> Result<()>;
    pub(crate) fn validate(&self) -> Result<()>;
}
```

Search reads the selected valid node image and follows `right_sibling` while `key > high_key`. A leaf upsert rewrites only that leaf's inactive image, sets `page_lsn = record.lsn`, persists it, then switches the selector. If `page_lsn >= record.lsn`, return success without modifying the node or counters.

- [ ] **Step 4: Test local-image crash recovery**

Inject failure after inactive image persistence and after selector persistence. After `simulate_crash()`, `recover()` must select either the old complete image or the new complete image, never a mixed entry set.

- [ ] **Step 5: Run checks and commit**

```bash
cargo fmt --all -- --check
cd kernel && cargo osdk check --ktests --package aster-core
git add src/fs/rnvpc/{persistent_btree.rs,mod.rs,tests.rs}
git commit -m "feat(rnvpc): add persistent B-link tree upsert"
```

---

### Task 5: Add B-link Node Splits and Atomic Root Publication

**Files:**
- Modify: `../os_com/kernel/src/fs/rnvpc/persistent_btree.rs`
- Modify: `../os_com/kernel/src/fs/rnvpc/wal.rs`
- Modify: `../os_com/kernel/src/fs/rnvpc/tests.rs`

**Interfaces:**
- Consumes: Task 4 `PersistentBtree::replay_upsert`, Task 3 anchor rewrite.
- Produces: split-capable `replay_upsert`, `RootUpdate`, and `PersistentInodeWal::publish_root`.

- [ ] **Step 1: Write failing leaf, internal, and root split tests**

```rust
#[ktest]
fn rnvpc_blink_split_keeps_right_node_searchable_before_parent_update() {
    let (_image, _config, pool) = restartable_pool(192);
    let tree = PersistentBtree::create(pool, 1, 44).unwrap();
    for key in 0..=(LEAF_ENTRY_CAPACITY as u64) {
        let fault = (key == LEAF_ENTRY_CAPACITY as u64)
            .then_some(PcFaultPoint::AfterLeftSiblingLinkPersist);
        let _ = tree.replay_upsert(&test_persistent_record(key + 1, key, key as u32 + 10), fault);
    }
    assert_eq!(tree.lookup(LEAF_ENTRY_CAPACITY as u64).unwrap().unwrap().page.slot,
               LEAF_ENTRY_CAPACITY as u32 + 10);
}
```

```rust
#[ktest]
fn rnvpc_blink_splits_internal_nodes_and_publishes_new_root() {
    let (image, config, pool) = restartable_pool(4096);
    let tree = PersistentBtree::create(pool.clone(), 1, 45).unwrap();
    let count = LEAF_ENTRY_CAPACITY * (INTERNAL_ENTRY_CAPACITY + 1) + 1;
    for key in 0..count as u64 {
        tree.replay_upsert(&test_persistent_record(key + 1, key, key as u32 + 20), None)
            .unwrap();
    }
    let root = tree.root_ref();
    let height = tree.height();
    assert!(height >= 2);
    tree.validate().unwrap();
    drop(tree);
    drop(pool);

    image.simulate_crash();
    let pool = Arc::new(Rnvpc::recover(Arc::new(image.attach()), config, None).unwrap());
    let tree = PersistentBtree::recover(pool, 1, 45, root, height).unwrap();
    tree.validate().unwrap();
    assert!(tree.lookup((count - 1) as u64).unwrap().is_some());
}
```

- [ ] **Step 2: Run the ktest gate and confirm RED**

Run `cargo osdk check --ktests --package aster-core`.

Expected: insertion returns `ENOSPC` at leaf capacity or the split fault point is absent.

- [ ] **Step 3: Implement split order and right-link traversal**

```rust
struct SplitResult {
    separator: u64,
    right: PersistentPageRef,
}

fn split_leaf(
    &self,
    left: PersistentPageRef,
    image: BtreeNodeImage,
    record_lsn: u64,
    fault: Option<PcFaultPoint>,
) -> Result<SplitResult>;

fn insert_separator(
    &self,
    path: &mut Vec<PathEntry>,
    separator: u64,
    right: PersistentPageRef,
    record_lsn: u64,
    fault: Option<PcFaultPoint>,
) -> Result<Option<RootUpdate>>;
```

Persist in this exact order: initialize and commit right node; rewrite left high key/right sibling; rewrite parent separator; recursively split parent. Search must already reach the right node after step two, so a missing parent separator is repairable by replay.

- [ ] **Step 4: Implement root publication through the anchor**

```rust
pub(crate) struct RootUpdate {
    pub(crate) root: PersistentPageRef,
    pub(crate) height: u16,
}

pub(crate) fn publish_root(
    &self,
    update: RootUpdate,
    fault: Option<PcFaultPoint>,
) -> Result<()>;
```

Create and persist the new root before storing it in the inactive anchor image. Switch the anchor selector only after the new root validates. Tree replay returns `RootUpdate`; the owning inode runtime publishes it before considering that replayed record durable in the tree.

- [ ] **Step 5: Run split fault matrix**

Cover:

```rust
[
    PcFaultPoint::AfterRightNodePersist,
    PcFaultPoint::AfterLeftSiblingLinkPersist,
    PcFaultPoint::AfterParentSeparatorPersist,
    PcFaultPoint::AfterNewRootPersist,
    PcFaultPoint::AfterRootAnchorPersist,
]
```

For every point, crash, recover, replay the source WAL record, call `validate()`, and assert all inserted keys map to their last LSN.

- [ ] **Step 6: Run checks and commit**

```bash
cargo fmt --all -- --check
cd kernel && cargo osdk check --ktests --package aster-core
git add src/fs/rnvpc/{persistent_btree.rs,wal.rs,tests.rs}
git commit -m "feat(rnvpc): add crash-recoverable B-link splits"
```

---

### Task 6: Integrate Foreground Checkpoint into the rNVPC Write Path

**Files:**
- Modify: `../os_com/kernel/src/fs/rnvpc/cow.rs`
- Modify: `../os_com/kernel/src/fs/rnvpc/wal.rs`
- Modify: `../os_com/kernel/src/fs/rnvpc/mod.rs`
- Modify: `../os_com/kernel/src/fs/fs_impls/ext4/inode/mod.rs`
- Modify: `../os_com/kernel/src/fs/fs_impls/ext4/inode/extent_manager/mod.rs`
- Modify: `../os_com/kernel/src/fs/rnvpc/tests.rs`

**Interfaces:**
- Consumes: persistent WAL and tree from Tasks 3-5.
- Produces: `InodeCowContext::{create,recover,checkpoint_if_full,update}`, keyed context APIs `cow_context(fs_id,inode)` and `existing_cow_context(fs_id,inode)`.
- Test-only API: `InodeCowContext::set_checkpoint_fault_for_test(Option<PcFaultPoint>)`; production builds contain no mutable global fault switch.

- [ ] **Step 1: Write failing record-1025 foreground-checkpoint tests**

```rust
#[ktest]
fn rnvpc_record_1025_checkpoints_in_the_calling_update() {
    let (_image, _config, pool) = restartable_pool(1200);
    let context = InodeCowContext::create(pool, 6, 19).unwrap();
    for page_index in 0..1024 {
        context.update(page_index, page_index + 100, |page| page[0] = 0x41).unwrap();
    }
    let before = context.counters();
    context.update(1024, 1124, |page| page[0] = 0x42).unwrap();
    let after = context.counters();
    assert_eq!(after.foreground_checkpoints, before.foreground_checkpoints + 1);
    assert_eq!(after.records_replayed - before.records_replayed, 1024);
    assert_eq!(context.wal().pending(), 1);
}
```

```rust
#[ktest]
fn rnvpc_checkpoint_failure_keeps_the_full_wal_visible() {
    let (_image, _config, pool) = restartable_pool(1200);
    let context = InodeCowContext::create(pool, 6, 20).unwrap();
    for page_index in 0..1024 {
        context.update(page_index, page_index + 200, |page| page[0] = 0x51).unwrap();
    }
    let before = context.wal().state_snapshot();
    context.set_checkpoint_fault_for_test(Some(PcFaultPoint::BeforeCheckpointAnchorAdvance));
    assert_eq!(
        context.update(1024, 1224, |page| page[0] = 0x52).unwrap_err().error(),
        Errno::EIO
    );
    assert_eq!(context.wal().state_snapshot(), before);
    assert_eq!(context.wal().pending(), 1024);
}
```

- [ ] **Step 2: Run the ktest gate and confirm RED**

Run `cargo osdk check --ktests --package aster-core`.

Expected: `InodeCowContext::create`, checkpoint counters, and the new `update` signature are absent.

- [ ] **Step 3: Replace writeback waiting with foreground checkpoint**

```rust
impl InodeCowContext {
    pub(crate) fn create(pool: Arc<Rnvpc>, fs_id: u64, inode: u64) -> Result<Arc<Self>>;

    pub(crate) fn recover(
        pool: Arc<Rnvpc>,
        fs_id: u64,
        inode: u64,
        wal: Arc<PersistentInodeWal>,
        tree: Arc<PersistentBtree>,
    ) -> Result<Arc<Self>>;

    pub(crate) fn checkpoint_if_full(&self) -> Result<bool>;

    pub(crate) fn update(
        &self,
        page_index: u64,
        ssd_backing_block: u64,
        apply: impl FnOnce(&mut [u8]),
    ) -> Result<UpdateOutcome>;
}
```

`InodeCowContext::create` first creates and persists `PersistentBtree`, then passes `tree.root_ref()` and `tree.height()` to `PersistentInodeWal::create`, which publishes the anchor last. Hold `mutation_lock` across `checkpoint_if_full` and the complete update. Checkpoint reads committed records after `checkpointed_lsn`, calls `tree.replay_upsert` for every record in order, publishes any root update, persists all node changes, advances the anchor checkpoint boundary, then unpins pages and queues replaced versions for reclamation.

- [ ] **Step 4: Enforce write persistence order**

Refactor `update` to execute:

```rust
self.checkpoint_if_full()?;
let old = self.current_mapping(page_index)?;
let new = self.persist_new_nvpage(page_index, apply)?;
let appended = self.wal.append(
    WalUpsert {
        page_index,
        old_page: old.unwrap_or(PersistentPageRef::NONE),
        new_page: new,
        ssd_backing_block,
    },
    None,
)?;
self.publish_dram_mapping(page_index, new, appended.lsn, ssd_backing_block);
```

If NVPage persistence fails, no WAL record is visible. If WAL append/anchor publication fails, free the unreferenced new NVPage and leave the DRAM mapping unchanged. Do not publish the DRAM mapping before durable WAL exposure.

- [ ] **Step 5: Move rNVPC publication to the foreground inode write path**

Add a dedicated helper distinct from the existing rNVPCe helper:

```rust
fn persist_written_pages_rnvpc(
    &self,
    fs: &Ext4,
    offset: usize,
    written: usize,
) -> Result<()>;
```

Call it in `Inode::write_at` after `write_at_once` releases `inner` and before returning `Ok(written)`. It covers every touched page, not only fully overwritten pages: read the resulting full page from PageCache, query the extent map, encode an unavailable SSD target as `u64::MAX`, and call `InodeCowContext::update`. Capacity pressure checkpoints/reclaims and retries in this foreground call; unlike the current rNVPCe best-effort helper, a persistence failure is returned to the writer because rNVPC success means NVPage plus WAL are durable.

Change `ExtentManager::submit_write_bio` for rNVPC so an already-published page only calls:

```rust
context.mark_writeback_target(idx as u64, pblock)?;
complete_fn(BioStatus::Complete);
return Ok(());
```

Use the existing full-page retirement there only as a fallback for dirty pages created outside `write(2)`. `mark_writeback_target` updates DRAM state; if the mapping is already checkpointed it persists the target in that leaf using a local dual-image update. If it is still WAL-pinned, checkpoint overlays the latest DRAM target while replaying. After modeled crash recovery, mappings carrying `u64::MAX` remain ineligible for rTIO until EXT4 extent lookup rebinds their target. This preserves delayed allocation without allowing SSD completion to retire WAL records.

- [ ] **Step 6: Key all rNVPC contexts by filesystem and inode**

Change:

```rust
static COW_CONTEXTS:
    Once<SpinLock<BTreeMap<(u64, u64), Arc<InodeCowContext>>>> = Once::new();

pub(crate) fn cow_context(fs_id: u64, inode: u64) -> Result<Arc<InodeCowContext>>;
pub(crate) fn existing_cow_context(fs_id: u64, inode: u64) -> Option<Arc<InodeCowContext>>;
```

Update EXT4 read/write call sites to obtain `fs.rnvpc_fs_id()` and pass it with `self.rnvpc_ino`. Do not change `extend_context`; rNVPCe behavior remains independent.

- [ ] **Step 7: Add same-inode serialization and cross-inode concurrency tests**

Use two kernel threads and barriers. Same-inode updates must report a maximum checkpoint concurrency of one. Two distinct `(fs_id, inode)` contexts must both enter checkpoint before either barrier is released, proving no global checkpoint lock was introduced.

- [ ] **Step 8: Run checks and commit**

```bash
cargo fmt --all -- --check
cd kernel && cargo osdk check --package aster-core
cargo osdk check --ktests --package aster-core
git add src/fs/rnvpc/{cow.rs,wal.rs,mod.rs,tests.rs} \
        src/fs/fs_impls/ext4/inode/mod.rs \
        src/fs/fs_impls/ext4/inode/extent_manager/mod.rs
git commit -m "feat(rnvpc): checkpoint full WAL in foreground"
```

---

### Task 7: Decouple rTIO and SSD Completion from WAL Retirement

**Files:**
- Modify: `../os_com/kernel/src/fs/rnvpc/cow.rs`
- Modify: `../os_com/kernel/src/fs/rnvpc/mod.rs`
- Modify: `../os_com/kernel/src/fs/fs_impls/ext4/fs.rs`
- Modify: `../os_com/kernel/src/fs/rnvpc/tests.rs`

**Interfaces:**
- Consumes: `CowEntry.wal_lsn` and anchor `checkpointed_lsn`.
- Produces: `CowEntry::wal_pinned`, checkpoint-based eligibility, WAL-neutral `complete_flush`.
- Test-only APIs: `InodeCowContext::force_checkpoint_for_test() -> Result<()>` and `PersistentInodeWal::state_snapshot() -> WalStateSnapshot`; neither is compiled into normal kernels.

- [ ] **Step 1: Write failing rTIO-boundary tests**

```rust
#[ktest]
fn rnvpc_rtio_skips_wal_pinned_pages_until_checkpoint() {
    let (_image, _config, pool) = restartable_pool(1200);
    let context = InodeCowContext::create(pool, 2, 8).unwrap();
    let outcome = context.update(0, 700, |page| page[0] = 0x55).unwrap();
    assert!(context.flush_candidates(8).is_empty());

    context.force_checkpoint_for_test().unwrap();
    assert_eq!(context.flush_candidates(8)[0].1, outcome.new_page);
}

#[ktest]
fn rnvpc_ssd_completion_does_not_advance_wal() {
    let (_image, _config, pool) = restartable_pool(1200);
    let context = InodeCowContext::create(pool, 2, 9).unwrap();
    context.update(0, 701, |page| page[0] = 0x66).unwrap();
    context.force_checkpoint_for_test().unwrap();
    let candidate = context.flush_candidates(1)[0];
    let before = context.wal().state_snapshot();
    context.complete_flush(candidate.0, candidate.1, candidate.3).unwrap();
    assert_eq!(context.wal().state_snapshot(), before);
}
```

- [ ] **Step 2: Run the ktest gate and confirm RED**

Run `cargo osdk check --ktests --package aster-core`.

Expected: current candidates include uncheckpointed pages and completion retires WAL records.

- [ ] **Step 3: Add checkpoint-based eligibility**

Extend `CowEntry`:

```rust
struct CowEntry {
    page: NvPageId,
    generation: u64,
    ssd_target: u64,
    dirty: bool,
    wal_lsn: u64,
    wal_pinned: bool,
}
```

`flush_candidates` filters `entry.dirty && !entry.wal_pinned`. After checkpoint anchor publication, clear `wal_pinned` only for entries with `wal_lsn <= checkpointed_lsn`; then notify rTIO if capacity is above its watermark.

- [ ] **Step 4: Remove WAL pressure and retirement from rTIO**

Change pressure to:

```rust
pub(crate) fn has_flush_pressure() -> bool {
    needs_flush_watermark() || extend_has_flushable_page() || cow_has_flushable_page()
}
```

Delete `wal_full_inodes`, `request_writeback_and_wait`, and all `wal.complete_page` calls. Change the global completion API to `complete_flush(fs_id: u64, inode: u64, page_index: u64, flushed_page: NvPageId, flushed_generation: u64) -> Result<()>` so it resolves the `(fs_id, inode)` context unambiguously. `complete_flush` may remove the exact flushed DRAM entry and reclaim the checkpointed NVPage, but it must not inspect or mutate WAL head/tail/used/checkpointed LSN.

- [ ] **Step 5: Preserve asynchronous completion behavior in `fs.rs`**

Keep the existing completion queue and in-flight accounting. The callback enqueues `(fs_id, inode, page_index, page, generation, status)`; process context calls the revised completion API. Verify no callback acquires `mutation_lock` while holding a page/frame/device lock.

- [ ] **Step 6: Run checks and commit**

```bash
cargo fmt --all -- --check
cd kernel && cargo osdk check --ktests --package aster-core
git add src/fs/rnvpc/{cow.rs,mod.rs,tests.rs} src/fs/fs_impls/ext4/fs.rs
git commit -m "fix(rnvpc): decouple rTIO completion from WAL"
```

---

### Task 8: Recover Anchors, Trees, WALs, and Reachable NVPages

**Files:**
- Modify: `../os_com/kernel/src/fs/rnvpc/frame.rs`
- Modify: `../os_com/kernel/src/fs/rnvpc/recover.rs`
- Modify: `../os_com/kernel/src/fs/rnvpc/reclaim.rs`
- Modify: `../os_com/kernel/src/fs/rnvpc/mod.rs`
- Modify: `../os_com/kernel/src/fs/rnvpc/cow.rs`
- Modify: `../os_com/kernel/src/fs/rnvpc/tests.rs`

**Interfaces:**
- Consumes: typed payload tags, persistent WAL/tree recovery APIs.
- Produces: `RecoveredPcacheCatalog`, `RecoveredInodePcache`, `Rnvpc::take_recovered_pcache`, and startup replay before foreground access.

- [ ] **Step 1: Write failing end-to-end modeled-crash recovery tests**

```rust
#[ktest]
fn rnvpc_recovery_replays_committed_wal_and_rebuilds_dram_index() {
    let (image, config, pool) = restartable_pool(128);
    let context = InodeCowContext::create(pool.clone(), 4, 31).unwrap();
    context.update(9, 900, |page| page[0] = 0x91).unwrap();
    context.update(9, 900, |page| page[0] = 0x92).unwrap();
    drop(context);
    drop(pool);

    image.simulate_crash();
    let recovered = Arc::new(Rnvpc::recover(Arc::new(image.attach()), config, None).unwrap());
    let context = InodeCowContext::recover_from_catalog(recovered, 4, 31).unwrap();
    let mut page = [0u8; PAGE_SIZE];
    assert!(context.read_page(9, &mut page).unwrap());
    assert_eq!(page[0], 0x92);
    assert_eq!(context.wal().pending(), 0);
}
```

Add orphan tests with one uncommitted WAL-tail NVPage and one tree-reachable NVPage; recovery reclaims only the former.

- [ ] **Step 2: Run the ktest gate and confirm RED**

Run `cargo osdk check --ktests --package aster-core`.

Expected: recovery knows generic `Nvbh` descriptors but cannot group typed payloads into inode state.

- [ ] **Step 3: Build the recovered page-cache catalog**

```rust
pub(crate) struct RecoveredInodePcache {
    pub(crate) anchor_ref: PersistentPageRef,
    pub(crate) anchor: InodePcAnchor,
    pub(crate) wal_frames: [PersistentPageRef; WAL_FRAME_COUNT],
    pub(crate) node_frames: BTreeMap<u64, PersistentPageRef>,
}

pub(crate) type RecoveredPcacheCatalog =
    BTreeMap<(u64, u64), RecoveredInodePcache>;
```

During descriptor scan, decode every committed metadata payload header. Group by `(fs_id, inode)`, select the highest valid anchor generation, validate all named WAL frames/root references by slot plus generation, and leave ordinary EXT4 metadata entries on the existing recovery path.

- [ ] **Step 4: Replay before exposing the context**

`cow_context(fs_id, inode)` first consumes a matching recovered catalog entry. Recover the tree, validate B-link reachability, read committed WAL records with `lsn > checkpointed_lsn`, replay each in order, publish checkpoint head advancement, then rebuild `CowIndex` from tree leaves. Only after this succeeds may the context enter `COW_CONTEXTS` or rTIO start selecting its pages.

- [ ] **Step 5: Make orphan reclamation reachability-based**

Build three protected sets: frames reachable from selected anchors, frames reachable from tree root/right-sibling traversal, and NVPages referenced by either selected tree leaves or committed WAL records. Clear and free committed objects outside all three sets only after replay succeeds. A crash after checkpoint-head advancement but before cleanup therefore produces a reclaimable leak, not lost data.

- [ ] **Step 6: Run the complete fault/recovery ktest suite**

Run:

```bash
cd /home/zn/Ant/Rust_EXT4/os_com-rnvpc-foreground-checkpoint
cargo fmt --all -- --check
cd kernel && cargo osdk check --package aster-core
cargo osdk check --ktests --package aster-core
cd .. && make ktest ENABLE_KVM=1
```

Expected: formatting and both checks pass; QEMU ktest reports all tests passed. If KVM permission blocks execution, preserve the exact command and log as an explicit validation gap rather than substituting a compile check.

- [ ] **Step 7: Commit recovery**

```bash
git add kernel/src/fs/rnvpc/{frame.rs,recover.rs,reclaim.rs,mod.rs,cow.rs,tests.rs}
git commit -m "feat(rnvpc): recover persistent WAL and B-link index"
```

---

### Task 9: Add Stable Observability and End-to-End Validation

**Files:**
- Modify: `../os_com/kernel/src/fs/rnvpc/wal.rs`
- Modify: `../os_com/kernel/src/fs/rnvpc/persistent_btree.rs`
- Modify: `../os_com/kernel/src/fs/rnvpc/cow.rs`
- Modify: `../os_com/kernel/src/fs/rnvpc/mod.rs`
- Modify: `../os_com/kernel/src/fs/fs_impls/ext4/fs.rs`
- Modify: `../os_com_implementation/rNVPC-rTIO/implementation.md`
- Modify: `../os_com_implementation/STATUS.md`

**Interfaces:**
- Consumes: all implemented counters and the existing `[RNVPC]` unmount summary path.
- Produces: stable counter names for WAL/checkpoint/tree behavior and final correctness/performance evidence.

- [ ] **Step 1: Write failing counter aggregation tests**

```rust
#[ktest]
fn rnvpc_snapshot_reports_foreground_checkpoint_work() {
    let (_image, _config, pool) = restartable_pool(1200);
    let context = InodeCowContext::create(pool, 12, 34).unwrap();
    for page_index in 0..1024 {
        context.update(page_index, page_index + 1000, |page| page[0] = 1).unwrap();
    }
    context.update(1024, 2024, |page| page[0] = 1).unwrap();
    let stats = context.snapshot();
    assert_eq!(stats.foreground_checkpoints, 1);
    assert_eq!(stats.checkpoint_records_replayed, 1024);
    assert!(stats.btree_node_updates >= 1024);
    assert!(stats.persist_fences > 0);
    assert_eq!(stats.writes_blocked_by_checkpoint, 1);
}
```

- [ ] **Step 2: Add counters without enabling normal-run latency timing**

```rust
pub(crate) struct ForegroundCheckpointCounters {
    pub(crate) wal_appends: u64,
    pub(crate) foreground_checkpoints: u64,
    pub(crate) checkpoint_records_replayed: u64,
    pub(crate) btree_node_updates: u64,
    pub(crate) btree_splits: u64,
    pub(crate) persist_fences: u64,
    pub(crate) writes_blocked_by_checkpoint: u64,
    pub(crate) max_wal_occupancy: u64,
    pub(crate) wal_append_ns: u64,
    pub(crate) checkpoint_ns: u64,
}
```

Always collect integer event counts. Guard nanosecond fields behind the existing compile-time latency switch so normal throughput runs execute no timer reads.

- [ ] **Step 3: Print one machine-parseable summary line**

Extend the existing `[RNVPC]` summary with stable `key=value` fields:

```text
wal_appends= checkpoint_count= checkpoint_records= btree_updates=
btree_splits= persist_fences= checkpoint_blocked_writes= wal_max_used=
```

Do not emit one line per write, WAL record, or B+tree operation.

- [ ] **Step 4: Run complete static and ktest validation**

```bash
cd /home/zn/Ant/Rust_EXT4/os_com-rnvpc-foreground-checkpoint
cargo fmt --all -- --check
cd kernel && cargo osdk check --package aster-core
cargo osdk check --ktests --package aster-core
cd .. && make ktest ENABLE_KVM=1
```

Expected: all pass with no rNVPCe counter regressions and no new warning-level per-operation logs.

- [ ] **Step 5: Run image-backed QEMU smoke tests**

Build once, then run the existing Asterinas EXT4 baseline harness against a disposable image with `rnvpc=on,rtio=on`. Use two workloads:

```text
1. 4 KiB random writes over at least 8 MiB, enough operations to cross 1024 records.
2. 128 KiB sequential writes over at least 256 MiB.
```

Acceptance evidence:

```text
guest test exits successfully;
checkpoint_count > 0 for the 4 KiB workload;
checkpoint_records is a multiple of 1024;
wal_max_used == 1024;
no SSD completion changes checkpoint_count or WAL used;
rNVPCe mode reports all WAL/checkpoint counters as zero.
```

Do not use `/dev/sda3` or `/dev/nvme1n1p1` for this correctness gate. Real-device performance is a separate explicitly authorized run.

- [ ] **Step 6: Record verified behavior in project documents**

Update `rNVPC-rTIO/implementation.md` with implemented interfaces, commit hashes, exact commands, PASS/FAIL evidence, and the modeled-persistence limitation. Update `STATUS.md` only for gates that actually passed; leave any blocked VM gate explicitly open.

- [ ] **Step 7: Commit observability and verified documentation separately**

In the code worktree:

```bash
git add kernel/src/fs/rnvpc/{wal.rs,persistent_btree.rs,cow.rs,mod.rs} \
        kernel/src/fs/fs_impls/ext4/fs.rs
git commit -m "feat(rnvpc): report foreground checkpoint metrics"
```

In `os_com_implementation` after copying the verified commit IDs and results:

```bash
git add rNVPC-rTIO/implementation.md STATUS.md
git commit -m "docs(rnvpc): record foreground checkpoint validation"
```

---

## Final Acceptance Checklist

- [ ] Record 1024 succeeds; record 1025 checkpoints exactly the preceding 1024 records in the caller and then appends as the sole new record.
- [ ] Repeated page keys are replayed one record at a time in increasing LSN order.
- [ ] Persisted ordering is NVPage, WAL record/commit, anchor tail, DRAM mapping.
- [ ] Checkpoint failure leaves persistent and runtime WAL head unchanged.
- [ ] Every node-image, split, root, and anchor fault point recovers to a searchable and replayable state.
- [ ] Same-inode checkpoints serialize; different-inode checkpoints do not use a global lock.
- [ ] rTIO excludes `wal_pinned` pages and admits them only after anchor checkpoint publication.
- [ ] SSD completion leaves WAL head, tail, used, and checkpointed LSN unchanged.
- [ ] Recovery protects all tree/WAL-reachable objects and reclaims only unreachable committed objects.
- [ ] rNVPCe source behavior and WAL-free counters remain unchanged.
- [ ] `cargo fmt`, normal `cargo osdk check`, ktest compile, full KVM ktest, and image-backed QEMU smoke evidence are recorded.
- [ ] Documentation states that DRAM-modeled persistence validates protocol logic but does not prove real-NVDIMM power-loss behavior.
