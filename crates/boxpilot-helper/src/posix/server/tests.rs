//! The accept loop over a real Unix socket, with the core over the fake
//! supervisor (`testing`).

use super::*;
use crate::helper::HelperCore;
use crate::testing::{FakeSupervisor, Script, TempDir};
use boxpilot_protocol::{
    decode_to_client, encode_request, Authority, FrameDecoder, Limits, Reply, Request, ToClient,
};
use std::io::{Read, Write};
use std::path::PathBuf;

/// A socket's client end, speaking the protocol as the GUI does.
struct Client {
    stream: UnixStream,
    decoder: FrameDecoder,
    unread: Vec<u8>,
}

impl Client {
    fn connect(path: &PathBuf) -> Self {
        let stream = UnixStream::connect(path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        Self {
            stream,
            decoder: FrameDecoder::new(Limits::default().to_gui_caps()),
            unread: Vec::new(),
        }
    }

    fn send(&mut self, request: &Request) {
        let bytes = encode_request(request, &Limits::default()).unwrap();
        self.stream.write_all(&bytes).unwrap();
    }

    /// The next message by `timeout`: `Ok(None)` at the end of the stream,
    /// `Err` when nothing came.
    fn next(&mut self, timeout: Duration) -> Result<Option<ToClient>, ()> {
        let deadline = Instant::now() + timeout;
        let mut buf = [0u8; 4096];
        loop {
            let taken = self.decoder.feed(&self.unread);
            self.unread.drain(..taken);
            if let Some(frame) = self.decoder.next_frame().unwrap() {
                return Ok(Some(decode_to_client(&frame).unwrap()));
            }
            match self.stream.read(&mut buf) {
                Ok(0) => return Ok(None),
                Ok(n) => self.unread.extend_from_slice(&buf[..n]),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    if Instant::now() >= deadline {
                        return Err(());
                    }
                }
                Err(_) => return Ok(None),
            }
        }
    }

    fn hello(&mut self) -> bool {
        self.send(&Request::hello());
        match self.next(Duration::from_secs(10)) {
            Ok(Some(ToClient::Reply(Reply::Hello(hello)))) => hello.may_start,
            other => panic!("hello was answered {other:?}"),
        }
    }

    fn status(&mut self) {
        self.send(&Request::Status);
        match self.next(Duration::from_secs(10)) {
            Ok(Some(ToClient::Reply(Reply::Status { .. }))) => {}
            other => panic!("status was answered {other:?}"),
        }
    }
}

/// The loop on its own thread over a socket in a temporary directory.
struct Served {
    _temp: TempDir,
    path: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<Stopped>>,
    core: Arc<HelperCore<FakeSupervisor>>,
}

