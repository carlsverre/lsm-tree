mod gc;
pub mod index;
pub mod value;

use self::value::MaybeInlineValue;
use crate::{
    file::BLOBS_FOLDER,
    r#abstract::{AbstractTree, RangeItem},
    serde::{Deserializable, Serializable},
    tree::inner::MemtableId,
    Config, KvPair, MemTable, SegmentId, SeqNo, Snapshot, UserKey, Value, ValueType,
};
use gc::{reader::GcReader, writer::GcWriter};
use index::IndexTree;
use std::{
    io::Cursor,
    ops::RangeBounds,
    sync::{Arc, RwLockWriteGuard},
};
use value_log::{ValueHandle, ValueLog};

/// A key-value-separated log-structured merge tree
///
/// This tree is a composite structure, consisting of an
/// index tree (LSM-tree) and a log-structured value log
/// to reduce write amplification.
///
/// See <https://docs.rs/value-log> for more information.
#[derive(Clone)]
pub struct BlobTree {
    /// Index tree that holds value handles or small inline values
    #[doc(hidden)]
    pub index: IndexTree,

    /// Log-structured value-log that stores large values
    #[doc(hidden)]
    pub blobs: ValueLog,
}

impl BlobTree {
    pub fn open(config: Config) -> crate::Result<Self> {
        let path = &config.path;

        let vlog_path = path.join(BLOBS_FOLDER);
        let vlog_cfg = value_log::Config::default()
            .blob_cache(config.blob_cache.clone())
            .segment_size_bytes(config.blob_file_target_size);

        let index: IndexTree = config.open()?.into();

        Ok(Self {
            index,
            blobs: ValueLog::open(vlog_path, vlog_cfg)?,
        })
    }

