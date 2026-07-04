use bytebuffer::{ByteBuffer, Endian};
use derive_getters::Getters;
use encoding::{all::WINDOWS_1252, DecoderTrap, Encoding};
use log::{debug, warn};
use reqwest::header::ToStrError;
use reqwest::Client;
use std::cmp;
use std::string::FromUtf8Error;
use thiserror::Error;

/// This module is based on https://users.cs.jmu.edu/buchhofp/forensics/formats/pkzip.html

const ZIP_EOCD_SIGNATURE: u32 = 0x06054b50;

const ZIP64_EOCD_SIGNATURE: u32 = 0x06064b50;
const ZIP64_EOCD_LOCATOR_SIGNATURE: u32 = 0x07064b50;
const ZIP64_SIGNATURE: i64 = 0xFFFFFFFF;

const ZIP_EOCD_FIXED_PART_SIZE: u32 = 22;
const ZIP64_EOCD_FIXED_PART_SIZE: u32 = 56;

const ZIP_FILE_HEADER_SIGNATURE: u32 = 0x02014b50;

const MAX_BACKSCAN_OFFSET: usize = 6 * 1024 * 1024;

#[derive(Error, Debug)]
pub enum EOCDError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("not enough space for end of central directory to be read, {required} is required, {0} is available", required = ZIP_EOCD_FIXED_PART_SIZE)]
    NotEnoughSpace(usize),
    #[error("invalid signature `{0:#10x}` (expected `{sig:#10x}`)", sig = ZIP_EOCD_SIGNATURE)]
    Signature(u32),
}

#[derive(Error, Debug)]
pub enum EntryError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Utf8(#[from] FromUtf8Error),

    #[error("failed to decode file name")]
    Decode,
    #[error("invalid signature `{0:#10x}` (expected `{sig:#10x}`)", sig = ZIP_FILE_HEADER_SIGNATURE)]
    Signature(u32),
}

