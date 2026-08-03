//! IO bridges between async chunk producers and blocking consumers.

/// A message from the async producer to a [`ChunkReader`].
///
/// The producer distinguishes transport failures from local validation
/// failures. A streaming importer must not retry an archive that has already
/// failed local validation merely because the producer later observes a TCP
/// reset while draining the response.
pub enum ChunkReaderInput {
    Chunk(bytes::Bytes),
    LocalError(std::io::Error),
    ProducerTransportError(std::io::Error),
}

/// Bridge from a tokio mpsc Receiver of byte chunks to a blocking
/// std::io::Read. Used by the streaming tarball pipeline to feed
/// HTTP body chunks into the gz+tar reader running on the blocking
/// pool. Each error input surfaces as `Read::read` Err so the
/// downstream parser aborts cleanly.
pub struct ChunkReader {
    rx: tokio::sync::mpsc::Receiver<ChunkReaderInput>,
    current: bytes::Bytes,
    pos: usize,
    producer_transport_error_seen: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl ChunkReader {
    pub fn new(
        rx: tokio::sync::mpsc::Receiver<ChunkReaderInput>,
        producer_transport_error_seen: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            rx,
            current: bytes::Bytes::new(),
            pos: 0,
            producer_transport_error_seen,
        }
    }
}

impl std::io::Read for ChunkReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.pos < self.current.len() {
                let n = (self.current.len() - self.pos).min(buf.len());
                buf[..n].copy_from_slice(&self.current[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            match self.rx.blocking_recv() {
                Some(ChunkReaderInput::Chunk(chunk)) => {
                    self.current = chunk;
                    self.pos = 0;
                }
                Some(ChunkReaderInput::LocalError(e)) => return Err(e),
                Some(ChunkReaderInput::ProducerTransportError(e)) => {
                    self.producer_transport_error_seen
                        .store(true, std::sync::atomic::Ordering::Release);
                    return Err(e);
                }
                None => return Ok(0),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn transport_sentinel_marks_only_an_error_read_by_importer() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let seen = Arc::new(AtomicBool::new(false));
        tx.try_send(ChunkReaderInput::ProducerTransportError(
            std::io::Error::other("connection reset"),
        ))
        .unwrap();
        let mut reader = ChunkReader::new(rx, seen.clone());

        let error = reader.read(&mut [0]).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert!(seen.load(Ordering::Acquire));
    }

    #[test]
    fn local_error_does_not_claim_transport_provenance() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let seen = Arc::new(AtomicBool::new(false));
        tx.try_send(ChunkReaderInput::LocalError(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "body cap",
        )))
        .unwrap();
        let mut reader = ChunkReader::new(rx, seen.clone());

        let error = reader.read(&mut [0]).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(!seen.load(Ordering::Acquire));
    }
}
