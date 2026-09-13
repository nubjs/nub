//! An untrusted Windows relay carries proxy-client bytes, never dial or credential RPCs.
//! Every parent stream connects to one fixed, command-owned policy proxy. Frame credits
//! bound buffering without letting a slow client block other streams or cancellation.

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[cfg(windows)]
pub(crate) mod pipe;

const VERSION: u8 = 1;
const DATA: u8 = 1;
const OPEN: u8 = 2;
const FIN: u8 = 3;
const CLOSE: u8 = 4;
const CREDIT: u8 = 5;
const READY: u8 = 6;
const PAYLOAD: usize = 16 * 1024;
const STREAMS: usize = 64;
const WINDOW: usize = 4;
const QUEUE: usize = STREAMS * (WINDOW * 2 + 4);
const TICK: Duration = Duration::from_millis(5);

struct Frame {
    kind: u8,
    id: u64,
    bytes: Vec<u8>,
}

impl Frame {
    fn control(kind: u8, id: u64) -> Self {
        Self {
            kind,
            id,
            bytes: Vec::new(),
        }
    }

    fn read(reader: &mut impl Read) -> io::Result<Self> {
        let mut head = [0; 14];
        reader.read_exact(&mut head)?;
        let kind = head[1];
        let id = u64::from_le_bytes(head[2..10].try_into().unwrap());
        let len = u32::from_le_bytes(head[10..14].try_into().unwrap()) as usize;
        let valid = match kind {
            DATA => id != 0 && (1..=PAYLOAD).contains(&len),
            OPEN | FIN | CLOSE | CREDIT => id != 0 && len == 0,
            READY => id == 0 && len == 2,
            _ => false,
        };
        if head[0] != VERSION || !valid {
            return Err(invalid("invalid relay frame"));
        }
        let mut bytes = vec![0; len];
        reader.read_exact(&mut bytes)?;
        Ok(Self { kind, id, bytes })
    }

