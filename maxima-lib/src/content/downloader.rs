use std::{fmt::Display, io::SeekFrom, path::Path, pin::Pin, prelude, task, task::Poll};

use anyhow::Result;
use async_compression::tokio::write::DeflateDecoder;
use bytes::{Bytes, BytesMut};
use futures::{Stream, StreamExt, TryStreamExt};
use log::{debug, error, info};
use reqwest::Client;
use strum_macros::Display;
use tokio::{
    fs::{create_dir, create_dir_all, OpenOptions},
    io::{AsyncSeekExt, AsyncWrite, BufReader},
};

use tokio_util::compat::FuturesAsyncReadCompatExt;

use crate::content::{
    zip::CompressionType,
    zlib::{restore_zlib_state, write_zlib_state},
};

use super::zip::{ZipFile, ZipFileEntry};

#[derive(Debug)]
enum DecoderRestoreError {
    CacheEmpty,
}

trait RestorableDecoder {
    fn save_state(&mut self);

    fn restore_state(&mut self) -> Result<(u64, u64), DecoderRestoreError>;
}

struct RestorableDeflateDecoder<W: AsyncWrite> {
    inner: DeflateDecoder<W>,
    file_name: String,
    bytes_written: usize,
    bytes_since_last: usize,
    should_save: bool,
}

impl<W: AsyncWrite> RestorableDeflateDecoder<W> {
    fn new(decoder: DeflateDecoder<W>, file_name: String) -> Self {
        Self {
            inner: decoder,
            file_name,
            bytes_written: 0,
            bytes_since_last: 0,
            should_save: false,
        }
    }

    fn update_bytes_written(&mut self, bytes: usize) {
        self.bytes_written += bytes;

        // if self.bytes_since_last + bytes >= 1_048_576 {
        if self.bytes_since_last + bytes >= 65536 {
            // self.bytes_since_last = self.bytes_since_last + bytes - 1_048_576;
            self.bytes_since_last = self.bytes_since_last + bytes - 65536;
            self.should_save = true;
            return;
        }

        self.bytes_since_last += bytes;
        self.should_save = false;
    }

    pub fn get_ref(&self) -> &W {
        self.inner.get_ref()
    }

    pub fn get_mut(&mut self) -> &mut W {
        self.inner.get_mut()
    }
}

impl<W: AsyncWrite> RestorableDecoder for RestorableDeflateDecoder<W> {
    fn save_state(&mut self) {
        let mut buf = BytesMut::new();

        {
            let zstream = self
                .inner
                .inner_mut()
                .decoder_mut()
                .inner
                .decompress
                .get_raw();
            write_zlib_state(&mut buf, zstream);
        }

        let cache_dir = dirs::cache_dir().unwrap().join("Maxima");

        if !cache_dir.exists() {
            std::fs::create_dir(&cache_dir).unwrap();
        }

        let cache_dir = cache_dir.join(format!("{}.state", self.file_name));

        if !cache_dir.parent().unwrap().exists() {
            std::fs::create_dir_all(&cache_dir.parent().unwrap()).unwrap();
        }

        std::fs::write(cache_dir, buf).unwrap();

        debug!("Serialized zlib state");
    }

