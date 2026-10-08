use std::io;

use tokio::io::{AsyncRead, AsyncReadExt};

/// LSX messages are NUL-terminated on the wire, in both directions. A single
/// request is a few hundred bytes; anything this large is a broken or hostile
/// peer, and we refuse to buffer it.
pub const MAX_FRAME_LEN: usize = 1024 * 1024;

const READ_CHUNK: usize = 4096;
const MESSAGE_END: &[u8] = b"</LSX>";

/// Splits an async byte stream into NUL-delimited frames.
///
/// `next_frame` is cancellation safe: everything it has read lives in
/// `self.buf`, so dropping the future (e.g. losing a `select!` race) never
/// loses bytes.
pub struct FrameReader<R> {
    reader: R,
    buf: Vec<u8>,
    /// Bytes at the start of `buf` already known not to contain a NUL.
    scanned: usize,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            buf: Vec::new(),
            scanned: 0,
        }
    }

    /// The next frame without its terminator, or `None` on a clean EOF.
    /// Bytes still buffered at EOF are delivered as a final frame.
    pub async fn next_frame(&mut self) -> io::Result<Option<Vec<u8>>> {
        loop {
            if let Some(frame) = self.take_frame() {
                return Ok(Some(frame));
            }

            if self.buf.len() > MAX_FRAME_LEN {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("LSX frame exceeds {} bytes", MAX_FRAME_LEN),
                ));
            }

            self.buf.reserve(READ_CHUNK);
            if self.reader.read_buf(&mut self.buf).await? == 0 {
                if self.buf.iter().all(|b| b.is_ascii_whitespace()) {
                    self.buf.clear();
                    return Ok(None);
                }

                self.scanned = 0;
                return Ok(Some(std::mem::take(&mut self.buf)));
            }
        }
    }

    fn take_frame(&mut self) -> Option<Vec<u8>> {
        while let Some(offset) = self.buf[self.scanned..].iter().position(|b| *b == 0) {
            let end = self.scanned + offset;
            let mut frame: Vec<u8> = self.buf.drain(..=end).collect();
            frame.pop();
            self.scanned = 0;
            // A bare terminator (e.g. the NUL following a message that was
            // already delivered without one) carries nothing.
            if !frame.is_empty() {
                return Some(frame);
            }
        }
        self.scanned = self.buf.len();

        // Tolerate a peer that forgets the terminator: a buffer that ends in
        // a closing `</LSX>` is a complete message (ciphertext is hex and can
        // never contain `<`, so this can't misfire on encrypted traffic).
        let end = self.buf.iter().rposition(|b| !b.is_ascii_whitespace())? + 1;
        if self.buf[..end].ends_with(MESSAGE_END) {
            self.scanned = 0;
            return Some(std::mem::take(&mut self.buf));
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    async fn collect<R: AsyncRead + Unpin>(reader: R) -> io::Result<Vec<Vec<u8>>> {
        let mut frames = FrameReader::new(reader);
        let mut out = Vec::new();
        while let Some(frame) = frames.next_frame().await? {
            out.push(frame);
        }
        Ok(out)
    }

    #[tokio::test]
    async fn splits_on_nul_and_strips_terminator() {
        let frames = collect(&b"<LSX>a</LSX>\0<LSX>b</LSX>\0"[..]).await.unwrap();
        assert_eq!(
            frames,
            vec![b"<LSX>a</LSX>".to_vec(), b"<LSX>b</LSX>".to_vec()]
        );
    }

    #[tokio::test]
    async fn reassembles_frames_split_across_reads() {
        let (mut tx, rx) = tokio::io::duplex(8);
        let writer = tokio::spawn(async move {
            for chunk in [&b"<LS"[..], b"X>one</L", b"SX>\0<LSX>tw", b"o</LSX>", b"\0"] {
                tx.write_all(chunk).await.unwrap();
                tx.flush().await.unwrap();
                tokio::task::yield_now().await;
            }
        });

        let frames = collect(rx).await.unwrap();
        writer.await.unwrap();
        assert_eq!(
            frames,
            vec![b"<LSX>one</LSX>".to_vec(), b"<LSX>two</LSX>".to_vec()]
        );
    }

    #[tokio::test]
    async fn bare_terminators_are_skipped() {
        let frames = collect(&b"\0abc\0\0\0def\0"[..]).await.unwrap();
        assert_eq!(frames, vec![b"abc".to_vec(), b"def".to_vec()]);
    }

    #[tokio::test]
    async fn unterminated_final_frame_is_delivered_at_eof() {
        let frames = collect(&b"first\0trailing"[..]).await.unwrap();
        assert_eq!(frames, vec![b"first".to_vec(), b"trailing".to_vec()]);
    }

    #[tokio::test]
    async fn trailing_whitespace_alone_is_not_a_frame() {
        let frames = collect(&b"first\0\r\n  "[..]).await.unwrap();
        assert_eq!(frames, vec![b"first".to_vec()]);
    }

    #[tokio::test]
    async fn complete_message_without_terminator_is_delivered_immediately() {
        let (mut tx, rx) = tokio::io::duplex(1024);
        let mut frames = FrameReader::new(rx);
        tx.write_all(b"<LSX><Request/></LSX>").await.unwrap();

        // The peer keeps the socket open and waits for a reply, so this must
        // resolve without EOF or a NUL.
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), frames.next_frame())
            .await
            .expect("frame should not wait for a terminator")
            .unwrap()
            .unwrap();
        assert_eq!(frame, b"<LSX><Request/></LSX>".to_vec());
    }

    #[tokio::test]
    async fn oversized_frame_is_an_error() {
        let data = vec![b'a'; MAX_FRAME_LEN + READ_CHUNK + 1];
        let err = collect(&data[..]).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn next_frame_is_cancellation_safe() {
        let (mut tx, rx) = tokio::io::duplex(1024);
        let mut frames = FrameReader::new(rx);

        tx.write_all(b"par").await.unwrap();
        let pending =
            tokio::time::timeout(std::time::Duration::from_millis(20), frames.next_frame()).await;
        assert!(pending.is_err(), "no terminator yet, must still be pending");

        tx.write_all(b"tial\0").await.unwrap();
        assert_eq!(
            frames.next_frame().await.unwrap().unwrap(),
            b"partial".to_vec()
        );
    }
}