#[derive(Error, Debug)]
pub enum ZipError {
    #[error(transparent)]
    Request(#[from] reqwest::Error),
    #[error(transparent)]
    ToStr(#[from] ToStrError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Utf8(#[from] FromUtf8Error),
    #[error(transparent)]
    Eocd(#[from] EOCDError),

    #[error("content-length > 8192")]
    ContentTooLong,
    #[error("failed to load central directory entry {idx} (out of {total}): {err}")]
    CentralDirectory { idx: u64, total: u64, err: String },
    #[error("failed to read end of central directory")]
    CentralDirectoryEndGeneric,
    #[error("failed to decode file name")]
    Decode,
    #[error("failed to find extra field {id} for zip entry {name}")]
    ExtraField { id: u16, name: String },
    #[error("not enough space for end of central directory to be read, {required} is required, {0} is available", required = ZIP_EOCD_FIXED_PART_SIZE)]
    NotEnoughSpace(usize),
    #[error("no content length found in response")]
    NoContentLength,
    #[error("requested read was too big (attempted {attempted}, limit is {max}")]
    ReadTooBig { attempted: i64, max: i64 },
    #[error("invalid signature {0:#10x}")]
    Signature(u32),
}

fn signature_scan_rev(data: &[u8], signature: u32) -> Option<usize> {
    let signature_bytes = signature.to_le_bytes();
    let signature_len = signature_bytes.len();

    for (i, window) in data.windows(signature_len).enumerate().rev() {
        if window != signature_bytes {
            continue;
        }

        return Some(i);
    }

    None
}

#[derive(Default, Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum CompressionType {
    #[default]
    None = 0,
    Deflate = 8,
}

impl CompressionType {
    pub fn from_num(num: u16) -> CompressionType {
        match num {
            8 => CompressionType::Deflate,
            0 | _ => CompressionType::None,
        }
    }
}

#[derive(Default, Debug, Clone, Getters, serde::Serialize, serde::Deserialize)]
pub struct ZipFileEntry {
    name: String,
    crc32: u32,
    compression_type: CompressionType,
    compressed_size: i64,
    uncompressed_size: i64,
    disk_number_start: u16,
    local_header_offset: i64,
    data_offset: i64,

    #[getter(skip)]
    extra_field: Vec<u8>,
}

impl ZipFileEntry {
    pub fn parse(data: &mut ByteBuffer) -> Result<ZipFileEntry, EntryError> {
        let mut entry = Self::default();

        let signature = data.read_u32()?;
        if signature != ZIP_FILE_HEADER_SIGNATURE {
            return Err(EntryError::Signature(signature));
        }

        data.read_u16()?; // Version
        data.read_u16()?; // Vers. needed
        let flags = data.read_u16()?;
        let use_utf8 = flags & (1 << 11) != 0;

        entry.compression_type = CompressionType::from_num(data.read_u16()?);

        data.read_u16()?; // Modified time
        data.read_u16()?; // Modified date

        entry.crc32 = data.read_u32()?;
        entry.compressed_size = data.read_u32()? as i64;
        entry.uncompressed_size = data.read_u32()? as i64;

        let file_name_len = data.read_u16()?;
        let extra_field_len = data.read_u16()?;
        let file_comment_len = data.read_u16()?;

        entry.disk_number_start = data.read_u16()?;

        data.read_u16()?; // Internal attr.
        data.read_u32()?; // External attr.

        entry.local_header_offset = data.read_u32()? as i64;

        let name_bytes = data.read_bytes(file_name_len as usize)?;
        entry.name = if use_utf8 {
            debug!("Using UTF-8...");
            String::from_utf8(name_bytes)?
        } else {
            match WINDOWS_1252.decode(&name_bytes, DecoderTrap::Strict) {
                Ok(s) => s,
                Err(_) => return Err(EntryError::Decode),
            }
        };
        entry.extra_field = data.read_bytes(extra_field_len as usize)?;

        if let Ok(data) = entry.extra_field(0x01) {
            let mut data = ByteBuffer::from_vec(data);
            data.set_endian(Endian::LittleEndian);

            if entry.uncompressed_size == 0xFFFFFFFF {
                entry.uncompressed_size = data.read_i64()?;
            }

            if entry.compressed_size == 0xFFFFFFFF {
                entry.compressed_size = data.read_i64()?;
            }

            if entry.local_header_offset == 0xFFFFFFFF {
                entry.local_header_offset = data.read_i64()?;
            }
        }

        data.set_rpos(data.get_rpos() + file_comment_len as usize);

        Ok(entry)
    }

    fn extra_field(&self, id: u16) -> Result<Vec<u8>, ZipError> {
        let mut data = ByteBuffer::from_vec(self.extra_field.clone());
        data.set_endian(Endian::LittleEndian);

        loop {
            let id2 = data.read_u16()?;
            let size = data.read_u16()? as usize;

            if id == id2 {
                if data.len() - data.get_rpos() >= size {
                    return Ok(data.read_bytes(size)?);
                }

                break;
            }

            data.set_rpos(size);

            if data.len() - data.get_rpos() > 0 {
                break;
            }
        }

        Err(ZipError::ExtraField {
            id,
            name: self.name.clone(),
        })
    }
}

#[derive(Default, Getters, serde::Serialize, serde::Deserialize)]
pub struct ZipFile {
    entries: Vec<ZipFileEntry>,
}

/// On-disk cache location for a fetched manifest. Keyed by the URL *path*
/// only — the `sauth` query token rotates per session while the path
/// uniquely (and immutably) names the build artifact.
fn manifest_cache_path(url: &str) -> Option<std::path::PathBuf> {
    let path_part = url.split('?').next().unwrap_or(url);
    let hash = crate::util::hash::hash_fnv1a(path_part.as_bytes());
    crate::util::native::maxima_dir()
        .ok()
        .map(|d| d.join("cache/manifests").join(format!("{hash:016x}.json")))
}

#[derive(Default)]
struct EndOfCentralDirectory {
    pub disk_number: u32,
    pub disk_number_with_cd: u32,
    pub disk_entries: u64,
    pub total_entries: u64,
    pub cd_size: u64,
    pub cd_offset: i64,
    pub comment_length: u16,
}

impl EndOfCentralDirectory {
    pub fn parse(&mut self, data: &mut ByteBuffer) -> Result<(), EOCDError> {
        if data.len() - data.get_rpos() < ZIP_EOCD_FIXED_PART_SIZE as usize {
            return Err(EOCDError::NotEnoughSpace(data.len() - data.get_rpos()));
        }

        let signature = data.read_u32()?;
        if signature as u32 != ZIP_EOCD_SIGNATURE {
            return Err(EOCDError::Signature(signature));
        }

        self.disk_number = data.read_u16()? as u32;
        self.disk_number_with_cd = data.read_u16()? as u32;
        self.disk_entries = data.read_u16()? as u64;
        self.total_entries = data.read_u16()? as u64;
        self.cd_size = data.read_u32()? as u64;
        self.cd_offset = data.read_u32()? as i64;
        self.comment_length = data.read_u16()?;

        // Discard the comment
        data.set_rpos(data.get_rpos() + self.comment_length as usize);

        Ok(())
    }

    pub fn parse64(&mut self, data: &mut ByteBuffer) -> Result<(), ZipError> {
        if data.len() - data.get_rpos() < ZIP64_EOCD_FIXED_PART_SIZE as usize {
            return Err(ZipError::NotEnoughSpace(data.len() - data.get_rpos()));
        }

        let signature = data.read_u32()?;
        if signature as u32 != ZIP64_EOCD_SIGNATURE {
            return Err(ZipError::Signature(signature));
        }

        let size_of_record = data.read_i64()?;
        if size_of_record < (ZIP64_EOCD_FIXED_PART_SIZE - 12) as i64 {
            return Ok(());
        }

        data.read_u16()?;
        data.read_u16()?;

        self.disk_number = data.read_u32()? as u32;
        self.disk_number_with_cd = data.read_u32()? as u32;
        self.disk_entries = data.read_i64()? as u64;
        self.total_entries = data.read_i64()? as u64;
        self.cd_size = data.read_i64()? as u64;
        self.cd_offset = data.read_i64()? as i64;
        self.comment_length = 0;

        Ok(())
    }
}

impl ZipFile {
    pub async fn fetch(url: &str) -> Result<Self, ZipError> {
        // Disk cache first: fetching the central directory from EA's CDN is
        // the flakiest step of an install on slow routes, and the artifact
        // is immutable per build — pay for it once.
        let cache_path = manifest_cache_path(url);
        if let Some(p) = &cache_path {
            if let Ok(bytes) = tokio::fs::read(p).await {
                if let Ok(zip) = serde_json::from_slice::<ZipFile>(&bytes) {
                    log::info!(
                        "Using cached build manifest ({} entries) from {}",
                        zip.entries.len(),
                        p.display()
                    );
                    return Ok(zip);
                }
                // Corrupt/stale cache — fall through to a network fetch,
                // which overwrites it on success.
            }
        }

        // Bounded connect, then STREAMED bodies with a progress-based stall
        // guard — same semantics as the file downloader. A total per-request
        // cap (tried first: 120s) kept expiring on real-world slow-but-alive
        // CDN routes, while dead connections still need killing fast. Kill
        // only on "no data for STALL_TIMEOUT", tolerate any transfer speed.
        const HEADER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
        const STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
        const ATTEMPTS: u32 = 4;

        let client = Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .build()?;

        fn stall(what: &str) -> ZipError {
            ZipError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("manifest fetch stalled ({what})"),
            ))
        }

        // Retry with backoff — EA's CDN route drops/stalls transiently, and
        // failing the whole install queue over one flaky request just makes
        // the user rerun by hand.
        async fn with_retry<T, F, Fut>(what: &str, f: F) -> Result<T, ZipError>
        where
            F: Fn() -> Fut,
            Fut: std::future::Future<Output = Result<T, ZipError>>,
        {
            let mut last_err = None;
            for attempt in 0..ATTEMPTS {
                match f().await {
                    Ok(v) => return Ok(v),
                    Err(err) => {
                        log::warn!(
                            "manifest {} attempt {}/{} failed: {}",
                            what,
                            attempt + 1,
                            ATTEMPTS,
                            err
                        );
                        last_err = Some(err);
                        if attempt + 1 < ATTEMPTS {
                            tokio::time::sleep(std::time::Duration::from_secs(
                                2u64 << attempt,
                            ))
                            .await;
                        }
                    }
                }
            }
            Err(last_err.expect("at least one attempt"))
        }

        let response = with_retry("HEAD", || async {
            Ok(
                tokio::time::timeout(HEADER_TIMEOUT, client.head(url).send())
                    .await
                    .map_err(|_| stall("HEAD: no response"))??,
            )
        })
        .await?;
        let content_length = response
            .headers()
            .get("content-length")
            .ok_or(ZipError::NoContentLength)?
            .to_str()?
            .parse::<i64>()
            .unwrap_or(0);

        let mut data: Vec<u8> = Vec::with_capacity(MAX_BACKSCAN_OFFSET);
        let mut offset = content_length - 8 * 1024;
        if offset < 0 {
            return Err(ZipError::ContentTooLong);
        }

        let mut zip = Self::default();

        while offset > 0 && data.len() < MAX_BACKSCAN_OFFSET {
            let read = content_length - offset - data.len() as i64;
            let start_offset = content_length - data.len() as i64 - read;
            let end_offset = start_offset + read;

            let range_header = format!("bytes={}-{}", start_offset, end_offset - 1);
            let this_data = with_retry("range read", || async {
                let mut response = tokio::time::timeout(
                    HEADER_TIMEOUT,
                    client.get(url).header("range", &range_header).send(),
                )
                .await
                .map_err(|_| stall("no response headers"))??;

                // Stream the body; abort only when no bytes arrive for
                // STALL_TIMEOUT (any transfer speed is acceptable).
                let mut buf: Vec<u8> = Vec::new();
                loop {
                    match tokio::time::timeout(STALL_TIMEOUT, response.chunk()).await {
                        Ok(Ok(Some(chunk))) => buf.extend_from_slice(&chunk),
                        Ok(Ok(None)) => break,
                        Ok(Err(err)) => return Err(err.into()),
                        Err(_) => return Err(stall("no data mid-body")),
                    }
                }
                Ok(buf)
            })
            .await?;
            data = [this_data, data].concat();

            offset = zip.load(&mut ByteBuffer::from_vec(data.clone()), content_length)?;
            if offset > content_length {
                return Err(ZipError::ReadTooBig {
                    attempted: offset,
                    max: content_length,
                });
            }
        }

        // Best-effort cache write — a failure here costs a re-fetch next
        // time, nothing more.
        if let Some(p) = &cache_path {
            if let Ok(bytes) = serde_json::to_vec(&zip) {
                if let Some(parent) = p.parent() {
                    let _ = tokio::fs::create_dir_all(parent).await;
                }
                match tokio::fs::write(p, bytes).await {
                    Ok(()) => log::info!("Cached build manifest at {}", p.display()),
                    Err(err) => warn!("Could not cache manifest: {}", err),
                }
            }
        }

        Ok(zip)
    }