impl Served {
    fn new(config: ServerConfig, authority: Authority) -> Self {
        // Short names: a socket's path holds 103 bytes on macOS, whose
        // $TMPDIR (under /var/folders) is long already.
        let temp = TempDir::new("s");
        let path = temp.0.join("s");
        let listener = UnixListener::bind(&path).unwrap();
        let core = Arc::new(HelperCore::new(FakeSupervisor::new(), Limits::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let identify: Identify = Arc::new(move |stream: &UnixStream| {
            let uid = super::super::peer::peer_uid(stream).unwrap();
            Caller {
                authority,
                user: Some(uid.to_string()),
            }
        });
        let thread = {
            let (core, stop) = (core.clone(), stop.clone());
            thread::spawn(move || run(&listener, core, identify, &stop, config))
        };
        Self {
            _temp: temp,
            path,
            stop,
            thread: Some(thread),
            core,
        }
    }

    fn connect(&self) -> Client {
        Client::connect(&self.path)
    }

    fn stop(mut self) -> Stopped {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap()
    }
}

fn quick() -> ServerConfig {
    ServerConfig {
        tick: Duration::from_millis(20),
        idle_exit: Duration::from_secs(600),
        ..ServerConfig::default()
    }
}

#[test]
fn a_client_is_served_by_its_authority() {
    let served = Served::new(quick(), Authority::MayStart);
    assert!(served.connect().hello());
    let served_read_only = Served::new(quick(), Authority::ReadOnly);
    let mut client = served_read_only.connect();
    assert!(!client.hello());
    client.send(&Request::Stop);
    assert!(matches!(
        client.next(Duration::from_secs(10)),
        Ok(Some(ToClient::Reply(Reply::Error { .. })))
    ));
    assert!(matches!(served.stop(), Stopped::Asked));
    assert!(matches!(served_read_only.stop(), Stopped::Asked));
}

#[test]
fn the_idle_helper_stops_and_a_connection_keeps_it_up() {
    let config = ServerConfig {
        idle_exit: Duration::from_millis(400),
        ..quick()
    };
    let mut served = Served::new(config, Authority::MayStart);
    let mut client = served.connect();
    assert!(client.hello());
    // Held open past the idle timeout: not idle.
    thread::sleep(Duration::from_millis(800));
    assert!(!served.thread.as_ref().unwrap().is_finished());
    client.status();
    drop(client);
    let began = Instant::now();
    let thread = served.thread.take().unwrap();
    while !thread.is_finished() {
        assert!(began.elapsed() < Duration::from_secs(10), "never idle");
        thread::sleep(Duration::from_millis(20));
    }
    assert!(matches!(thread.join().unwrap(), Stopped::Idle));
}

/// A running sing-box keeps the helper up with no connection... but there
/// is none without one: it stops with its connection. The core's idle
/// check is what the loop asks.
#[test]
fn stopping_stops_sing_box() {
    let served = Served::new(quick(), Authority::MayStart);
    served.core.supervisor().set_script(Script::default());
    let mut client = served.connect();
    assert!(client.hello());
    client.send(&Request::Start(boxpilot_protocol::StartRequest {
        config: "{}".into(),
        attachments: Vec::new(),
        options: boxpilot_protocol::TunOptions {
            ipv6: false,
            proxy_port: 1,
            allow_lan: false,
            system_proxy: false,
        },
    }));
    assert!(matches!(
        client.next(Duration::from_secs(10)),
        Ok(Some(ToClient::Reply(Reply::Started(_))))
    ));
    assert!(!served.core.is_idle());
    let core = served.core.clone();
    assert!(matches!(served.stop(), Stopped::Asked));
    assert!(core.is_idle());
    assert_eq!(core.supervisor().stops.load(Ordering::SeqCst), 1);
}

/// Full: the next client waits, unanswered, until one ends.
#[test]
fn a_client_past_the_limit_waits_for_a_slot() {
    let config = ServerConfig {
        max_connections: 2,
        ..quick()
    };
    let served = Served::new(config, Authority::MayStart);
    let mut first = served.connect();
    assert!(first.hello());
    let mut second = served.connect();
    assert!(second.hello());
    let mut third = served.connect();
    third.send(&Request::hello());
    assert_eq!(third.next(Duration::from_millis(500)), Err(()));
    drop(first);
    assert!(matches!(
        third.next(Duration::from_secs(10)),
        Ok(Some(ToClient::Reply(Reply::Hello(_))))
    ));
    second.status();
    served.stop();
}

/// Read-only callers get their share, and no more: the next one is closed
/// at once.
#[test]
fn read_only_clients_are_capped() {
    let config = ServerConfig {
        max_read_only: 2,
        ..quick()
    };
    let served = Served::new(config, Authority::ReadOnly);
    let mut held: Vec<Client> = (0..2).map(|_| served.connect()).collect();
    for client in &mut held {
        assert!(!client.hello());
    }
    let mut extra = served.connect();
    let _ = extra
        .stream
        .write_all(&encode_request(&Request::hello(), &Limits::default()).unwrap());
    assert_eq!(extra.next(Duration::from_secs(10)), Ok(None));
    drop(held.pop());
    // A slot freed: served again.
    let began = Instant::now();
    loop {
        let mut again = served.connect();
        again.send(&Request::hello());
        match again.next(Duration::from_secs(10)) {
            Ok(Some(ToClient::Reply(Reply::Hello(_)))) => break,
            _ => assert!(began.elapsed() < Duration::from_secs(10)),
        }
        thread::sleep(Duration::from_millis(20));
    }
    served.stop();
}

/// A helper that refuses to run closes its waiting clients before it
/// exits.
#[test]
fn waiting_clients_are_turned_away() {
    let temp = TempDir::new("t");
    let path = temp.0.join("s");
    let listener = UnixListener::bind(&path).unwrap();
    let mut clients: Vec<Client> = (0..3).map(|_| Client::connect(&path)).collect();
    assert_eq!(turn_away_waiting(&listener), 3);
    for client in &mut clients {
        assert_eq!(client.next(Duration::from_secs(5)), Ok(None));
    }
    assert_eq!(turn_away_waiting(&listener), 0);
}

/// The log names the socket's path, without the NULs launchd's `sun_path`
/// carries after it.
#[test]
fn the_listening_path_ends_at_its_first_nul() {
    let launchd = OsStr::from_bytes(b"/var/run/io.github.glide01.boxpilot.helper.sock\0\0\0\0");
    assert_eq!(
        before_nul(Path::new(launchd)),
        Path::new("/var/run/io.github.glide01.boxpilot.helper.sock")
    );
    assert_eq!(
        before_nul(Path::new("/var/run/a.sock")),
        Path::new("/var/run/a.sock")
    );
    let temp = TempDir::new("t");
    let path = temp.0.join("s");
    let listener = UnixListener::bind(&path).unwrap();
    assert_eq!(listening_path(&listener), Some(path));
}
