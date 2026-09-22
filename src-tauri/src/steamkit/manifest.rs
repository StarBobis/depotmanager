//! Steam depot manifest (v5 protobuf format) parsing, serialization and
//! filename decryption.

use anyhow::{bail, Context};
use prost::Message;

use super::crypto;
use super::proto_gen::*;

pub const PAYLOAD_MAGIC: u32 = 0x71F6_17D0;
pub const METADATA_MAGIC: u32 = 0x1F48_12BE;
pub const SIGNATURE_MAGIC: u32 = 0x1B81_B817;
pub const END_MAGIC: u32 = 0x32C4_15AB;

pub mod file_flags {
    pub const USER_CONFIG: u32 = 1;
    pub const VERSIONED_USER_CONFIG: u32 = 2;
    pub const ENCRYPTED: u32 = 4;
    pub const READ_ONLY: u32 = 8;
    pub const HIDDEN: u32 = 16;
    pub const EXECUTABLE: u32 = 32;
    pub const DIRECTORY: u32 = 64;
    pub const CUSTOM_EXECUTABLE: u32 = 128;
    pub const INSTALL_SCRIPT: u32 = 256;
    pub const SYMLINK: u32 = 512;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkData {
    /// 20-byte SHA-1 chunk id.
    pub id: Vec<u8>,
    /// Expected adler-32 of the uncompressed data.
    pub checksum: u32,
    pub offset: u64,
    pub compressed_len: u32,
    pub uncompressed_len: u32,
}

impl ChunkData {
    pub fn id_hex(&self) -> String {
        hex::encode(&self.id)
    }
}

#[derive(Debug, Clone)]
pub struct FileData {
    pub name: String,
    pub name_hash: Vec<u8>,
    pub chunks: Vec<ChunkData>,
    pub flags: u32,
    pub size: u64,
    pub hash: Vec<u8>,
    pub link_target: Option<String>,
}

impl FileData {
    pub fn is_directory(&self) -> bool {
        self.flags & file_flags::DIRECTORY != 0
    }
}

#[derive(Debug, Clone)]
pub struct DepotManifest {
    pub files: Vec<FileData>,
    pub filenames_encrypted: bool,
    pub depot_id: u32,
    pub manifest_gid: u64,
    pub creation_time: u32,
    pub total_uncompressed: u64,
    pub total_compressed: u64,
}

impl DepotManifest {
    /// Parses the binary manifest format (magic + len + protobuf sections).
    pub fn parse(data: &[u8]) -> anyhow::Result<Self> {
        let mut payload: Option<ContentManifestPayload> = None;
        let mut metadata: Option<ContentManifestMetadata> = None;

        let mut pos = 0usize;
        loop {
            if pos + 4 > data.len() {
                bail!("manifest truncated at offset {}", pos);
            }
            let magic = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
            pos += 4;
            if magic == END_MAGIC {
                break;
            }
            if pos + 4 > data.len() {
                bail!("manifest truncated (section length) at offset {}", pos);
            }
            let len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
            pos += 4;
            if pos + len > data.len() {
                bail!("manifest section overruns buffer");
            }
            let section = &data[pos..pos + len];
            pos += len;

            match magic {
                PAYLOAD_MAGIC => {
                    payload = Some(ContentManifestPayload::decode(section).context("payload decode")?);
                }
                METADATA_MAGIC => {
                    metadata = Some(ContentManifestMetadata::decode(section).context("metadata decode")?);
                }
                SIGNATURE_MAGIC => { /* signature not needed */ }
                other => bail!("unrecognized manifest section magic {:#x}", other),
            }
        }

        let payload = payload.context("manifest missing payload section")?;
        let metadata = metadata.context("manifest missing metadata section")?;

        let files = payload
            .mappings
            .into_iter()
            .map(|m| FileData {
                name: m.filename.unwrap_or_default(),
                name_hash: m.sha_filename.unwrap_or_default(),
                chunks: m
                    .chunks
                    .into_iter()
                    .map(|c| ChunkData {
                        id: c.sha.unwrap_or_default(),
                        checksum: c.crc.unwrap_or(0),
                        offset: c.offset.unwrap_or(0),
                        compressed_len: c.cb_compressed.unwrap_or(0),
                        uncompressed_len: c.cb_original.unwrap_or(0),
                    })
                    .collect(),
                flags: m.flags.unwrap_or(0),
                size: m.size.unwrap_or(0),
                hash: m.sha_content.unwrap_or_default(),
                link_target: m.linktarget.filter(|s| !s.is_empty()),
            })
            .collect();

        Ok(DepotManifest {
            files,
            filenames_encrypted: metadata.filenames_encrypted.unwrap_or(false),
            depot_id: metadata.depot_id.unwrap_or(0),
            manifest_gid: metadata.gid_manifest.unwrap_or(0),
            creation_time: metadata.creation_time.unwrap_or(0),
            total_uncompressed: metadata.cb_disk_original.unwrap_or(0),
            total_compressed: metadata.cb_disk_compressed.unwrap_or(0),
        })
    }

