use std::io::{self, Read, Write};

use openxet_hashing::MerkleHash;

// ─── Constants ───────────────────────────────────────────────────────────────

/// Magic tag at the start of every shard file.
/// Bytes: "HFRepoMetaData\0" + sentinel bytes.
pub const MDB_SHARD_HEADER_TAG: [u8; 32] = [
    b'H', b'F', b'R', b'e', b'p', b'o', b'M', b'e', b't', b'a', b'D', b'a', b't', b'a', 0, 85, 105,
    103, 69, 106, 123, 129, 87, 131, 165, 189, 217, 92, 205, 209, 74, 169,
];

pub const MDB_SHARD_HEADER_VERSION: u64 = 2;
pub const MDB_SHARD_FOOTER_VERSION: u64 = 1;

/// All entry structs are 48 bytes.
pub const ENTRY_SIZE: usize = 48;

/// File info flag: verification entries are present.
pub const MDB_FILE_FLAG_WITH_VERIFICATION: u32 = 0x8000_0000;

/// File info flag: metadata extension is present.
pub const MDB_FILE_FLAG_WITH_METADATA_EXT: u32 = 0x4000_0000;

/// Header size.
pub const HEADER_SIZE: usize = 48;

/// Footer size.
pub const FOOTER_SIZE: usize = 200;

/// Maximum shard size: 64 MiB.
pub const MAX_SHARD_SIZE: usize = 64 * 1024 * 1024;

/// Bookend marker: 32 bytes of 0xFF followed by 16 bytes of 0x00.
const BOOKEND_HASH: [u8; 32] = [0xFF; 32];

// ─── Header ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ShardHeader {
    pub tag: [u8; 32],
    pub version: u64,
    pub footer_size: u64,
}

impl ShardHeader {
    pub fn new(footer_size: u64) -> Self {
        Self {
            tag: MDB_SHARD_HEADER_TAG,
            version: MDB_SHARD_HEADER_VERSION,
            footer_size,
        }
    }

    pub fn read_from(reader: &mut impl Read) -> Result<Self, ShardError> {
        let mut tag = [0u8; 32];
        reader.read_exact(&mut tag)?;
        if tag != MDB_SHARD_HEADER_TAG {
            return Err(ShardError::InvalidMagic);
        }

        let version = read_u64_le(reader)?;
        if version != MDB_SHARD_HEADER_VERSION {
            return Err(ShardError::UnsupportedVersion {
                section: "header",
                expected: MDB_SHARD_HEADER_VERSION,
                actual: version,
            });
        }

        let footer_size = read_u64_le(reader)?;

        Ok(Self {
            tag,
            version,
            footer_size,
        })
    }

    pub fn write_to(&self, writer: &mut impl Write) -> Result<(), io::Error> {
        writer.write_all(&self.tag)?;
        writer.write_all(&self.version.to_le_bytes())?;
        writer.write_all(&self.footer_size.to_le_bytes())?;
        Ok(())
    }
}

// ─── File Info Section ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct FileDataSequenceHeader {
    pub file_hash: MerkleHash,
    pub file_flags: u32,
    pub num_entries: u32,
}

impl FileDataSequenceHeader {
    pub fn read_from(reader: &mut impl Read) -> Result<Option<Self>, ShardError> {
        let mut buf = [0u8; ENTRY_SIZE];
        reader.read_exact(&mut buf)?;

        // Check for bookend: first 32 bytes all 0xFF
        if buf[..32] == BOOKEND_HASH {
            return Ok(None);
        }

        let file_hash = MerkleHash::from_bytes(buf[..32].try_into().unwrap());
        let file_flags = u32::from_le_bytes(buf[32..36].try_into().unwrap());
        let num_entries = u32::from_le_bytes(buf[36..40].try_into().unwrap());
        // bytes 40..48 are unused

        Ok(Some(Self {
            file_hash,
            file_flags,
            num_entries,
        }))
    }

    pub fn write_to(&self, writer: &mut impl Write) -> Result<(), io::Error> {
        writer.write_all(self.file_hash.as_bytes())?;
        writer.write_all(&self.file_flags.to_le_bytes())?;
        writer.write_all(&self.num_entries.to_le_bytes())?;
        writer.write_all(&[0u8; 8])?; // unused
        Ok(())
    }