    fn load(&mut self, data: &mut ByteBuffer, total_size: i64) -> Result<i64, ZipError> {
        data.set_endian(Endian::LittleEndian);

        if let Some(pos) = signature_scan_rev(data.as_bytes(), ZIP_EOCD_SIGNATURE) {
            data.set_rpos(pos);
        } else {
            let amt = cmp::min(total_size - data.len() as i64, 1024);
            return Ok(total_size - data.len() as i64 - amt);
        }

        let mut eocd = EndOfCentralDirectory::default();
        eocd.parse(data)?;

        // Check if we're Zip64
        if eocd.cd_offset == ZIP64_SIGNATURE {
            if let Some(pos) = signature_scan_rev(data.as_bytes(), ZIP64_EOCD_LOCATOR_SIGNATURE) {
                data.set_rpos(pos);
            } else {
                let amt = cmp::min(total_size - data.len() as i64, 1024);
                return Ok(total_size - data.len() as i64 - amt);
            }

            let signature = data.read_u32()?;
            if signature != ZIP64_EOCD_LOCATOR_SIGNATURE {
                return Err(ZipError::Signature(signature));
            }

            data.read_u32()?; // Disk that contains EOCD
            let zip64_eocd_offset = data.read_i64()?;
            data.read_u32()?; // Disk count

            let pos2 = zip64_eocd_offset - (total_size - data.len() as i64);
            if pos2 < 0 {
                return Ok(zip64_eocd_offset);
            }

            data.set_rpos(pos2 as usize);
            if eocd.parse64(data).is_err() {
                warn!("Failed to read ZIP64 end of central directory");
                eocd.cd_offset = 0;
            }
        }

        if eocd.cd_offset < 0 || eocd.cd_offset == ZIP64_SIGNATURE {
            return Err(ZipError::CentralDirectoryEndGeneric);
        }

        if data.len() < (total_size - eocd.cd_offset) as usize {
            if eocd.cd_offset < total_size {
                return Ok(eocd.cd_offset);
            }

            warn!("Something went wrong while parsing the end of central directory");
            return Ok(0);
        }

        let pos = eocd.cd_offset - (total_size - data.len() as i64);
        data.set_rpos(pos as usize);

        self.load_central_directory(data, eocd)?;

        Ok(0)
    }

    fn load_central_directory(
        &mut self,
        data: &mut ByteBuffer,
        eocd: EndOfCentralDirectory,
    ) -> Result<(), ZipError> {
        for i in 0..eocd.total_entries {
            let entry = match ZipFileEntry::parse(data) {
                Err(err) => {
                    return Err(ZipError::CentralDirectory {
                        idx: i,
                        total: eocd.total_entries,
                        err: format!("{:?}", err).to_string(),
                    })
                }
                Ok(e) => e,
            };

            self.entries.push(entry.clone());

            if i == 0 {
                continue;
            }

            let prev = if let Some(prev) = self.entries.get_mut(i as usize - 1) {
                prev
            } else {
                continue;
            };

            if prev.disk_number_start != entry.disk_number_start {
                warn!("Data offset could not be calculated");
                continue;
            }

            prev.data_offset = entry.local_header_offset - prev.compressed_size;
        }

        if let Some(last_entry) = self.entries.last_mut() {
            last_entry.data_offset = eocd.cd_offset - last_entry.compressed_size;
        }

        Ok(())
    }
}