    /// Decrypts filenames in-place using the depot key.
    pub fn decrypt_filenames(&mut self, depot_key: &[u8]) -> anyhow::Result<()> {
        if !self.filenames_encrypted {
            return Ok(());
        }
        for file in &mut self.files {
            file.name = crypto::decrypt_symmetric_name(&file.name, depot_key)
                .context("解密文件名失败")?;
            if let Some(link) = &file.link_target {
                file.link_target = Some(crypto::decrypt_symmetric_name(link, depot_key)?);
            }
        }
        // Steam sorts alphabetically after decryption.
        self.files
            .sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        self.filenames_encrypted = false;
        Ok(())
    }

    /// Serializes back to the Steam binary manifest format (for local cache).
    pub fn serialize(&self) -> Vec<u8> {
        let payload = ContentManifestPayload {
            mappings: self
                .files
                .iter()
                .map(|f| content_manifest_payload::FileMapping {
                    filename: Some(f.name.clone()),
                    size: Some(f.size),
                    flags: Some(f.flags),
                    sha_filename: Some(f.name_hash.clone()),
                    sha_content: Some(f.hash.clone()),
                    chunks: f
                        .chunks
                        .iter()
                        .map(|c| content_manifest_payload::file_mapping::ChunkData {
                            sha: Some(c.id.clone()),
                            crc: Some(c.checksum),
                            offset: Some(c.offset),
                            cb_original: Some(c.uncompressed_len),
                            cb_compressed: Some(c.compressed_len),
                        })
                        .collect(),
                    linktarget: f.link_target.clone(),
                })
                .collect(),
        };
        let metadata = ContentManifestMetadata {
            depot_id: Some(self.depot_id),
            gid_manifest: Some(self.manifest_gid),
            creation_time: Some(self.creation_time),
            filenames_encrypted: Some(self.filenames_encrypted),
            cb_disk_original: Some(self.total_uncompressed),
            cb_disk_compressed: Some(self.total_compressed),
            unique_chunks: None,
            crc_encrypted: None,
            crc_clear: None,
        };

        let payload_bytes = payload.encode_to_vec();
        let metadata_bytes = metadata.encode_to_vec();

        let mut out = Vec::with_capacity(payload_bytes.len() + metadata_bytes.len() + 24);
        out.extend_from_slice(&PAYLOAD_MAGIC.to_le_bytes());
        out.extend_from_slice(&(payload_bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&payload_bytes);
        out.extend_from_slice(&METADATA_MAGIC.to_le_bytes());
        out.extend_from_slice(&(metadata_bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&metadata_bytes);
        out.extend_from_slice(&SIGNATURE_MAGIC.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&END_MAGIC.to_le_bytes());
        out
    }
}
