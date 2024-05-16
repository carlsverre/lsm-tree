use std::{
    fs::File,
    io::{BufReader, Seek, Write},
    path::Path,
};

use super::{meta::Metadata, writer::FileOffsets};
use crate::{
    serde::{Deserializable, Serializable},
    SerializeError,
};
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};

pub const TRAILER_MAGIC: &[u8] = &[b'L', b'S', b'M', b'T', b'T', b'R', b'L', b'1'];
pub const TRAILER_SIZE: usize = 256;

#[derive(Debug)]
#[allow(clippy::module_name_repetitions)]
pub struct SegmentFileTrailer {
    pub(crate) metadata: Metadata,
    pub(crate) offsets: FileOffsets,
}

impl SegmentFileTrailer {
    pub fn from_file<P: AsRef<Path>>(path: P) -> crate::Result<Self> {
        let file = File::open(path)?;
        let mut reader = BufReader::new(file);
        reader.seek(std::io::SeekFrom::End(-(TRAILER_SIZE as i64)))?;

        let metadata = Metadata::deserialize(&mut reader)?;

        let index_block_ptr = reader.read_u64::<BigEndian>()?;
        let tli_ptr = reader.read_u64::<BigEndian>()?;
        let bloom_ptr = reader.read_u64::<BigEndian>()?;
        let range_tombstone_ptr = reader.read_u64::<BigEndian>()?;

        Ok(Self {
            metadata,
            offsets: FileOffsets {
                index_block_ptr,
                tli_ptr,
                bloom_ptr,
                range_tombstone_ptr,
            },
        })
    }
}

impl Serializable for SegmentFileTrailer {
    fn serialize<W: Write>(&self, writer: &mut W) -> Result<(), SerializeError> {
        let mut v = Vec::with_capacity(TRAILER_SIZE);

        self.metadata.serialize(&mut v)?;

        v.write_u64::<BigEndian>(self.offsets.index_block_ptr)?;
        v.write_u64::<BigEndian>(self.offsets.tli_ptr)?;
        v.write_u64::<BigEndian>(self.offsets.bloom_ptr)?;
        v.write_u64::<BigEndian>(self.offsets.range_tombstone_ptr)?;

        v.resize(TRAILER_SIZE - TRAILER_MAGIC.len(), 0);

        v.write_all(TRAILER_MAGIC)?;

        debug_assert_eq!(v.len(), TRAILER_SIZE);

        writer.write_all(&v)?;

        Ok(())
    }
}