    fn write(&self, writer: &mut impl Write) -> io::Result<()> {
        let mut head = [0; 14];
        head[0] = VERSION;
        head[1] = self.kind;
        head[2..10].copy_from_slice(&self.id.to_le_bytes());
        head[10..14].copy_from_slice(&(self.bytes.len() as u32).to_le_bytes());
        writer.write_all(&head)?;
        writer.write_all(&self.bytes)
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn send(queue: &mpsc::SyncSender<Frame>, frame: Frame) -> io::Result<()> {
    queue
        .try_send(frame)
        .map_err(|_| invalid("relay output limit or closed channel"))
}

struct Stream {
    socket: TcpStream,
    pending: VecDeque<Vec<u8>>,
    offset: usize,
    credit: usize,
    peer_fin: bool,
    local_fin: bool,
    write_closed: bool,
}

impl Stream {
    fn new(socket: TcpStream) -> io::Result<Self> {
        socket.set_nonblocking(true)?;
        Ok(Self {
            socket,
            pending: VecDeque::new(),
            offset: 0,
            credit: WINDOW,
            peer_fin: false,
            local_fin: false,
            write_closed: false,
        })
    }

    fn frame(&mut self, frame: Frame) -> io::Result<()> {
        match frame.kind {
            DATA if !self.peer_fin && self.pending.len() < WINDOW => {
                self.pending.push_back(frame.bytes);
            }
            CREDIT if self.credit < WINDOW => self.credit += 1,
            FIN if !self.peer_fin => self.peer_fin = true,
            _ => return Err(invalid("relay stream state violation")),
        }
        Ok(())
    }

    fn pump(&mut self, id: u64, output: &mpsc::SyncSender<Frame>) -> io::Result<bool> {
        while let Some(bytes) = self.pending.front() {
            match self.socket.write(&bytes[self.offset..]) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    self.offset += n;
                    if self.offset == bytes.len() {
                        self.pending.pop_front();
                        self.offset = 0;
                        send(output, Frame::control(CREDIT, id))?;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        if self.peer_fin && self.pending.is_empty() && !self.write_closed {
            self.socket.shutdown(Shutdown::Write)?;
            self.write_closed = true;
        }
        while !self.local_fin && self.credit > 0 {
            let mut bytes = vec![0; PAYLOAD];
            match self.socket.read(&mut bytes) {
                Ok(0) => {
                    self.local_fin = true;
                    send(output, Frame::control(FIN, id))?;
                }
                Ok(n) => {
                    bytes.truncate(n);
                    self.credit -= 1;
                    send(
                        output,
                        Frame {
                            kind: DATA,
                            id,
                            bytes,
                        },
                    )?;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(self.local_fin && self.write_closed)
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}

enum Side {
    Parent {
        port: u16,
        ready: mpsc::SyncSender<u16>,
    },
    Helper(TcpListener),
}

fn run(
    mut input: impl Read + Send,
    mut output: impl Write + Send,
    side: Side,
    stop: Arc<AtomicBool>,
) -> io::Result<()> {
    thread::scope(|scope| {
        let (incoming, frames) = mpsc::sync_channel(QUEUE);
        let (outgoing, writes) = mpsc::sync_channel::<Frame>(QUEUE);
        let stopped = stop.clone();
        let reader = thread::Builder::new()
            .name("nub-relay-read".into())
            .spawn_scoped(scope, move || {
                loop {
                    let frame = Frame::read(&mut input);
                    let failed = frame.is_err();
                    if incoming.send(frame).is_err() || failed {
                        break;
                    }
                }
            })?;
        let writer = thread::Builder::new()
            .name("nub-relay-write".into())
            .spawn_scoped(scope, move || {
                while !stopped.load(Ordering::Acquire) {
                    match writes.recv_timeout(TICK) {
                        Ok(frame) if frame.write(&mut output).is_err() => break,
                        Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                stopped.store(true, Ordering::Release);
            });
        let result = match writer {
            Ok(writer) => {
                let result = drive(&frames, &outgoing, side, &stop);
                stop.store(true, Ordering::Release);
                drop(frames);
                drop(outgoing);
                let _ = writer.join();
                result
            }
            Err(error) => {
                stop.store(true, Ordering::Release);
                drop(frames);
                Err(error)
            }
        };
        let _ = reader.join();
        result
    })
}

fn drive(
    frames: &mpsc::Receiver<io::Result<Frame>>,
    outgoing: &mpsc::SyncSender<Frame>,
    side: Side,
    stop: &AtomicBool,
) -> io::Result<()> {
    let mut streams = BTreeMap::<u64, Stream>::new();
    let mut last_id = 0u64;
    let mut ready_seen = false;
    if let Side::Helper(listener) = &side {
        listener.set_nonblocking(true)?;
        send(
            outgoing,
            Frame {
                kind: READY,
                id: 0,
                bytes: listener.local_addr()?.port().to_le_bytes().to_vec(),
            },
        )?;
        ready_seen = true;
    }
    while !stop.load(Ordering::Acquire) {
        if let Side::Helper(listener) = &side {
            while streams.len() < STREAMS {
                let socket = match listener.accept() {
                    Ok((socket, _)) => socket,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) => return Err(error),
                };
                last_id = last_id
                    .checked_add(1)
                    .ok_or_else(|| invalid("relay stream ID overflow"))?;
                streams.insert(last_id, Stream::new(socket)?);
                send(outgoing, Frame::control(OPEN, last_id))?;
            }
        }
        let frame = match frames.recv_timeout(TICK) {
            Ok(frame) => Some(frame?),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if let Some(frame) = frame {
            match frame.kind {
                READY if !ready_seen => {
                    let Side::Parent { ready, .. } = &side else {
                        return Err(invalid("unexpected relay ready"));
                    };
                    let port = u16::from_le_bytes(frame.bytes[..].try_into().unwrap());
                    if port == 0 {
                        return Err(invalid("zero relay port"));
                    }
                    ready
                        .send(port)
                        .map_err(|_| invalid("relay readiness abandoned"))?;
                    ready_seen = true;
                }
                READY => return Err(invalid("duplicate relay ready")),
                _ if !ready_seen => return Err(invalid("relay used before ready")),
                OPEN => {
                    let Side::Parent { port, .. } = &side else {
                        return Err(invalid("parent cannot open relay streams"));
                    };
                    if frame.id <= last_id || streams.len() >= STREAMS {
                        return Err(invalid("relay stream ID or count limit"));
                    }
                    last_id = frame.id;
                    // The peer supplies no destination: this is the command's parent proxy.
                    match TcpStream::connect_timeout(
                        &([127, 0, 0, 1], *port).into(),
                        Duration::from_secs(1),
                    ) {
                        Ok(socket) => {
                            streams.insert(frame.id, Stream::new(socket)?);
                        }
                        Err(_) => send(outgoing, Frame::control(CLOSE, frame.id))?,
                    }
                }
                CLOSE => {
                    if frame.id > last_id {
                        return Err(invalid("unknown relay stream"));
                    }
                    streams.remove(&frame.id);
                }
                _ => {
                    if let Some(stream) = streams.get_mut(&frame.id) {
                        stream.frame(frame)?;
                    } else if frame.id > last_id {
                        return Err(invalid("unknown relay stream"));
                    }
                    // Frames already in flight when either end closes are discarded.
                }
            }
        }
        let mut closed = Vec::new();
        for (&id, stream) in &mut streams {
            if !matches!(stream.pump(id, outgoing), Ok(false)) {
                closed.push(id);
            }
        }
        for id in closed {
            streams.remove(&id);
            send(outgoing, Frame::control(CLOSE, id))?;
        }
    }
    Ok(())
}

pub(crate) struct Relay {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<io::Result<()>>>,
}

impl Relay {
    pub(crate) fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    #[cfg(windows)]
    pub(crate) fn parent(
        input: std::os::windows::io::OwnedHandle,
        output: std::os::windows::io::OwnedHandle,
        port: u16,
    ) -> io::Result<(Self, mpsc::Receiver<u16>)> {
        let stop = Arc::new(AtomicBool::new(false));
        let input = pipe::Pipe::new(input, stop.clone())?;
        let output = pipe::Pipe::new(output, stop.clone())?;
        let (ready, receiver) = mpsc::sync_channel(1);
        let stopped = stop.clone();
        let worker = thread::Builder::new()
            .name("nub-relay-parent".into())
            .spawn(move || run(input, output, Side::Parent { port, ready }, stopped))?;
        Ok((
            Self {
                stop,
                worker: Some(worker),
            },
            receiver,
        ))
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.stop();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(windows)]
pub(crate) fn serve() -> io::Result<()> {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
    let stop = Arc::new(AtomicBool::new(false));
    // The registered helper entry owns exactly these inherited endpoints.
    let input = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    let output = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    if input.is_null()
        || output.is_null()
        || input == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE
        || output == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE
        || input == output
    {
        return Err(invalid("relay requires two inherited pipe endpoints"));
    }
    let input = unsafe { OwnedHandle::from_raw_handle(input) };
    let output = unsafe { OwnedHandle::from_raw_handle(output) };
    let input = pipe::Pipe::new(input, stop.clone())?;
    let output = pipe::Pipe::new(output, stop.clone())?;
    let listener = TcpListener::bind(([127, 0, 0, 1], 0))?;
    run(input, output, Side::Helper(listener), stop)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    struct TestIo {
        socket: TcpStream,
        stop: Arc<AtomicBool>,
    }

    impl TestIo {
        fn retry<T>(
            &mut self,
            mut op: impl FnMut(&mut TcpStream) -> io::Result<T>,
        ) -> io::Result<T> {
            loop {
                if self.stop.load(Ordering::Acquire) {
                    return Err(io::ErrorKind::ConnectionAborted.into());
                }
                match op(&mut self.socket) {
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(TICK);
                    }
                    result => return result,
                }
            }
        }
    }

    impl Read for TestIo {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            self.retry(|socket| socket.read(bytes))
        }
    }

    impl Write for TestIo {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.retry(|socket| socket.write(bytes))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn sockets() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind(([127, 0, 0, 1], 0)).unwrap();
        let first = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let second = listener.accept().unwrap().0;
        (first, second)
    }

    pub(crate) struct TestRelay {
        pub(crate) port: u16,
        _parent: Relay,
        _helper: Relay,
    }

    impl TestRelay {
        pub(crate) fn start(port: u16) -> Self {
            fn start(socket: TcpStream, side: Side) -> Relay {
                socket.set_nonblocking(true).unwrap();
                let stop = Arc::new(AtomicBool::new(false));
                let input = TestIo {
                    socket: socket.try_clone().unwrap(),
                    stop: stop.clone(),
                };
                let output = TestIo {
                    socket,
                    stop: stop.clone(),
                };
                let stopped = stop.clone();
                let worker = thread::spawn(move || run(input, output, side, stopped));
                Relay {
                    stop,
                    worker: Some(worker),
                }
            }
            let (parent, helper) = sockets();
            let (ready, receiver) = mpsc::sync_channel(1);
            let parent = start(parent, Side::Parent { port, ready });
            let helper = start(
                helper,
                Side::Helper(TcpListener::bind(([127, 0, 0, 1], 0)).unwrap()),
            );
            let port = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
            Self {
                port,
                _parent: parent,
                _helper: helper,
            }
        }
    }

    #[test]
    fn relays_multiple_credit_windows_and_half_close() {
        let listener = TcpListener::bind(([127, 0, 0, 1], 0)).unwrap();
        let relay = TestRelay::start(listener.local_addr().unwrap().port());
        let payload = vec![0x5a; PAYLOAD * WINDOW * 3 + 7];
        let expected = payload.clone();
        let upstream = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut received = Vec::new();
            socket.read_to_end(&mut received).unwrap();
            assert_eq!(received, expected);
            socket.write_all(&received).unwrap();
        });
        let mut client = TcpStream::connect(([127, 0, 0, 1], relay.port)).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        client.write_all(&payload).unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        let mut received = Vec::new();
        client.read_to_end(&mut received).unwrap();
        assert_eq!(received, payload);
        upstream.join().unwrap();
    }

    #[test]
    fn cancellation_joins_idle_transport_readers() {
        let listener = TcpListener::bind(([127, 0, 0, 1], 0)).unwrap();
        let relay = TestRelay::start(listener.local_addr().unwrap().port());
        let _client = TcpStream::connect(([127, 0, 0, 1], relay.port)).unwrap();
        let (done, completed) = mpsc::channel();
        let worker = thread::spawn(move || {
            drop(relay);
            done.send(()).unwrap();
        });
        completed
            .recv_timeout(Duration::from_secs(10))
            .expect("relay cancellation must join idle I/O");
        worker.join().unwrap();
    }

    #[test]
    fn relay_preserves_parent_authentication_and_host_denial() {
        use crate::proxy::{EgressProxy, StaticDecider};
        use base64::Engine as _;
        let policy = crate::policy::NetPolicy {
            enforce: true,
            default_effect: crate::policy::Effect::Deny,
            ..Default::default()
        };
        let proxy = EgressProxy::start(Arc::new(StaticDecider::new(policy)), None).unwrap();
        let relay = TestRelay::start(proxy.port());
        for (token, status) in [("wrong-command-token", "407"), (proxy.token(), "403")] {
            let mut socket = TcpStream::connect(([127, 0, 0, 1], relay.port)).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let auth = base64::engine::general_purpose::STANDARD.encode(format!("{token}:"));
            write!(
                socket,
                "CONNECT denied.example:443 HTTP/1.1\r\nProxy-Authorization: Basic {auth}\r\n\r\n"
            )
            .unwrap();
            let mut reply = Vec::new();
            socket.read_to_end(&mut reply).unwrap();
            assert!(String::from_utf8_lossy(&reply).starts_with(&format!("HTTP/1.1 {status}")));
        }
    }

    #[test]
    fn invalid_stream_transitions_and_window_overflow_are_rejected() {
        let (socket, _peer) = sockets();
        let mut stream = Stream::new(socket).unwrap();
        assert!(stream.frame(Frame::control(CREDIT, 1)).is_err());
        for _ in 0..WINDOW {
            stream
                .frame(Frame {
                    kind: DATA,
                    id: 1,
                    bytes: vec![1],
                })
                .unwrap();
        }
        assert!(
            stream
                .frame(Frame {
                    kind: DATA,
                    id: 1,
                    bytes: vec![1]
                })
                .is_err()
        );
        stream.frame(Frame::control(FIN, 1)).unwrap();
        assert!(stream.frame(Frame::control(FIN, 1)).is_err());
    }

    #[test]
    fn parent_rejects_stream_limit_and_reused_ids() {
        for reused in [false, true] {
            let listener = TcpListener::bind(([127, 0, 0, 1], 0)).unwrap();
            let (input, frames) = mpsc::sync_channel(STREAMS + 2);
            input
                .send(Ok(Frame {
                    kind: READY,
                    id: 0,
                    bytes: vec![1, 0],
                }))
                .unwrap();
            for id in 1..=if reused { 2 } else { STREAMS as u64 + 1 } {
                input
                    .send(Ok(Frame::control(OPEN, if reused { 1 } else { id })))
                    .unwrap();
            }
            let (output, _writes) = mpsc::sync_channel(QUEUE);
            let (ready, _receiver) = mpsc::sync_channel(1);
            let result = drive(
                &frames,
                &output,
                Side::Parent {
                    port: listener.local_addr().unwrap().port(),
                    ready,
                },
                &AtomicBool::new(false),
            );
            assert_eq!(
                result.unwrap_err().to_string(),
                "relay stream ID or count limit"
            );
        }
    }

    #[test]
    fn command_proxy_cancellation_does_not_stop_shared_context_sibling() {
        use crate::proxy::{ProxyContext, StaticDecider};
        let context = ProxyContext {
            decider: Arc::new(StaticDecider::new(crate::policy::NetPolicy::default())),
            mitm: None,
        };
        let first = context.start().unwrap();
        let second = context.start().unwrap();
        assert_ne!(first.token(), second.token());
        let first_relay = TestRelay::start(first.port());
        let second_relay = TestRelay::start(second.port());
        drop(first_relay);
        drop(first);
        let mut socket = TcpStream::connect(([127, 0, 0, 1], second_relay.port)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        socket
            .write_all(b"CONNECT denied.example:443 HTTP/1.1\r\n\r\n")
            .unwrap();
        let mut reply = Vec::new();
        socket.read_to_end(&mut reply).unwrap();
        assert!(reply.starts_with(b"HTTP/1.1 407"));
    }

    #[test]
    fn duplicate_ready_unknown_stream_and_early_open_fail_closed() {
        for invalid_frame in [
            Frame {
                kind: READY,
                id: 0,
                bytes: vec![1, 0],
            },
            Frame::control(CLOSE, 1),
            Frame::control(DATA, 1),
        ] {
            let (input, frames) = mpsc::sync_channel(4);
            input
                .send(Ok(Frame {
                    kind: READY,
                    id: 0,
                    bytes: vec![1, 0],
                }))
                .unwrap();
            input.send(Ok(invalid_frame)).unwrap();
            let (output, _writes) = mpsc::sync_channel(4);
            let (ready, _receiver) = mpsc::sync_channel(1);
            assert!(
                drive(
                    &frames,
                    &output,
                    Side::Parent { port: 1, ready },
                    &AtomicBool::new(false)
                )
                .is_err()
            );
        }
        let (input, frames) = mpsc::sync_channel(1);
        input.send(Ok(Frame::control(OPEN, 1))).unwrap();
        let (output, _writes) = mpsc::sync_channel(1);
        let (ready, _receiver) = mpsc::sync_channel(1);
        assert!(
            drive(
                &frames,
                &output,
                Side::Parent { port: 1, ready },
                &AtomicBool::new(false)
            )
            .is_err()
        );
    }

    #[test]
    fn frame_roundtrip_and_truncation() {
        for kind in [DATA, OPEN, FIN, CLOSE, CREDIT, READY] {
            let frame = Frame {
                kind,
                id: if kind == READY { 0 } else { 1 },
                bytes: match kind {
                    DATA => vec![3; PAYLOAD],
                    READY => vec![1, 2],
                    _ => vec![],
                },
            };
            let mut bytes = Vec::new();
            frame.write(&mut bytes).unwrap();
            let parsed = Frame::read(&mut bytes.as_slice()).unwrap();
            assert_eq!(
                (parsed.kind, parsed.id, parsed.bytes),
                (frame.kind, frame.id, frame.bytes)
            );
            for length in [0, 1, 13, bytes.len() - 1] {
                assert!(Frame::read(&mut &bytes[..length]).is_err());
            }
        }
    }

    #[test]
    fn malformed_lengths_versions_and_operations_are_rejected_before_allocation() {
        for (version, kind, id, size) in [
            (2, OPEN, 1u64, 0u32),
            (1, 99, 1, 0),
            (1, OPEN, 0, 0),
            (1, OPEN, 1, 1),
            (1, DATA, 1, u32::MAX),
            (1, DATA, 1, 0),
            (1, READY, 1, 2),
        ] {
            let mut head = vec![version, kind];
            head.extend_from_slice(&id.to_le_bytes());
            head.extend_from_slice(&size.to_le_bytes());
            assert_eq!(
                Frame::read(&mut head.as_slice()).err().unwrap().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }
}