    fn restore_state(&mut self) -> Result<(u64, u64), DecoderRestoreError> {
        let cache_dir = dirs::cache_dir()
            .unwrap()
            .join("Maxima")
            .join(format!("{}.state", self.file_name));

        if !cache_dir.exists() {
            debug!("No cache available.");
            return Err(DecoderRestoreError::CacheEmpty);
        }

        info!("Got some cache!");

        let mut bytes = Bytes::from(std::fs::read(cache_dir).unwrap());
        let decompress = &mut self.inner.inner_mut().decoder_mut().inner.decompress;
        decompress.reset(false);
        let zstream = decompress.get_raw();
        restore_zlib_state(&mut bytes, zstream);
        debug!("reset and restored zlib state");

        Ok((zstream.total_in, zstream.total_out))
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for RestorableDeflateDecoder<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &[u8],
    ) -> task::Poll<prelude::v1::Result<usize, std::io::Error>> {
        let inner = Pin::new(&mut self.inner);
        let poll_result = inner.poll_write(cx, buf);

        if let Poll::Ready(Ok(bytes_written)) = &poll_result {
            // Update the bytes_written count when the write is successful
            self.update_bytes_written(*bytes_written);
        }

        if self.should_save {
            debug!("bytes have been written since last time. Saving state...",);
            self.save_state();
        }

        poll_result
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<prelude::v1::Result<(), std::io::Error>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<prelude::v1::Result<(), std::io::Error>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

pub struct ZipDownloadRequest {
    _entries: Vec<ZipFileEntry>,
}

pub struct ZipDownloader {
    url: String,
    client: Client,
    manifest: ZipFile,
}

impl ZipDownloader {
    pub async fn new(url: &str) -> Result<Self> {
        let manifest = ZipFile::fetch(url).await?;

        Ok(Self {
            url: url.to_owned(),
            client: Client::builder().build()?,
            manifest,
        })
    }

    pub fn manifest(&self) -> &ZipFile {
        &self.manifest
    }

    #[async_recursion::async_recursion]
    pub async fn download_single_file(&self, entry: &ZipFileEntry) -> Result<usize> {
        let dir_path = Path::new("/Users/gustash/Documents/GameTest");
        let file_path = dir_path.join(entry.name());

        if !file_path.parent().unwrap().exists() {
            debug!("Creating {}", file_path.parent().unwrap().display());
            create_dir_all(&file_path.parent().unwrap()).await?;
        }

        if entry.name().ends_with("/") && !file_path.exists() {
            // This is a folder, create the dir
            debug!("{} is a directory", entry.name());
            debug!("Creating {}", file_path.display());
            create_dir(file_path).await?;
            return Ok(0);
        }

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file_path)
            .await?;

        if *entry.uncompressed_size() == 0 {
            debug!("{} is empty", entry.name());
            return Ok(0);
        }

        let mut bytes_downloaded = 0;
        let writer = tokio::io::BufWriter::new(file);
        let mut writer: Box<dyn AsyncWrite + Unpin + Send> = match entry.compression_type() {
            CompressionType::None => {
                bytes_downloaded = match tokio::fs::metadata(&file_path).await {
                    Ok(metadata) => metadata.len(),
                    Err(_) => 0,
                };

                Box::new(writer)
            }
            CompressionType::Deflate => {
                let decoder = DeflateDecoder::new(writer);
                let mut decoder = RestorableDeflateDecoder::new(decoder, entry.name().into());

                match decoder.restore_state() {
                    Ok((bytes_in, bytes_out)) => {
                        bytes_downloaded = bytes_in;

                        let current_size = match tokio::fs::metadata(&file_path).await {
                            Ok(metadata) => metadata.len(),
                            Err(_) => 0,
                        };
                        let new_size = current_size.saturating_sub(bytes_out);
                        info!("New size for {} is {}", entry.name(), new_size);

                        let file = decoder.get_mut().get_mut();
                        file.seek(SeekFrom::Start(0)).await?;
                        file.set_len(new_size).await?;
                    }
                    Err(err) => {
                        let file = decoder.get_mut().get_mut();
                        file.seek(SeekFrom::Start(0)).await?;
                        file.set_len(0).await?;
                        debug!("Failed to restore state for {}: {:?}", entry.name(), err);
                    }
                }

                Box::new(decoder)
            }
        };

        let offset = entry.data_offset();
        let start_offset = if bytes_downloaded == 0 {
            offset.clone()
        } else {
            offset + (bytes_downloaded as i64)
        };
        debug!("Type: {:?}", entry.compression_type());
        debug!("Compressed Size: {}", entry.compressed_size());
        debug!("Offset: {}", offset);

        let range = format!(
            "bytes={}-{}",
            start_offset,
            offset + entry.compressed_size() - 1
        );
        let data = match self
            .client
            .get(&self.url)
            .header("range", range)
            .send()
            .await
        {
            Ok(res) => res,
            Err(err) => {
                error!("Failed to download ({}): {}", file_path.display(), err);

                return self.download_single_file(entry).await;
            }
        };

        let stream = data.bytes_stream();
        let counting_stream = ByteCountingStream::new(stream);
        let stream = counting_stream.into_async_read();
        let mut stream_reader = BufReader::new(stream.compat());

        tokio::io::copy(&mut stream_reader, &mut writer).await?;

        Ok(0)
    }
}

struct ByteCountingStream<S> {
    inner: S,
    byte_count: usize,
}

impl<S> ByteCountingStream<S>
where
    S: Stream<Item = Result<bytes::Bytes, reqwest::Error>>,
{
    fn new(inner: S) -> Self {
        ByteCountingStream {
            inner,
            byte_count: 0,
        }
    }

    fn byte_count(&self) -> usize {
        self.byte_count
    }
}

#[derive(Debug, Display)]
pub enum DownloadError {
    DownloadFailed(usize),
}

impl std::error::Error for DownloadError {}

impl<S> Stream for ByteCountingStream<S>
where
    S: Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
{
    type Item = Result<bytes::Bytes, tokio::io::Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match self.inner.poll_next_unpin(cx) {
            std::task::Poll::Ready(Some(Ok(chunk))) => {
                self.byte_count += chunk.len();
                std::task::Poll::Ready(Some(Ok(chunk)))
            }
            std::task::Poll::Ready(Some(Err(_))) => std::task::Poll::Ready(Some(Err(
                futures::io::Error::other(DownloadError::DownloadFailed(self.byte_count)),
            ))),
            std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}
