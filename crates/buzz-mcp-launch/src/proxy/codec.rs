//! Bounded newline-delimited JSON-RPC decoder (memo decision 4).
//!
//! MCP over stdio is newline-delimited JSON and carries no length prefix, so
//! the inbound bound has to be constructed explicitly. `rmcp` 1.8.0 resolves in
//! this workspace (`Cargo.lock`) and its newline codec defaults to
//! `usize::MAX`: the shipped default is no bound at all. This decoder refuses a
//! frame over its cap **before** any JSON deserialization and without buffering
//! past the cap, so neither an unterminated flood nor an oversized terminated
//! frame can grow the process.

use tokio::io::{AsyncRead, AsyncReadExt};

/// Production inbound frame cap.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Bytes read from the transport per `read` call.
const READ_CHUNK: usize = 16 * 1024;

/// Why a frame could not be decoded.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    /// The frame exceeded the decoder's cap. Carries the byte count observed
    /// when the cap was breached, which is at most `cap + READ_CHUNK`.
    #[error("json-rpc frame exceeded the {cap}-byte cap (saw {observed} bytes)")]
    TooLong {
        /// The configured cap.
        cap: usize,
        /// Bytes buffered when the breach was detected.
        observed: usize,
    },
    /// The underlying transport failed.
    #[error("transport read failed: {0}")]
    Io(String),
}

/// A newline-delimited frame reader with a hard byte cap.
pub struct FrameDecoder {
    buffer: Vec<u8>,
    cap: usize,
}

impl FrameDecoder {
    /// A decoder that refuses any frame longer than `cap` bytes.
    pub fn with_cap(cap: usize) -> Self {
        Self {
            buffer: Vec::new(),
            cap,
        }
    }

    /// A decoder at the production cap of [`MAX_FRAME_BYTES`].
    pub fn new() -> Self {
        Self::with_cap(MAX_FRAME_BYTES)
    }

    /// Read the next frame from `reader`, without its trailing newline.
    ///
    /// `Ok(None)` at clean end of input.
    ///
    /// # Errors
    /// [`FrameError::TooLong`] when the frame is over the cap — whether or not
    /// it is ever terminated — and [`FrameError::Io`] when the transport fails.
    /// Neither is recoverable: the caller closes the transport, because a
    /// decoder that skipped an oversized frame would resynchronize on attacker
    /// -chosen boundaries.
    pub async fn next_frame<R>(&mut self, reader: &mut R) -> Result<Option<Vec<u8>>, FrameError>
    where
        R: AsyncRead + Unpin,
    {
        loop {
            if let Some(index) = self.buffer.iter().position(|b| *b == b'\n') {
                if index > self.cap {
                    return Err(FrameError::TooLong {
                        cap: self.cap,
                        observed: index,
                    });
                }
                let mut frame: Vec<u8> = self.buffer.drain(..=index).collect();
                frame.pop();
                if frame.last() == Some(&b'\r') {
                    frame.pop();
                }
                return Ok(Some(frame));
            }
            if self.buffer.len() > self.cap {
                return Err(FrameError::TooLong {
                    cap: self.cap,
                    observed: self.buffer.len(),
                });
            }

            let before = self.buffer.len();
            // Never grow past cap + one chunk: the check above runs on the next
            // pass, so the buffer's high-water mark is bounded, not the input.
            self.buffer.resize(before + READ_CHUNK, 0);
            let read = reader
                .read(&mut self.buffer[before..])
                .await
                .map_err(|e| FrameError::Io(e.to_string()))?;
            self.buffer.truncate(before + read);
            if read == 0 {
                if self.buffer.is_empty() {
                    return Ok(None);
                }
                // A trailing frame with no newline is still bounded by the cap
                // check above, so returning it here cannot exceed the budget.
                let frame = std::mem::take(&mut self.buffer);
                return Ok(Some(frame));
            }
        }
    }
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_successive_frames() {
        let mut input = &b"{\"a\":1}\n{\"b\":2}\n"[..];
        let mut decoder = FrameDecoder::new();
        assert_eq!(
            decoder.next_frame(&mut input).await.expect("frame"),
            Some(b"{\"a\":1}".to_vec())
        );
        assert_eq!(
            decoder.next_frame(&mut input).await.expect("frame"),
            Some(b"{\"b\":2}".to_vec())
        );
        assert_eq!(decoder.next_frame(&mut input).await.expect("eof"), None);
    }

    #[tokio::test]
    async fn strips_a_carriage_return() {
        let mut input = &b"{\"a\":1}\r\n"[..];
        let mut decoder = FrameDecoder::new();
        assert_eq!(
            decoder.next_frame(&mut input).await.expect("frame"),
            Some(b"{\"a\":1}".to_vec())
        );
    }

    #[tokio::test]
    async fn stdio_frame_bound_holds_without_a_newline() {
        let flood = vec![b'x'; MAX_FRAME_BYTES + 1024];
        let mut input = &flood[..];
        let mut decoder = FrameDecoder::new();
        let err = decoder
            .next_frame(&mut input)
            .await
            .expect_err("an unterminated flood must be refused");
        assert!(matches!(err, FrameError::TooLong { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn stdio_frame_bound_holds_with_a_newline() {
        let mut flood = vec![b'x'; MAX_FRAME_BYTES + 1024];
        flood.push(b'\n');
        let mut input = &flood[..];
        let mut decoder = FrameDecoder::new();
        let err = decoder
            .next_frame(&mut input)
            .await
            .expect_err("an oversized terminated frame must be refused");
        assert!(matches!(err, FrameError::TooLong { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn a_frame_exactly_at_the_cap_is_accepted() {
        let mut at_cap = vec![b'x'; MAX_FRAME_BYTES];
        at_cap.push(b'\n');
        let mut input = &at_cap[..];
        let mut decoder = FrameDecoder::new();
        let frame = decoder
            .next_frame(&mut input)
            .await
            .expect("cap is inclusive")
            .expect("a frame");
        assert_eq!(frame.len(), MAX_FRAME_BYTES);
    }

    #[tokio::test]
    async fn the_buffer_never_grows_far_past_the_cap() {
        // The guard is the cap check that runs before every read, not the
        // input length: a 64 MiB flood must not become a 64 MiB buffer.
        let flood = vec![b'x'; 8 * 1024 * 1024];
        let mut input = &flood[..];
        let mut decoder = FrameDecoder::with_cap(4096);
        let err = decoder
            .next_frame(&mut input)
            .await
            .expect_err("refused at the cap");
        assert!(matches!(err, FrameError::TooLong { .. }), "{err:?}");
        assert!(
            decoder.buffer.len() <= 4096 + READ_CHUNK,
            "buffer grew to {} bytes",
            decoder.buffer.len()
        );
    }
}
