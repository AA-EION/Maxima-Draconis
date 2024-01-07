use std::path::Path;

use anyhow::Result;
use futures::{Stream, StreamExt, TryStreamExt};
use log::{debug, error, warn};
use reqwest::Client;
use strum_macros::Display;
use tokio::{
    fs::{create_dir, create_dir_all, File, OpenOptions},
    io::BufReader,
};

use tokio_util::compat::FuturesAsyncReadCompatExt;

use crate::content::zip::CompressionType;

use super::zip::{ZipFile, ZipFileEntry};

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
    pub async fn download_single_file(
        &self,
        entry: &ZipFileEntry,
        bytes_downloaded: usize,
    ) -> Result<usize> {
        let dir_path = Path::new("/Users/gustash/Documents/GameTest");
        let file_path = dir_path.join(entry.name());

        if bytes_downloaded == 0 {
            if !file_path.parent().unwrap().exists() {
                warn!("Creating {}", file_path.parent().unwrap().display());
                create_dir_all(&file_path.parent().unwrap()).await?;
            }

            if entry.name().ends_with("/") && !file_path.exists() {
                // This is a folder, create the dir
                debug!("{} is a directory", entry.name());
                warn!("Creating {}", file_path.display());
                create_dir(file_path).await?;
                return Ok(0);
            }
        }

        let file = if bytes_downloaded > 0 {
            OpenOptions::new().append(true).open(&file_path).await?
        } else {
            File::create(&file_path).await?
        };

        if *entry.uncompressed_size() == 0 {
            debug!("{} is empty", entry.name());
            return Ok(0);
        }

        let offset = entry.data_offset();
        debug!("Type: {:?}", entry.compression_type());
        debug!("Compressed Size: {}", entry.compressed_size());
        debug!("Offset: {}", offset);

        let range = format!(
            "bytes={}-{}",
            offset + bytes_downloaded as i64,
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

                return self.download_single_file(entry, bytes_downloaded).await;
            }
        };

        let stream = data.bytes_stream();
        let counting_stream = ByteCountingStream::new(stream);
        let stream = counting_stream.into_async_read();
        let mut stream_reader = BufReader::new(stream.compat());

        let mut writer = tokio::io::BufWriter::new(file);

        match entry.compression_type() {
            CompressionType::None => {
                tokio::io::copy(&mut stream_reader, &mut writer).await?;
            }
            CompressionType::Deflate => {
                let mut decoder = async_compression::tokio::write::DeflateDecoder::new(writer);

                tokio::io::copy(&mut stream_reader, &mut decoder).await?;
            }
        };

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
