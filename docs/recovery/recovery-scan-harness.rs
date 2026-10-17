#![allow(dead_code)]
use std::collections::BTreeSet;
#[derive(Clone,Copy,Debug,PartialEq)] enum Errno { EINVAL,EUCLEAN }
type Result<T> = std::result::Result<T,Errno>;
macro_rules! return_errno_with_message { ($e:expr,$m:expr) => {return Err($e)}; }
#[derive(Clone,Copy,PartialEq)] enum PoolClass {Metadata,Data}
#[derive(Clone,Copy)] enum NvObjectKind {NvPage,Nvbh}
#[derive(Clone,Copy,Eq,PartialEq,Ord,PartialOrd)] struct PageKey{fs_id:u64,inode:u64,page_index:u64}
impl PageKey{fn new(fs_id:u64,inode:u64,page_index:u64)->Self{Self{fs_id,inode,page_index}}}
const FIXED_INODE_LOGICAL_INDEX:u64=(1<<61)|3;
#[derive(Clone,Copy)] struct Descriptor{fs_id:u64,inode:u64,logical_index:u64,recovery_id:u64,ssd_backing_block:u64}
impl Descriptor{fn kind(&self)->Result<NvObjectKind>{Ok(NvObjectKind::Nvbh)} fn allocation_seq(&self)->u64{self.recovery_id}}
struct PersistEngine(Vec<Descriptor>);
impl PersistEngine{fn read_header(&self)->Result<()>{Ok(())} fn read_entry(&self,i:usize)->Result<Option<Descriptor>>{Ok(self.0.get(i).copied())}}
struct RecoveredEntry {
    index: usize,
    class: PoolClass,
    key: PageKey,
    version: u64,
    allocation_seq: u64,
    ssd_backing_block: u64,
}

fn scan(engine: &PersistEngine, capacity_pages: usize) -> Result<Vec<RecoveredEntry>> {
    engine.read_header()?;
    let mut metadata_identities = BTreeSet::new();
    let mut fixed_roots = BTreeSet::new();
    let mut metadata_entries = Vec::new();
    let mut data_entries = Vec::new();
    for index in 0..capacity_pages {
        // 自描述：只靠条目自己的描述符就能知道“这页是谁、数据在哪”，
        // 不需要预建“SSD block → NV object”的全局映射。
        let Some(descriptor) = engine.read_entry(index)? else {
            continue;
        };
        let class = match descriptor.kind()? {
            NvObjectKind::NvPage => PoolClass::Data,
            NvObjectKind::Nvbh => PoolClass::Metadata,
        };
        let key = PageKey::new(descriptor.fs_id, descriptor.inode, descriptor.logical_index);
        let version = descriptor.recovery_id;
        let candidate = RecoveredEntry {
            index,
            class,
            key,
            version,
            allocation_seq: descriptor.allocation_seq(),
            ssd_backing_block: descriptor.ssd_backing_block,
        };
        if class == PoolClass::Data {
            data_entries.push(candidate);
            continue;
        }
        if key.page_index == FIXED_INODE_LOGICAL_INDEX
            && !fixed_roots.insert((key.fs_id, key.inode)) {
            return_errno_with_message!(Errno::EUCLEAN, "duplicate fixed inode roots");
        }
        if !metadata_identities.insert((key, version)) {
            return_errno_with_message!(Errno::EINVAL, "duplicate RNVPC metadata version");
        }
        metadata_entries.push(candidate);
    }
    let mut entries = metadata_entries;
    entries.extend(data_entries);
    Ok(entries)
}


#[test] fn duplicate_fixed_root_rejected_before_rebuild(){let e=PersistEngine((1..=2).map(|v|Descriptor{fs_id:1,inode:2,logical_index:FIXED_INODE_LOGICAL_INDEX,recovery_id:v,ssd_backing_block:0}).collect());assert!(matches!(scan(&e,2),Err(Errno::EUCLEAN)));}
#[test] fn different_inode_roots_allowed(){let e=PersistEngine((1..=2).map(|i|Descriptor{fs_id:1,inode:i,logical_index:FIXED_INODE_LOGICAL_INDEX,recovery_id:i,ssd_backing_block:0}).collect());assert_eq!(scan(&e,2).unwrap().len(),2);}