    pub fn has_verification(&self) -> bool {
        self.file_flags & MDB_FILE_FLAG_WITH_VERIFICATION != 0
    }

    pub fn has_metadata_ext(&self) -> bool {
        self.file_flags & MDB_FILE_FLAG_WITH_METADATA_EXT != 0
    }
}

#[derive(Debug, Clone)]
pub struct FileDataSequenceEntry {
    /// Xorb hash for this term.
    pub cas_hash: MerkleHash,
    pub cas_flags: u32,
    /// Unpacked (decompressed) bytes for this term's chunk range.
    pub unpacked_segment_bytes: u32,
    /// Start chunk index (inclusive).
    pub chunk_index_start: u32,
    /// End chunk index (exclusive).
    pub chunk_index_end: u32,
}

impl FileDataSequenceEntry {
    pub fn read_from(reader: &mut impl Read) -> Result<Self, ShardError> {
        let mut buf = [0u8; ENTRY_SIZE];
        reader.read_exact(&mut buf)?;

        Ok(Self {
            cas_hash: MerkleHash::from_bytes(buf[..32].try_into().unwrap()),
            cas_flags: u32::from_le_bytes(buf[32..36].try_into().unwrap()),
            unpacked_segment_bytes: u32::from_le_bytes(buf[36..40].try_into().unwrap()),
            chunk_index_start: u32::from_le_bytes(buf[40..44].try_into().unwrap()),
            chunk_index_end: u32::from_le_bytes(buf[44..48].try_into().unwrap()),
        })
    }

