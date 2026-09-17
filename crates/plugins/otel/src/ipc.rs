use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

pub const MAX_RECORD_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Signal {
    Traces = 1,
    Logs = 2,
    Metrics = 3,
}

#[derive(Clone)]
pub struct Sender(Arc<Inner>);

struct Inner {
    path: PathBuf,
    connection: Mutex<Connection>,
    dropped: AtomicU64,
}

struct Connection {
    pid: u32,
    stream: Option<UnixStream>,
}

impl Sender {
    pub fn new(path: PathBuf) -> Self {
        Self(Arc::new(Inner {
            path,
            connection: Mutex::new(Connection {
                pid: std::process::id(),
                stream: None,
            }),
            dropped: AtomicU64::new(0),
        }))
    }

    pub fn dropped_records(&self) -> u64 {
        self.0.dropped.load(Relaxed)
    }

    pub fn disconnect(&self) {
        if let Ok(mut connection) = self.0.connection.lock() {
            connection.stream = None;
        }
    }

    pub fn send(&self, signal: Signal, payload: &[u8]) -> bool {
        let accepted = self.submit(signal, payload).is_ok();
        if !accepted {
            self.0.dropped.fetch_add(1, Relaxed);
        }
        accepted
    }

    fn submit(&self, signal: Signal, payload: &[u8]) -> io::Result<()> {
        if payload.len() >= MAX_RECORD_BYTES {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let mut frame = Vec::with_capacity(payload.len() + 5);
        frame.extend_from_slice(&((payload.len() + 1) as u32).to_be_bytes());
        frame.push(signal as u8);
        frame.extend_from_slice(payload);
        let mut connection = self
            .0
            .connection
            .lock()
            .map_err(|_| io::Error::other("otel connection lock poisoned"))?;
        let pid = std::process::id();
        if connection.pid != pid {
            connection.stream = None;
            connection.pid = pid;
            self.0.dropped.store(0, Relaxed);
        }
        if connection.stream.is_none() {
            connection.stream = Some(connect(&self.0.path)?);
        }
        let result = connection
            .stream
            .as_mut()
            .expect("connected stream")
            .write_all(&frame);
        if result.is_err() {
            // A partial frame ends with this connection. The receiver keeps complete records.
            connection.stream = None;
        }
        result
    }
}

fn connect(path: &Path) -> io::Result<UnixStream> {
    // SAFETY: socket returns a new descriptor or -1.
    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: raw is a new owned descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: fd is live and F_SETFD takes an integer flag.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let capacity = (MAX_RECORD_BYTES + 4) as libc::c_int;
    // SAFETY: fd is live and capacity is a readable socket-option value.
    if unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            (&capacity as *const libc::c_int).cast(),
            std::mem::size_of_val(&capacity) as _,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    let stream = UnixStream::from(fd);
    stream.set_nonblocking(true)?;
    // SAFETY: an all-zero sockaddr_un is valid for initialization.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= addr.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "otel socket path is too long",
        ));
    }
    addr.sun_family = libc::AF_UNIX as _;
    for (dst, src) in addr.sun_path.iter_mut().zip(bytes) {
        *dst = *src as _;
    }
    let len = std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1;
    #[cfg(target_os = "macos")]
    {
        addr.sun_len = len as _;
    }
    // SAFETY: addr contains a terminated Unix socket path and len covers it.
    if unsafe {
        libc::connect(
            stream.as_raw_fd(),
            (&addr as *const libc::sockaddr_un).cast(),
            len as _,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixListener;
    use std::sync::Barrier;

    #[test]
    fn large_records_fit_an_idle_connection() {
        struct Case {
            name: &'static str,
            bytes: usize,
        }
        let cases = [
            Case {
                name: "metric snapshot above the default macOS socket buffer",
                bytes: 16 * 1024,
            },
            Case {
                name: "large log record",
                bytes: 128 * 1024,
            },
        ];
        for case in cases {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("otel.sock");
            let listener = UnixListener::bind(&path).unwrap();
            let sender = Sender::new(path);
            let payload = vec![b'x'; case.bytes];
            let accepted = sender.send(Signal::Metrics, &payload);
            drop(sender);
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = Vec::new();
            stream.read_to_end(&mut bytes).unwrap();
            assert!(accepted, "{}: idle socket dropped the record", case.name);
            assert_eq!(bytes.len(), case.bytes + 5, "{}", case.name);
            assert_eq!(bytes[4], Signal::Metrics as u8, "{}", case.name);
            assert_eq!(&bytes[5..], payload, "{}", case.name);
        }
    }

    #[test]
    fn concurrent_producers_preserve_small_records() {
        struct Case {
            name: &'static str,
            connected: bool,
            records: usize,
        }
        let cases = [
            Case {
                name: "concurrent first submissions",
                connected: false,
                records: 8,
            },
            Case {
                name: "concurrent submissions on an open connection",
                connected: true,
                records: 9,
            },
        ];
        for case in cases {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("otel.sock");
            let listener = UnixListener::bind(&path).unwrap();
            let sender = Sender::new(path);
            if case.connected {
                assert!(sender.send(Signal::Traces, b"completed"));
            }
            let receiver = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                stream.read_to_end(&mut bytes).unwrap();
                bytes
            });
            let start = Barrier::new(8);
            let submitted = std::thread::scope(|scope| {
                let producers: Vec<_> = (0..8)
                    .map(|_| {
                        scope.spawn(|| {
                            start.wait();
                            sender.send(Signal::Traces, b"completed")
                        })
                    })
                    .collect();
                producers
                    .into_iter()
                    .map(|producer| usize::from(producer.join().unwrap()))
                    .sum::<usize>()
            });
            assert_eq!(submitted, 8, "{}", case.name);
            assert_eq!(sender.dropped_records(), 0, "{}", case.name);
            drop(sender);
            let bytes = receiver.join().unwrap();
            assert_eq!(bytes.len(), case.records * 14, "{}", case.name);
            for frame in bytes.as_chunks::<14>().0 {
                assert_eq!(frame, b"\0\0\0\x0a\x01completed", "{}", case.name);
            }
        }
    }

    #[test]
    fn backpressure_drops_new_records_and_preserves_complete_frames() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("otel.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let sender = Sender::new(path);
        let payload = [b'x'; 1024];
        assert!(sender.send(Signal::Traces, &payload));
        let (mut stream, _) = listener.accept().unwrap();
        let mut accepted = 1;
        while accepted < 10_000 && sender.send(Signal::Traces, &payload) {
            accepted += 1;
        }
        assert!(
            accepted < 10_000,
            "a stalled receiver must apply backpressure"
        );
        assert_eq!(sender.dropped_records(), 1);
        drop(sender);
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).unwrap();
        let frame_size = 1029;
        for frame in bytes[..accepted * frame_size].chunks_exact(frame_size) {
            assert_eq!(&frame[..5], b"\0\0\x04\x01\x01");
            assert_eq!(&frame[5..], payload);
        }
        assert!(bytes.len() - accepted * frame_size < frame_size);
    }

    #[test]
    fn submitted_records_survive_producer_disconnect() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("otel.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let sender = Sender::new(path);
        assert!(sender.send(Signal::Traces, b"completed-span"));
        assert!(sender.send(Signal::Logs, b"worker-log"));
        drop(sender);
        let (mut stream, _) = listener.accept().unwrap();
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).unwrap();
        assert_eq!(
            bytes,
            b"\0\0\0\x0f\x01completed-span\0\0\0\x0b\x02worker-log"
        );
    }

    #[test]
    fn failed_submission_reconnects_to_a_replacement_listener() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("otel.sock");
        let sender = Sender::new(path.clone());
        assert!(!sender.send(Signal::Traces, b"before-start"));
        assert_eq!(sender.dropped_records(), 1);
        let listener = UnixListener::bind(path).unwrap();
        assert!(sender.send(Signal::Traces, b"after-start"));
        drop(sender);
        let (mut stream, _) = listener.accept().unwrap();
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"\0\0\0\x0c\x01after-start");
    }
}
