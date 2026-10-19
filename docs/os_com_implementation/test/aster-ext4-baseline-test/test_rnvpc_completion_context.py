#!/usr/bin/env python3
"""Regression contract for rNVPC BIO completion context.

The device completion callback may run with local interrupts disabled.  It may
account completion and enqueue work, but must not perform the lock-heavy NVPool
state transition itself.
"""

from pathlib import Path
import re
import unittest


FS_RS = (
    Path(__file__).resolve().parents[4]
    / "kernel/src/fs/fs_impls/ext4/fs.rs"
)
INODE_RS = FS_RS.parent / "inode/mod.rs"


class RnvpcCompletionContextTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.source = FS_RS.read_text(encoding="utf-8")

    def test_bio_callback_only_enqueues_heavy_completion(self) -> None:
        match = re.search(
            r"let complete: BioCompleteFn = Box::new\(move \|status\| \{(?P<body>.*?)\n\s*\}\);",
            self.source,
            re.S,
        )
        self.assertIsNotNone(match, "rNVPC BIO completion callback not found")
        body = match.group("body")
        self.assertIn("rnvpc_completion_queue", body)
        for forbidden in (
            "rnvpc::complete_flush",
            "evicting.finish",
            "alloc_blocks",
            "free_blocks",
            "sync_metadata",
            "block_device.sync",
            "migrate_mapped_nv_page",
            "release_rnvpce_blocks",
            "free_data_page",
        ):
            self.assertNotIn(forbidden, body)


    def test_completion_queue_is_preallocated_before_interrupt_use(self) -> None:
        self.assertRegex(
            self.source,
            r"VecDeque::with_capacity\(\s*Self::RNVPC_MAX_INFLIGHT_BIOS",
        )

    def test_rnvpce_completion_does_not_acquire_inode_publication(self) -> None:
        start = self.source.index("    fn publish_and_reclaim_rnvpce_group(")
        end = self.source.index("    fn reclaim_rnvpce_page(", start)
        body = self.source[start:end]
        self.assertIn("migrate_mapped_nv_pages_to_ssd_by_reverse", body)
        self.assertNotIn("begin_rnvpce_publication", body)
        self.assertNotIn("self.read_inode", body)

    def test_callback_cannot_silently_grow_completion_queue(self) -> None:
        match = re.search(
            r"let complete: BioCompleteFn = Box::new\(move \|status\| \{(?P<body>.*?)\n\s*\}\);",
            self.source,
            re.S,
        )
        body = match.group("body")
        self.assertIn("queue.len() < queue.capacity()", body)
        self.assertNotIn("INFLIGHT_FLUSH_BIOS.fetch_sub", body)
        self.assertNotIn("INFLIGHT_FLUSH_PAGES.fetch_sub", body)
    def test_completion_worker_owns_nvpool_completion_transition(self) -> None:
        start = self.source.find("fn process_rnvpc_completions(&self)")
        self.assertNotEqual(start, -1, "deferred rNVPC completion processor not found")
        end = self.source.find("    /// 单条合并 BIO", start)
        self.assertNotEqual(end, -1, "end of completion processor not found")
        body = self.source[start:end]
        self.assertIn("rnvpc::complete_flush", body)
        self.assertIn("evicting.finish", body)

    def test_migration_thread_uses_physical_dirty_queue_without_tree_scan(self) -> None:
        mod_rs = FS_RS.parents[2] / "rnvpc/mod.rs"
        source = mod_rs.read_text(encoding="utf-8")
        start = source.find("pub(crate) fn rnvpce_flush_candidates")
        self.assertNotEqual(start, -1, "rNVPCe physical candidate collector not found")
        end = source.find("\n}\n", start)
        self.assertNotEqual(end, -1)
        body = source[start:end]
        self.assertIn("collect_dirty_nvpages", body)
        self.assertIn("reverse_lookup", body)
        self.assertNotIn("mapped_nv_page_ref", body)
        self.assertNotIn("collect_inode_pages", body)

    def test_flush_latency_breakdown_keeps_device_timing_in_callback(self) -> None:
        callback = re.search(
            r"let complete: BioCompleteFn = Box::new\(move \|status\| \{(?P<body>.*?)\n\s*\}\);",
            self.source,
            re.S,
        )
        self.assertIsNotNone(callback)
        self.assertIn("nvpc_flush_device.record_since(submitted_at)", callback.group("body"))
        self.assertNotIn("nvpc_flush_materialize", callback.group("body"))
        self.assertLess(
            self.source.find("let submitted_at = read_ns();"),
            self.source.find("let complete: BioCompleteFn"),
        )

    def test_flush_worker_records_serial_completion_phases(self) -> None:
        start = self.source.find("fn process_rnvpc_completions(&self)")
        end = self.source.find("    /// 单条合并 BIO", start)
        body = self.source[start:end]
        self.assertNotIn("materialize_rnvpce_completion", body)
        self.assertNotIn("checkpoint_rnvpce_ssd_mappings", body)
        for counter in (
            "nvpc_flush_durability",
            "rnvpce_completion_group",
            "nvpc_flush_reclaim",
        ):
            self.assertIn(counter, body)

    def test_rnvpce_capacity_and_migration_waits_are_separated(self) -> None:
        source = INODE_RS.read_text(encoding="utf-8")
        start = source.find("if fs.rnvpc_extend_enabled() && self.type_ == InodeType::File")
        self.assertNotEqual(start, -1, "rNVPCe write path not found")
        end = source.find("// Native EXT4 and the WAL-based rNVPC comparison", start)
        self.assertNotEqual(end, -1, "end of rNVPCe write path not found")
        body = source[start:end]

        capacity_start = body.find("pool.foreground_write_must_throttle()")
        capacity_end = body.find("let remain_before", capacity_start)
        capacity = body[capacity_start:capacity_end]
        self.assertIn("Thread::yield_now()", capacity)
        self.assertNotIn("wait_for_data_page_lease_change", capacity)

        self.assertIsNotNone(re.search(
            r"Err\(error\)\s+if error\.error\(\) == Errno::EBUSY.*?"
            r"wait_for_data_page_lease_change",
            body,
            re.S,
        ))
        self.assertIsNotNone(re.search(
            r"Err\(error\)\s+if error\.error\(\) == Errno::ESTALE.*?"
            r"Thread::yield_now\(\)",
            body,
            re.S,
        ))

if __name__ == "__main__":
    unittest.main()
