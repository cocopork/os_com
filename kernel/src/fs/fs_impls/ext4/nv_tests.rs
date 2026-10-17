// SPDX-License-Identifier: MPL-2.0

//! Route tests explicitly initialize a shared pool with default modes off.
use ostd::prelude::*;

use super::{
    prelude::*,
    test_utils::{Ext4FixtureBuilder, make_empty_file_inode},
};
use crate::fs::{
    file::StatusFlags,
    vfs::inode::{FileOps, Inode as VfsInode},
};

#[ktest]
fn nv_route_rnvpce_direct_partial_write_and_deferred_export() {
    crate::fs::rnvpc::init_for_ext4_tests();
    crate::time::clocks::init_for_ktest();
    let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
        .with_block_bitmap_metadata_marked()
        .with_uuid([0x91; 16])
        .with_mount_options("rnvpce=on,rtio=off")
        .build()
        .unwrap();
    fixture.write_raw_inode(11, &make_empty_file_inode());
    let inode = fixture.ext4.read_inode(11).unwrap();
    let flushes = fixture.disk.flush_count();
    let payload = [0x74u8; 37];
    let mut reader = VmReader::from(payload.as_slice()).to_fallible();
    assert_eq!(
        FileOps::write_at(
            inode.as_ref(),
            3,
            &mut reader,
            StatusFlags::O_DIRECT | StatusFlags::O_SYNC
        )
        .unwrap(),
        payload.len()
    );
    assert_eq!(
        fixture.disk.flush_count(),
        flushes,
        "per-file sync must not export or flush SSD"
    );
    let mut output = [0u8; 40];
    let mut writer = VmWriter::from(output.as_mut_slice()).to_fallible();
    assert_eq!(
        FileOps::read_at(inode.as_ref(), 0, &mut writer, StatusFlags::O_DIRECT).unwrap(),
        40
    );
    assert_eq!(&output[..3], &[0; 3]);
    assert_eq!(&output[3..], &payload);
    assert!(VfsInode::page_cache(inode.as_ref()).is_none());
    assert_eq!(inode.resize(0).unwrap_err().error(), Errno::EOPNOTSUPP);
    fixture.ext4.sync_all().unwrap();
    let mut after = [0u8; 40];
    inode
        .read_at(0, &mut VmWriter::from(after.as_mut_slice()).to_fallible())
        .unwrap();
    assert_eq!(after, output);
    let pool = crate::fs::rnvpc::pool().unwrap();
    assert!(
        crate::fs::rnvpc::write_transaction::mapped_nv_page_ref(
            pool,
            fixture.ext4.rnvpc_fs_id(),
            11,
            0
        )
        .unwrap()
        .is_none()
    );
    assert!(
        crate::fs::rnvpc::write_transaction::current_page_mapping(
            pool,
            fixture.ext4.rnvpc_fs_id(),
            11,
            0
        )
        .unwrap()
        .unwrap()
        .is_written()
    );
}

#[ktest]
fn nv_route_rnvpc_wal_and_native_paths_remain_distinct() {
    crate::fs::rnvpc::init_for_ext4_tests();
    crate::time::clocks::init_for_ktest();
    let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
        .with_block_bitmap_metadata_marked()
        .with_uuid([0x92; 16])
        .with_mount_options("rnvpc=on,rtio=off")
        .build()
        .unwrap();
    fixture.write_raw_inode(11, &make_empty_file_inode());
    let inode = fixture.ext4.read_inode(11).unwrap();
    let payload = [0x39u8; 73];
    assert_eq!(
        inode
            .write_at(7, &mut VmReader::from(payload.as_slice()).to_fallible())
            .unwrap(),
        73
    );
    assert_eq!(fixture.read_raw_inode(11).size_lo, 80);
    let mut output = [0u8; 80];
    assert_eq!(
        inode
            .read_at(0, &mut VmWriter::from(output.as_mut_slice()).to_fallible())
            .unwrap(),
        80
    );
    assert_eq!(&output[..7], &[0; 7]);
    assert_eq!(&output[7..], &payload);
    let context = crate::fs::rnvpc::cow_context(fixture.ext4.rnvpc_fs_id(), 11).unwrap();
    assert_eq!(context.foreground_checkpoint_counters().wal_appends, 1);
    assert_eq!(
        context.flush_candidates(128).len(),
        0,
        "uncheckpointed WAL pages cannot migrate"
    );
    assert_eq!(inode.resize(0).unwrap_err().error(), Errno::EOPNOTSUPP);
    let native = Ext4FixtureBuilder::new(2048, 256, 2048)
        .with_block_bitmap_metadata_marked()
        .with_uuid([0x93; 16])
        .with_mount_options("rnvpc=off,rnvpc_extend=off")
        .build()
        .unwrap();
    native.write_raw_inode(11, &make_empty_file_inode());
    let inode = native.ext4.read_inode(11).unwrap();
    assert_eq!(
        inode
            .write_at(0, &mut VmReader::from(payload.as_slice()).to_fallible())
            .unwrap(),
        73
    );
    assert!(VfsInode::page_cache(inode.as_ref()).is_some());
    assert!(crate::fs::rnvpc::existing_cow_context(native.ext4.rnvpc_fs_id(), 11).is_none());
    inode.resize(0).unwrap();
}