    fn resolve_value_handle(&self, item: RangeItem) -> RangeItem {
        match item {
            Ok((key, value)) => {
                let mut cursor = Cursor::new(value);
                let item = MaybeInlineValue::deserialize(&mut cursor)?;

                match item {
                    MaybeInlineValue::Inline(bytes) => Ok((key, bytes)),
                    MaybeInlineValue::Indirect { handle, .. } => match self.blobs.get(&handle) {
                        Ok(Some(bytes)) => Ok((key, bytes)),
                        Err(e) => Err(e.into()),
                        _ => panic!("Aahhhh"), // TODO: 2.0.0
                    },
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Scans the index tree, collecting statistics about
    /// value log fragmentation
    #[doc(hidden)]
    pub fn gc_scan_stats(&self) -> crate::Result<()> {
        use std::io::{Error as IoError, ErrorKind as IoErrorKind};
        use MaybeInlineValue::{Indirect, Inline};

        self.blobs
            .scan_for_stats(self.index.iter().filter_map(|kv| {
                let Ok((_, v)) = kv else {
                    return Some(Err(IoError::new(
                        IoErrorKind::Other,
                        "Failed to load KV pair",
                    )));
                };

                let mut cursor = Cursor::new(v);
                let value = match MaybeInlineValue::deserialize(&mut cursor) {
                    Ok(v) => v,
                    Err(e) => return Some(Err(IoError::new(IoErrorKind::Other, e.to_string()))),
                };

                match value {
                    Indirect { handle, size } => Some(Ok((handle, size))),
                    Inline(_) => None,
                }
            }))?;

        Ok(())
    }

    pub fn gc_with_target_space_amp(
        &self,
        space_amp_target: f32,
        seqno: SeqNo,
    ) -> crate::Result<()> {
        let ids = self
            .blobs
            .select_segments_for_space_amp_reduction(space_amp_target);

        // IMPORTANT: Write lock memtable to avoid read skew
        let memtable_lock = self.index.lock_active_memtable();

        self.blobs.rollover(
            &ids,
            &GcReader::new(&self.index, &memtable_lock),
            GcWriter::new(seqno, &memtable_lock),
        )?;

        // NOTE: We still have the memtable lock, can't use gc_drop_stable because recursive locking
        self.blobs.drop_stale_segments()?;

        Ok(())
    }

    /// Rewrites blob files that have reached a stale threshold
    pub fn gc_with_staleness_threshold(
        &self,
        stale_threshold: f32,
        seqno: SeqNo,
    ) -> crate::Result<()> {
        // First, find the segment IDs that are stale
        let ids = self
            .blobs
            .find_segments_with_stale_threshold(stale_threshold);

        // IMPORTANT: Write lock memtable to avoid read skew
        let memtable_lock = self.index.lock_active_memtable();

        self.blobs.rollover(
            &ids,
            &GcReader::new(&self.index, &memtable_lock),
            GcWriter::new(seqno, &memtable_lock),
        )?;

        // NOTE: We still have the memtable lock, can't use gc_drop_stable because recursive locking
        self.blobs.drop_stale_segments()?;

        Ok(())
    }

    /// Drops all stale blob segment files
    #[doc(hidden)]
    pub fn gc_drop_stale(&self) -> crate::Result<()> {
        // IMPORTANT: Write lock memtable to avoid read skew
        let _lock = self.index.lock_active_memtable();
        self.blobs.drop_stale_segments()?;
        Ok(())
    }

    pub fn flush_active_memtable(&self) -> crate::Result<Option<Arc<crate::Segment>>> {
        let Some((segment_id, yanked_memtable)) = self.index.rotate_memtable() else {
            return Ok(None);
        };

        let segment = self.flush_memtable(segment_id, &yanked_memtable)?;
        self.register_segments(&[segment.clone()])?;

        Ok(Some(segment))
    }
}

impl AbstractTree for BlobTree {
    fn flush_memtable(
        &self,
        segment_id: SegmentId,
        memtable: &Arc<MemTable>,
    ) -> crate::Result<Arc<crate::Segment>> {
        use crate::{
            file::SEGMENTS_FOLDER,
            segment::writer::{Options, Writer as SegmentWriter},
        };
        use value::MaybeInlineValue;

        log::debug!("flushing memtable & performing key-value separation");

        let lsm_segment_folder = self.index.config.path.join(SEGMENTS_FOLDER);

        let mut segment_writer = SegmentWriter::new(Options {
            segment_id,
            block_size: self.index.config.inner.block_size,
            evict_tombstones: false,
            folder: lsm_segment_folder,
            compression: self.index.config.inner.compression,

            #[cfg(feature = "bloom")]
            bloom_fp_rate: 0.0001,
        })?;
        let mut blob_writer = self.blobs.get_writer()?;

        let blob_id = blob_writer.segment_id();

        for entry in &memtable.items {
            let key = entry.key();

            let value = entry.value();
            let mut cursor = Cursor::new(value);
            let value = MaybeInlineValue::deserialize(&mut cursor)?;
            let MaybeInlineValue::Inline(value) = value else {
                panic!("values are initially always inlined");
            };

            let size = value.len() as u32;

            // TODO: 2.0.0 blob threshold
            let value_wrapper = if size < 2_048 {
                MaybeInlineValue::Inline(value)
            } else {
                let offset = blob_writer.offset(&key.user_key);
                blob_writer.write(&key.user_key, &value)?;

                let value_handle = ValueHandle {
                    offset,
                    segment_id: blob_id,
                };
                MaybeInlineValue::Indirect {
                    handle: value_handle,
                    size,
                }
            };

            let mut serialized = vec![];
            value_wrapper.serialize(&mut serialized)?;

            segment_writer.write(crate::Value::from(((key.clone()), serialized.into())))?;
        }

        self.blobs.register(blob_writer)?;
        let created_segment = self.index.consume_writer(segment_id, segment_writer)?;

        Ok(created_segment)
    }

    fn register_segments(&self, segments: &[Arc<crate::Segment>]) -> crate::Result<()> {
        self.index.register_segments(segments)
    }

    fn lock_active_memtable(&self) -> std::sync::RwLockWriteGuard<'_, MemTable> {
        self.index.lock_active_memtable()
    }

    fn set_active_memtable(&self, memtable: MemTable) {
        self.index.set_active_memtable(memtable);
    }

    fn add_sealed_memtable(&self, id: MemtableId, memtable: Arc<MemTable>) {
        self.index.add_sealed_memtable(id, memtable);
    }

    fn compact(
        &self,
        strategy: Arc<dyn crate::compaction::CompactionStrategy>,
    ) -> crate::Result<()> {
        self.index.compact(strategy)
    }

    fn get_next_segment_id(&self) -> SegmentId {
        self.index.get_next_segment_id()
    }

    fn tree_config(&self) -> &Config {
        &self.index.config
    }

    fn get_lsn(&self) -> Option<SeqNo> {
        self.index.get_lsn()
    }

    fn active_memtable_size(&self) -> u32 {
        self.index.active_memtable_size()
    }

    fn tree_type(&self) -> crate::TreeType {
        crate::TreeType::Blob
    }

    fn rotate_memtable(&self) -> Option<(crate::tree::inner::MemtableId, Arc<crate::MemTable>)> {
        self.index.rotate_memtable()
    }

    fn segment_count(&self) -> usize {
        self.index.segment_count()
    }

    fn first_level_segment_count(&self) -> usize {
        self.index.first_level_segment_count()
    }

    fn approximate_len(&self) -> u64 {
        self.index.approximate_len()
    }

    // NOTE: Override the default implementation to not fetch
    // data from the value log, so we get much faster key reads
    fn contains_key<K: AsRef<[u8]>>(&self, key: K) -> crate::Result<bool> {
        self.index.contains_key(key)
    }

    // NOTE: Override the default implementation to not fetch
    // data from the value log, so we get much faster scans
    fn len(&self) -> crate::Result<usize> {
        self.index.len()
    }

    #[must_use]
    fn disk_space(&self) -> u64 {
        self.index.disk_space() + self.blobs.manifest.disk_space_used()
    }

    fn get_memtable_lsn(&self) -> Option<SeqNo> {
        self.index.get_memtable_lsn()
    }

    fn get_segment_lsn(&self) -> Option<SeqNo> {
        self.index.get_segment_lsn()
    }

    fn register_snapshot(&self) {
        self.index.open_snapshots.increment();
    }

    fn deregister_snapshot(&self) {
        self.index.open_snapshots.decrement();
    }

    fn snapshot(&self, seqno: SeqNo) -> Snapshot {
        use crate::AnyTree::Blob;

        Snapshot::new(Blob(self.clone()), seqno)
    }

    fn iter_with_seqno<'a>(
        &'a self,
        seqno: SeqNo,
        index: Option<&'a MemTable>,
    ) -> Box<dyn DoubleEndedIterator<Item = crate::Result<KvPair>> + 'a> {
        self.range_with_seqno::<UserKey, _>(.., seqno, index)
    }

    fn range_with_seqno<'a, K: AsRef<[u8]>, R: RangeBounds<K>>(
        &'a self,
        range: R,
        seqno: SeqNo,
        index: Option<&'a MemTable>,
    ) -> Box<dyn DoubleEndedIterator<Item = crate::Result<KvPair>> + 'a> {
        Box::new(
            self.index
                .0
                .create_range(range, Some(seqno), index)
                .map(|item| self.resolve_value_handle(item)),
        )
    }

    fn prefix_with_seqno<'a, K: AsRef<[u8]>>(
        &'a self,
        prefix: K,
        seqno: SeqNo,
        index: Option<&'a MemTable>,
    ) -> Box<dyn DoubleEndedIterator<Item = crate::Result<KvPair>> + 'a> {
        Box::new(
            self.index
                .0
                .create_prefix(prefix, Some(seqno), index)
                .map(|item| self.resolve_value_handle(item)),
        )
    }

    fn range<K: AsRef<[u8]>, R: RangeBounds<K>>(
        &self,
        range: R,
    ) -> Box<dyn DoubleEndedIterator<Item = crate::Result<KvPair>> + '_> {
        Box::new(
            self.index
                .0
                .create_range(range, None, None)
                .map(|item| self.resolve_value_handle(item)),
        )
    }

    fn prefix<K: AsRef<[u8]>>(
        &self,
        prefix: K,
    ) -> Box<dyn DoubleEndedIterator<Item = crate::Result<KvPair>> + '_> {
        Box::new(
            self.index
                .0
                .create_prefix(prefix, None, None)
                .map(|item| self.resolve_value_handle(item)),
        )
    }

    fn raw_insert_with_lock<K: AsRef<[u8]>, V: AsRef<[u8]>>(
        &self,
        lock: &RwLockWriteGuard<'_, MemTable>,
        key: K,
        value: V,
        seqno: SeqNo,
        r#type: ValueType,
    ) -> (u32, u32) {
        use value::MaybeInlineValue;

        // NOTE: Initially, we always write an inline value
        // On memtable flush, depending on the values' sizes, they will be separated
        // into inline or indirect values
        let item = MaybeInlineValue::Inline(value.as_ref().into());

        let mut value = vec![];
        item.serialize(&mut value).expect("should serialize");

        let value = Value::new(key.as_ref(), value, seqno, r#type);
        lock.insert(value)
    }

    fn insert<K: AsRef<[u8]>, V: AsRef<[u8]>>(&self, key: K, value: V, seqno: SeqNo) -> (u32, u32) {
        use value::MaybeInlineValue;

        // NOTE: Initially, we always write an inline value
        // On memtable flush, depending on the values' sizes, they will be separated
        // into inline or indirect values
        let item = MaybeInlineValue::Inline(value.as_ref().into());

        let mut value = vec![];
        item.serialize(&mut value).expect("should serialize");

        self.index.insert(key, value, seqno)
    }

    fn get_with_seqno<K: AsRef<[u8]>>(
        &self,
        key: K,
        seqno: SeqNo,
    ) -> crate::Result<Option<crate::UserValue>> {
        use value::MaybeInlineValue::{Indirect, Inline};

        let Some(value) = self.index.get_internal_with_seqno(key.as_ref(), seqno)? else {
            return Ok(None);
        };

        match value {
            Inline(bytes) => Ok(Some(bytes)),
            Indirect { handle, .. } => {
                // Resolve indirection using value log
                self.blobs.get(&handle).map_err(Into::into)
            }
        }
    }

    fn get<K: AsRef<[u8]>>(&self, key: K) -> crate::Result<Option<Arc<[u8]>>> {
        use value::MaybeInlineValue::{Indirect, Inline};

        let Some(value) = self.index.get_internal(key.as_ref())? else {
            return Ok(None);
        };

        match value {
            Inline(bytes) => Ok(Some(bytes)),
            Indirect { handle, .. } => {
                // Resolve indirection using value log
                self.blobs.get(&handle).map_err(Into::into)
            }
        }
    }

    fn remove<K: AsRef<[u8]>>(&self, key: K, seqno: SeqNo) -> (u32, u32) {
        self.index.remove(key, seqno)
    }
}
