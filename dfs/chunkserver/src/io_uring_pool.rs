use std::io;
use std::path::PathBuf;
use tokio::sync::{mpsc, oneshot};
use tokio_uring::buf::BoundedBuf;
use tokio_uring::fs::File;

enum IoRequest {
    Write {
        path: PathBuf,
        data: Vec<u8>,
        reply: oneshot::Sender<io::Result<()>>,
    },
    Read {
        path: PathBuf,
        offset: u64,
        length: usize,
        reply: oneshot::Sender<io::Result<Vec<u8>>>,
    },
}

pub struct IoUringPool {
    sender: mpsc::Sender<IoRequest>,
}

impl IoUringPool {
    pub fn new(channel_capacity: usize) -> Self {
        let (tx, mut rx) = mpsc::channel::<IoRequest>(channel_capacity);
        std::thread::Builder::new()
            .name("io-uring-pool".to_string())
            .spawn(move || {
                tokio_uring::start(async move {
                    while let Some(req) = rx.recv().await {
                        tokio_uring::spawn(async move {
                            match req {
                                IoRequest::Write { path, data, reply } => {
                                    let _ = reply.send(do_write(path, data).await);
                                }
                                IoRequest::Read {
                                    path,
                                    offset,
                                    length,
                                    reply,
                                } => {
                                    let _ = reply.send(do_read(path, offset, length).await);
                                }
                            }
                        });
                    }
                });
            })
            .expect("failed to spawn io-uring thread");
        IoUringPool { sender: tx }
    }

    pub async fn write(&self, path: PathBuf, data: Vec<u8>) -> io::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.sender
            .send(IoRequest::Write {
                path,
                data,
                reply: tx,
            })
            .await
            .map_err(|_| io::Error::other("io_uring pool unavailable"))?;
        rx.await
            .map_err(|_| io::Error::other("io_uring thread panicked"))?
    }

    pub async fn read(&self, path: PathBuf, offset: u64, length: usize) -> io::Result<Vec<u8>> {
        let (tx, rx) = oneshot::channel();
        self.sender
            .send(IoRequest::Read {
                path,
                offset,
                length,
                reply: tx,
            })
            .await
            .map_err(|_| io::Error::other("io_uring pool unavailable"))?;
        rx.await
            .map_err(|_| io::Error::other("io_uring thread panicked"))?
    }
}

async fn do_write(path: PathBuf, data: Vec<u8>) -> io::Result<()> {
    // File::create uses O_CREAT|O_WRONLY|O_TRUNC — any existing content is discarded.
    // This is intentional: blocks are always written whole (no partial overwrites).
    let file = File::create(&path).await?;
    let (res, _) = file.write_all_at(data, 0).await;
    res?;
    file.sync_all().await
}

async fn do_read(path: PathBuf, offset: u64, length: usize) -> io::Result<Vec<u8>> {
    let file = File::open(&path).await?;
    let mut buf = vec![0u8; length];
    let mut filled = 0usize;
    while filled < length {
        // Use a Slice to read directly into buf[filled..] without allocating a
        // sub-buffer.  slice(filled..) takes ownership of `buf`, submits the
        // io-uring read into the correct region, then returns the Vec via
        // into_inner() so we can continue using it.
        let slice = buf.slice(filled..length);
        let (res, slice) = file.read_at(slice, offset + filled as u64).await;
        let n = res?;
        buf = slice.into_inner();
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "read_at returned 0 bytes before buffer full",
            ));
        }
        filled += n;
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_write_read_roundtrip() {
        let pool = IoUringPool::new(16);
        let dir = tempdir().unwrap();
        let path = dir.path().join("block001");
        pool.write(path.clone(), b"hello io_uring".to_vec())
            .await
            .unwrap();
        let data = pool.read(path, 0, 14).await.unwrap();
        assert_eq!(data, b"hello io_uring");
    }

    #[tokio::test]
    async fn test_partial_read_at_offset() {
        let pool = IoUringPool::new(16);
        let dir = tempdir().unwrap();
        let path = dir.path().join("block002");
        let data: Vec<u8> = (0u8..=255).cycle().take(65536).collect();
        pool.write(path.clone(), data.clone()).await.unwrap();
        let result = pool.read(path, 32768, 4096).await.unwrap();
        assert_eq!(result, data[32768..32768 + 4096].to_vec());
    }

    #[tokio::test]
    async fn test_write_creates_file() {
        let pool = IoUringPool::new(16);
        let dir = tempdir().unwrap();
        let path = dir.path().join("block003");
        pool.write(path.clone(), b"test data".to_vec())
            .await
            .unwrap();
        assert!(path.exists());
        let content = std::fs::read(&path).unwrap();
        assert_eq!(content, b"test data");
    }
}