    pub fn write_to(&self, writer: &mut impl Write) -> Result<(), io::Error> {
        writer.write_all(self.cas_hash.as_bytes())?;
        writer.write_all(&self.cas_flags.to_le_bytes())?;
        writer.write_all(&self.unpacked_segment_bytes.to_le_bytes())?;
        writer.write_all(&self.chunk_index_start.to_le_bytes())?;
        writer.write_all(&self.chunk_index_end.to_le_bytes())?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct FileVerificationEntry {
    pub range_hash: MerkleHash,
}

impl FileVerificationEntry {
    pub fn read_from(reader: &mut impl Read) -> Result<Self, ShardError> {
        let mut buf = [0u8; ENTRY_SIZE];
        reader.read_exact(&mut buf)?;

        Ok(Self {
            range_hash: MerkleHash::from_bytes(buf[..32].try_into().unwrap()),
            // bytes 32..48 are unused
        })
    }

    pub fn write_to(&self, writer: &mut impl Write) -> Result<(), io::Error> {
        writer.write_all(self.range_hash.as_bytes())?;
        writer.write_all(&[0u8; 16])?; // unused
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct FileMetadataExt {
    /// SHA-256 hash of the complete file contents.
    pub sha256: [u8; 32],
}

impl FileMetadataExt {
    pub fn read_from(reader: &mut impl Read) -> Result<Self, ShardError> {
        let mut buf = [0u8; ENTRY_SIZE];
        reader.read_exact(&mut buf)?;

        Ok(Self {
            sha256: buf[..32].try_into().unwrap(),
            // bytes 32..48 are unused
        })
    }

    pub fn write_to(&self, writer: &mut impl Write) -> Result<(), io::Error> {
        writer.write_all(&self.sha256)?;
        writer.write_all(&[0u8; 16])?; // unused
        Ok(())
    }
}

/// A complete file info block (one per file in the shard).
#[derive(Debug, Clone)]
pub struct FileInfoBlock {
    pub header: FileDataSequenceHeader,
    pub entries: Vec<FileDataSequenceEntry>,
    pub verification_entries: Vec<FileVerificationEntry>,
    pub metadata_ext: Option<FileMetadataExt>,
}

impl FileInfoBlock {
    /// Read a complete file info block. Returns None if bookend is encountered.
    pub fn read_from(reader: &mut impl Read) -> Result<Option<Self>, ShardError> {
        let header = match FileDataSequenceHeader::read_from(reader)? {
            Some(h) => h,
            None => return Ok(None), // bookend
        };

        let mut entries = Vec::with_capacity(header.num_entries as usize);
        for _ in 0..header.num_entries {
            entries.push(FileDataSequenceEntry::read_from(reader)?);
        }

        let mut verification_entries = Vec::new();
        if header.has_verification() {
            for _ in 0..header.num_entries {
                verification_entries.push(FileVerificationEntry::read_from(reader)?);
            }
        }

        let metadata_ext = if header.has_metadata_ext() {
            Some(FileMetadataExt::read_from(reader)?)
        } else {
            None
        };

        Ok(Some(Self {
            header,
            entries,
            verification_entries,
            metadata_ext,
        }))
    }

    pub fn write_to(&self, writer: &mut impl Write) -> Result<(), io::Error> {
        self.header.write_to(writer)?;
        for entry in &self.entries {
            entry.write_to(writer)?;
        }
        for ve in &self.verification_entries {
            ve.write_to(writer)?;
        }
        if let Some(ext) = &self.metadata_ext {
            ext.write_to(writer)?;
        }
        Ok(())
    }
}

// ─── CAS Info Section ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CASChunkSequenceHeader {
    /// Xorb hash.
    pub cas_hash: MerkleHash,
    pub cas_flags: u32,
    /// Number of chunk entries.
    pub num_entries: u32,
    /// Total size of all raw chunk bytes in this xorb.
    pub num_bytes_in_cas: u32,
    /// Length of the xorb after serialization (on disk).
    pub num_bytes_on_disk: u32,
}

impl CASChunkSequenceHeader {
    pub fn read_from(reader: &mut impl Read) -> Result<Option<Self>, ShardError> {
        let mut buf = [0u8; ENTRY_SIZE];
        reader.read_exact(&mut buf)?;

        // Check for bookend
        if buf[..32] == BOOKEND_HASH {
            return Ok(None);
        }

        Ok(Some(Self {
            cas_hash: MerkleHash::from_bytes(buf[..32].try_into().unwrap()),
            cas_flags: u32::from_le_bytes(buf[32..36].try_into().unwrap()),
            num_entries: u32::from_le_bytes(buf[36..40].try_into().unwrap()),
            num_bytes_in_cas: u32::from_le_bytes(buf[40..44].try_into().unwrap()),
            num_bytes_on_disk: u32::from_le_bytes(buf[44..48].try_into().unwrap()),
        }))
    }

    pub fn write_to(&self, writer: &mut impl Write) -> Result<(), io::Error> {
        writer.write_all(self.cas_hash.as_bytes())?;
        writer.write_all(&self.cas_flags.to_le_bytes())?;
        writer.write_all(&self.num_entries.to_le_bytes())?;
        writer.write_all(&self.num_bytes_in_cas.to_le_bytes())?;
        writer.write_all(&self.num_bytes_on_disk.to_le_bytes())?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct CASChunkSequenceEntry {
    pub chunk_hash: MerkleHash,
    pub chunk_byte_range_start: u32,
    pub unpacked_segment_bytes: u32,
}

impl CASChunkSequenceEntry {
    pub fn read_from(reader: &mut impl Read) -> Result<Self, ShardError> {
        let mut buf = [0u8; ENTRY_SIZE];
        reader.read_exact(&mut buf)?;

        Ok(Self {
            chunk_hash: MerkleHash::from_bytes(buf[..32].try_into().unwrap()),
            chunk_byte_range_start: u32::from_le_bytes(buf[32..36].try_into().unwrap()),
            unpacked_segment_bytes: u32::from_le_bytes(buf[36..40].try_into().unwrap()),
            // bytes 40..48 are unused
        })
    }

    pub fn write_to(&self, writer: &mut impl Write) -> Result<(), io::Error> {
        writer.write_all(self.chunk_hash.as_bytes())?;
        writer.write_all(&self.chunk_byte_range_start.to_le_bytes())?;
        writer.write_all(&self.unpacked_segment_bytes.to_le_bytes())?;
        writer.write_all(&[0u8; 8])?; // unused
        Ok(())
    }
}

/// A complete CAS info block (one per xorb in the shard).
#[derive(Debug, Clone)]
pub struct CASInfoBlock {
    pub header: CASChunkSequenceHeader,
    pub entries: Vec<CASChunkSequenceEntry>,
}

impl CASInfoBlock {
    /// Read a complete CAS info block. Returns None if bookend is encountered.
    pub fn read_from(reader: &mut impl Read) -> Result<Option<Self>, ShardError> {
        let header = match CASChunkSequenceHeader::read_from(reader)? {
            Some(h) => h,
            None => return Ok(None), // bookend
        };

        let mut entries = Vec::with_capacity(header.num_entries as usize);
        for _ in 0..header.num_entries {
            entries.push(CASChunkSequenceEntry::read_from(reader)?);
        }

        Ok(Some(Self { header, entries }))
    }

    pub fn write_to(&self, writer: &mut impl Write) -> Result<(), io::Error> {
        self.header.write_to(writer)?;
        for entry in &self.entries {
            entry.write_to(writer)?;
        }
        Ok(())
    }
}

// ─── Footer ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ShardFooter {
    pub version: u64,
    pub file_info_offset: u64,
    pub cas_info_offset: u64,
    pub chunk_hash_hmac_key: [u8; 32],
    pub shard_creation_timestamp: u64,
    pub shard_key_expiry: u64,
    pub footer_offset: u64,
}

impl ShardFooter {
    pub fn read_from(reader: &mut impl Read) -> Result<Self, ShardError> {
        let version = read_u64_le(reader)?;
        if version != MDB_SHARD_FOOTER_VERSION {
            return Err(ShardError::UnsupportedVersion {
                section: "footer",
                expected: MDB_SHARD_FOOTER_VERSION,
                actual: version,
            });
        }

        let file_info_offset = read_u64_le(reader)?;
        let cas_info_offset = read_u64_le(reader)?;

        // Skip 48 bytes reserved buffer
        let mut reserved = [0u8; 48];
        reader.read_exact(&mut reserved)?;

        let mut chunk_hash_hmac_key = [0u8; 32];
        reader.read_exact(&mut chunk_hash_hmac_key)?;

        let shard_creation_timestamp = read_u64_le(reader)?;
        let shard_key_expiry = read_u64_le(reader)?;

        // Skip 72 bytes reserved buffer
        let mut reserved2 = [0u8; 72];
        reader.read_exact(&mut reserved2)?;

        let footer_offset = read_u64_le(reader)?;

        Ok(Self {
            version,
            file_info_offset,
            cas_info_offset,
            chunk_hash_hmac_key,
            shard_creation_timestamp,
            shard_key_expiry,
            footer_offset,
        })
    }

    pub fn write_to(&self, writer: &mut impl Write) -> Result<(), io::Error> {
        writer.write_all(&self.version.to_le_bytes())?;
        writer.write_all(&self.file_info_offset.to_le_bytes())?;
        writer.write_all(&self.cas_info_offset.to_le_bytes())?;
        writer.write_all(&[0u8; 48])?; // reserved
        writer.write_all(&self.chunk_hash_hmac_key)?;
        writer.write_all(&self.shard_creation_timestamp.to_le_bytes())?;
        writer.write_all(&self.shard_key_expiry.to_le_bytes())?;
        writer.write_all(&[0u8; 72])?; // reserved
        writer.write_all(&self.footer_offset.to_le_bytes())?;
        Ok(())
    }

    pub fn has_hmac_key(&self) -> bool {
        self.chunk_hash_hmac_key != [0u8; 32]
    }
}

// ─── Complete Shard ──────────────────────────────────────────────────────────

/// A fully parsed shard.
#[derive(Debug, Clone)]
pub struct Shard {
    pub header: ShardHeader,
    pub file_info_blocks: Vec<FileInfoBlock>,
    pub cas_info_blocks: Vec<CASInfoBlock>,
    pub footer: Option<ShardFooter>,
}

impl Shard {
    /// Deserialize a complete shard from bytes (streaming / linear read).
    pub fn from_bytes(data: &[u8]) -> Result<Self, ShardError> {
        let mut reader = io::Cursor::new(data);

        let header = ShardHeader::read_from(&mut reader)?;

        // Read file info section
        let mut file_info_blocks = Vec::new();
        while let Some(block) = FileInfoBlock::read_from(&mut reader)? {
            file_info_blocks.push(block);
        }

        // Read CAS info section
        let mut cas_info_blocks = Vec::new();
        while let Some(block) = CASInfoBlock::read_from(&mut reader)? {
            cas_info_blocks.push(block);
        }

        // Read footer if present
        let footer = if header.footer_size > 0 {
            Some(ShardFooter::read_from(&mut reader)?)
        } else {
            None
        };

        Ok(Self {
            header,
            file_info_blocks,
            cas_info_blocks,
            footer,
        })
    }

    /// Serialize the shard to bytes.
    pub fn to_bytes(&self) -> Result<Vec<u8>, io::Error> {
        let mut buffer = Vec::new();
        self.header.write_to(&mut buffer)?;

        // File info section
        for block in &self.file_info_blocks {
            block.write_to(&mut buffer)?;
        }
        write_bookend(&mut buffer)?;

        // CAS info section
        for block in &self.cas_info_blocks {
            block.write_to(&mut buffer)?;
        }
        write_bookend(&mut buffer)?;

        // Footer (optional)
        if let Some(footer) = &self.footer {
            footer.write_to(&mut buffer)?;
        }

        Ok(buffer)
    }

    /// Serialize the shard for upload (footer omitted).
    pub fn to_upload_bytes(&self) -> Result<Vec<u8>, io::Error> {
        let mut buffer = Vec::new();

        let upload_header = ShardHeader::new(0); // footer_size = 0
        upload_header.write_to(&mut buffer)?;

        let file_info_offset = buffer.len() as u64;
        for block in &self.file_info_blocks {
            block.write_to(&mut buffer)?;
        }
        write_bookend(&mut buffer)?;

        let cas_info_offset = buffer.len() as u64;
        for block in &self.cas_info_blocks {
            block.write_to(&mut buffer)?;
        }
        write_bookend(&mut buffer)?;

        // No footer for upload
        let _ = (file_info_offset, cas_info_offset);

        Ok(buffer)
    }
}

/// Write a bookend entry (32 bytes 0xFF + 16 bytes 0x00).
fn write_bookend(writer: &mut impl Write) -> Result<(), io::Error> {
    writer.write_all(&BOOKEND_HASH)?;
    writer.write_all(&[0u8; 16])?;
    Ok(())
}

fn read_u64_le(reader: &mut impl Read) -> Result<u64, io::Error> {
    let mut buf = [0u8; 8];
    reader.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

#[derive(Debug, thiserror::Error)]
pub enum ShardError {
    #[error("invalid shard magic tag")]
    InvalidMagic,
    #[error("unsupported {section} version: expected {expected}, got {actual}")]
    UnsupportedVersion {
        section: &'static str,
        expected: u64,
        actual: u64,
    },
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("shard too large: {0} bytes (max {MAX_SHARD_SIZE})")]
    TooLarge(usize),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_header_roundtrip() {
        let header = ShardHeader::new(200);
        let mut buf = Vec::new();
        header.write_to(&mut buf).unwrap();
        assert_eq!(buf.len(), HEADER_SIZE);

        let mut reader = io::Cursor::new(&buf);
        let decoded = ShardHeader::read_from(&mut reader).unwrap();
        assert_eq!(decoded.version, MDB_SHARD_HEADER_VERSION);
        assert_eq!(decoded.footer_size, 200);
    }

    #[test]
    fn test_empty_shard_roundtrip() {
        let shard = Shard {
            header: ShardHeader::new(0),
            file_info_blocks: vec![],
            cas_info_blocks: vec![],
            footer: None,
        };

        let bytes = shard.to_bytes().unwrap();
        let decoded = Shard::from_bytes(&bytes).unwrap();

        assert_eq!(decoded.file_info_blocks.len(), 0);
        assert_eq!(decoded.cas_info_blocks.len(), 0);
        assert!(decoded.footer.is_none());
    }

    #[test]
    fn test_shard_with_file_info_roundtrip() {
        let file_hash = MerkleHash::from_bytes([1u8; 32]);
        let xorb_hash = MerkleHash::from_bytes([2u8; 32]);
        let verif_hash = MerkleHash::from_bytes([3u8; 32]);

        let shard = Shard {
            header: ShardHeader::new(0),
            file_info_blocks: vec![FileInfoBlock {
                header: FileDataSequenceHeader {
                    file_hash,
                    file_flags: MDB_FILE_FLAG_WITH_VERIFICATION | MDB_FILE_FLAG_WITH_METADATA_EXT,
                    num_entries: 1,
                },
                entries: vec![FileDataSequenceEntry {
                    cas_hash: xorb_hash,
                    cas_flags: 0,
                    unpacked_segment_bytes: 65536,
                    chunk_index_start: 0,
                    chunk_index_end: 10,
                }],
                verification_entries: vec![FileVerificationEntry {
                    range_hash: verif_hash,
                }],
                metadata_ext: Some(FileMetadataExt { sha256: [42u8; 32] }),
            }],
            cas_info_blocks: vec![CASInfoBlock {
                header: CASChunkSequenceHeader {
                    cas_hash: xorb_hash,
                    cas_flags: 0,
                    num_entries: 1,
                    num_bytes_in_cas: 65536,
                    num_bytes_on_disk: 50000,
                },
                entries: vec![CASChunkSequenceEntry {
                    chunk_hash: MerkleHash::from_bytes([4u8; 32]),
                    chunk_byte_range_start: 0,
                    unpacked_segment_bytes: 65536,
                }],
            }],
            footer: None,
        };

        let bytes = shard.to_bytes().unwrap();
        let decoded = Shard::from_bytes(&bytes).unwrap();

        assert_eq!(decoded.file_info_blocks.len(), 1);
        assert_eq!(decoded.file_info_blocks[0].entries.len(), 1);
        assert_eq!(decoded.file_info_blocks[0].verification_entries.len(), 1);
        assert!(decoded.file_info_blocks[0].metadata_ext.is_some());
        assert_eq!(decoded.cas_info_blocks.len(), 1);
        assert_eq!(decoded.cas_info_blocks[0].entries.len(), 1);
    }

    #[test]
    fn test_footer_roundtrip() {
        let footer = ShardFooter {
            version: MDB_SHARD_FOOTER_VERSION,
            file_info_offset: 48,
            cas_info_offset: 200,
            chunk_hash_hmac_key: [0u8; 32],
            shard_creation_timestamp: 1700000000,
            shard_key_expiry: 1700600000,
            footer_offset: 400,
        };

        let mut buf = Vec::new();
        footer.write_to(&mut buf).unwrap();
        assert_eq!(buf.len(), FOOTER_SIZE);

        let mut reader = io::Cursor::new(&buf);
        let decoded = ShardFooter::read_from(&mut reader).unwrap();
        assert_eq!(decoded.file_info_offset, 48);
        assert_eq!(decoded.cas_info_offset, 200);
        assert_eq!(decoded.footer_offset, 400);
        assert!(!decoded.has_hmac_key());
    }

    #[test]
    fn test_shard_with_footer_roundtrip() {
        let shard = Shard {
            header: ShardHeader::new(FOOTER_SIZE as u64),
            file_info_blocks: vec![],
            cas_info_blocks: vec![],
            footer: Some(ShardFooter {
                version: MDB_SHARD_FOOTER_VERSION,
                file_info_offset: HEADER_SIZE as u64,
                cas_info_offset: HEADER_SIZE as u64 + ENTRY_SIZE as u64,
                chunk_hash_hmac_key: [7u8; 32],
                shard_creation_timestamp: 1700000000,
                shard_key_expiry: 1700600000,
                footer_offset: HEADER_SIZE as u64 + 2 * ENTRY_SIZE as u64,
            }),
        };

        let bytes = shard.to_bytes().unwrap();
        let decoded = Shard::from_bytes(&bytes).unwrap();

        let footer = decoded.footer.unwrap();
        assert!(footer.has_hmac_key());
        assert_eq!(footer.chunk_hash_hmac_key, [7u8; 32]);
    }

    #[test]
    fn test_upload_bytes_no_footer() {
        let shard = Shard {
            header: ShardHeader::new(FOOTER_SIZE as u64),
            file_info_blocks: vec![],
            cas_info_blocks: vec![],
            footer: Some(ShardFooter {
                version: MDB_SHARD_FOOTER_VERSION,
                file_info_offset: 48,
                cas_info_offset: 96,
                chunk_hash_hmac_key: [0u8; 32],
                shard_creation_timestamp: 0,
                shard_key_expiry: 0,
                footer_offset: 144,
            }),
        };

        let upload = shard.to_upload_bytes().unwrap();
        let decoded = Shard::from_bytes(&upload).unwrap();

        // Footer size should be 0 in upload format
        assert_eq!(decoded.header.footer_size, 0);
        assert!(decoded.footer.is_none());
    }
}