#[ktest]
fn nv_route_busy_lease_read_and_partial_write_retry() {
    use core::sync::atomic::{AtomicUsize, Ordering};
    crate::fs::rnvpc::init_for_ext4_tests();
    crate::time::clocks::init_for_ktest();
    let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
        .with_block_bitmap_metadata_marked()
        .with_uuid([0x94; 16])
        .with_mount_options("rnvpc=on,rtio=off")
        .build()
        .unwrap();
    fixture.write_raw_inode(11, &make_empty_file_inode());
    let inode = fixture.ext4.read_inode(11).unwrap();
    let original = [0x31u8; PAGE_SIZE];
    inode
        .write_at(0, &mut VmReader::from(original.as_slice()).to_fallible())
        .unwrap();
    let context = crate::fs::rnvpc::cow_context(fixture.ext4.rnvpc_fs_id(), 11).unwrap();
    let pool = crate::fs::rnvpc::pool().unwrap();
    for write in [false, true] {
        // Direct lease admission injects the transient condition deterministically;
        // production rTIO additionally requires a checkpointed WAL mapping.
        let held = pool
            .pin_for_writeback(context.resident_page(0).unwrap())
            .unwrap();
        let state = Arc::new(AtomicUsize::new(0));
        let worker_state = state.clone();
        let worker_inode = inode.clone();
        let worker = crate::thread::kernel_thread::ThreadOptions::new(move || {
            worker_state.store(1, Ordering::Release);
            if write {
                let replacement = [0x72u8; 19];
                assert_eq!(
                    worker_inode
                        .write_at(
                            17,
                            &mut VmReader::from(replacement.as_slice()).to_fallible()
                        )
                        .unwrap(),
                    19
                );
            } else {
                let mut output = [0u8; PAGE_SIZE];
                assert_eq!(
                    worker_inode
                        .read_at(0, &mut VmWriter::from(output.as_mut_slice()).to_fallible())
                        .unwrap(),
                    PAGE_SIZE
                );
                assert_eq!(output, [0x31u8; PAGE_SIZE]);
            }
            worker_state.store(2, Ordering::Release);
        })
        .spawn();
        while state.load(Ordering::Acquire) == 0 {
            crate::thread::Thread::yield_now();
        }
        for _ in 0..4 {
            crate::thread::Thread::yield_now();
        }
        assert_eq!(
            state.load(Ordering::Acquire),
            1,
            "NV access must wait for the exact busy lease"
        );
        drop(held);
        worker.join();
        assert_eq!(state.load(Ordering::Acquire), 2);
    }
    let mut output = [0u8; PAGE_SIZE];
    inode
        .read_at(0, &mut VmWriter::from(output.as_mut_slice()).to_fallible())
        .unwrap();
    assert_eq!(&output[..17], &[0x31; 17]);
    assert_eq!(&output[17..36], &[0x72; 19]);
    assert!(output[36..].iter().all(|byte| *byte == 0x31));
    assert_eq!(
        context.foreground_checkpoint_counters().wal_appends,
        2,
        "retry must not duplicate the user prefix or WAL append"
    );
}

#[ktest]
fn nv_route_rnvpc_failed_allocation_flush_does_not_append_wal() {
    crate::fs::rnvpc::init_for_ext4_tests();
    crate::time::clocks::init_for_ktest();
    let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
        .with_block_bitmap_metadata_marked()
        .with_uuid([0x95; 16])
        .with_mount_options("rnvpc=on,rtio=off")
        .build()
        .unwrap();
    fixture.write_raw_inode(11, &make_empty_file_inode());
    let inode = fixture.ext4.read_inode(11).unwrap();
    let context = crate::fs::rnvpc::cow_context(fixture.ext4.rnvpc_fs_id(), 11).unwrap();
    let payload = [0x43u8; 31];
    fixture.disk.set_fail_flushes(true);
    let result = inode.write_at(0, &mut VmReader::from(payload.as_slice()).to_fallible());
    fixture.disk.set_fail_flushes(false);
    assert_eq!(result.unwrap_err().error(), Errno::EIO);
    assert_eq!(context.foreground_checkpoint_counters().wal_appends, 0);
    assert!(context.resident_page(0).is_none());
    // Retry must make the allocation durable even when it already exists in RAM.
    assert_eq!(
        inode
            .write_at(0, &mut VmReader::from(payload.as_slice()).to_fallible())
            .unwrap(),
        31
    );
    assert_eq!(context.foreground_checkpoint_counters().wal_appends, 1);
    assert_eq!(fixture.read_raw_inode(11).size_lo, 31);
}

#[ktest]
fn nv_route_mount_rejects_switching_persistent_authority() {
    crate::fs::rnvpc::init_for_ext4_tests();
    crate::time::clocks::init_for_ktest();
    for (uuid, mode, opposite) in [
        ([0x96; 16], "rnvpc=on,rtio=off", "rnvpce=on,rtio=off"),
        ([0x97; 16], "rnvpce=on,rtio=off", "rnvpc=on,rtio=off"),
    ] {
        let fixture = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_uuid(uuid)
            .with_mount_options(mode)
            .build()
            .unwrap();
        fixture.write_raw_inode(11, &make_empty_file_inode());
        let inode = fixture.ext4.read_inode(11).unwrap();
        inode
            .write_at(0, &mut VmReader::from([0x53u8; 8].as_slice()).to_fallible())
            .unwrap();
        for rejected in [opposite, "rnvpc=off,rnvpc_extend=off"] {
            let result = Ext4FixtureBuilder::new(2048, 256, 2048)
                .with_block_bitmap_metadata_marked()
                .with_uuid(uuid)
                .with_mount_options(rejected)
                .build();
            match result {
                Err(error) => assert_eq!(error.error(), Errno::EINVAL),
                Ok(_) => panic!("mount accepted incompatible persistent authority"),
            }
        }
    }
}
