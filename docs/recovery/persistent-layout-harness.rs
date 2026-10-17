// SPDX-License-Identifier: MPL-2.0
// Standalone codec verification. Uses the real production source; it does not
// model the NV backend, concurrency, metadata commit, or crash durability.
#![allow(dead_code)]
macro_rules! return_errno_with_message {
    ($errno:expr, $message:expr) => { return Err(crate::prelude::Error($errno)) };
}
mod prelude {
    pub const PAGE_SIZE: usize = 4096;
    #[derive(Debug, PartialEq)]
    pub enum Errno { EINVAL }
    #[derive(Debug)]
    pub struct Error(pub Errno);
    impl Error { pub fn new(errno: Errno) -> Self { Self(errno) } }
    pub type Result<T> = core::result::Result<T, Error>;
}
#[path = "../../kernel/src/fs/rnvpc/pcache_layout.rs"]
mod pcache_layout;
use pcache_layout::*;
use prelude::PAGE_SIZE;

#[test]
fn wal_record_commit_and_checksum_are_both_required() {
    let record = PersistentWalRecord::upsert(1, 4, PersistentPageRef::NONE,
        PersistentPageRef::new(3, 7), 55);
    let mut bytes = [0; WAL_RECORD_BYTES];
    record.encode(&mut bytes);
    assert_eq!(PersistentWalRecord::decode(&bytes).unwrap(), record);
    bytes[20] ^= 1;
    assert!(PersistentWalRecord::decode(&bytes).is_err());
    record.encode(&mut bytes);
    bytes[56..64].fill(0);
    assert!(PersistentWalRecord::decode(&bytes).is_err());
}

#[test]
fn invalid_selector_recovers_a_valid_local_node_image() {
    let node = BtreeNodeFrame::empty_leaf(1, 2, 3);
    let mut bytes = [0; PAGE_SIZE];
    node.encode_page(&mut bytes).unwrap();
    bytes[40..48].copy_from_slice(&u64::MAX.to_le_bytes());
    let recovered = BtreeNodeFrame::decode_page(&bytes, 1, 2, 3).unwrap();
    assert_eq!(recovered.images[recovered.active_slot as usize], node.images[0]);
}

#[test]
fn invalid_selector_recovers_a_valid_anchor_image() {
    let anchor = InodePcAnchor::empty(1, 2, PersistentPageRef::new(3, 7));
    let mut bytes = [0; PAGE_SIZE];
    anchor.encode_page(&mut bytes).unwrap();
    bytes[40..48].copy_from_slice(&u64::MAX.to_le_bytes());
    let recovered = InodePcAnchor::decode_page(&bytes, 1, 2).unwrap();
    assert_eq!(recovered.images[0], anchor.images[0]);
}

#[test]
fn unselected_new_image_does_not_publish_early() {
    let mut anchor = InodePcAnchor::empty(1, 2, PersistentPageRef::new(3, 7));
    anchor.images[1] = anchor.images[0];
    anchor.images[1].generation += 1;
    anchor.images[1].root = PersistentPageRef::new(8, 9);
    let mut bytes = [0; PAGE_SIZE];
    anchor.encode_page(&mut bytes).unwrap();
    let recovered = InodePcAnchor::decode_page(&bytes, 1, 2).unwrap();
    assert_eq!(recovered.images[0], anchor.images[0]);
}
