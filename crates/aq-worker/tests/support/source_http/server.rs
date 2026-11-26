use super::*;
pub(super) struct Server {
    pub(super) addr: std::net::SocketAddr,
    pub(super) state: Arc<Mutex<PeerState>>,
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}
impl Server {
    pub(super) fn start(fixture: Arc<Fixture>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(PeerState::default()));
        let shared = state.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let server = std::thread::spawn(move || {
            for c in listener.incoming() {
                let mut c = c.unwrap();
                if done.load(Ordering::SeqCst) {
                    break;
                }
                c.set_read_timeout(Some(std::time::Duration::from_secs(10)))
                    .unwrap();
                let request = read_request(&c);
                shared.lock().unwrap().route(&mut c, &request, &fixture);
            }
        });
        Self {
            addr,
            state,
            stop,
            thread: server,
        }
    }

    pub(super) fn stop(self) {
        self.stop.store(true, Ordering::SeqCst);
        drop(TcpStream::connect(self.addr).unwrap());
        self.thread.join().unwrap();
    }
}
pub(super) struct Request {
    pub(super) path: String,
    pub(super) body: Vec<u8>,
}
fn read_request(c: &TcpStream) -> Request {
    let mut reader = BufReader::new(c.try_clone().unwrap());
    let mut first = String::new();
    reader.read_line(&mut first).unwrap();
    let mut size = 0;
    let mut auth = false;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        let (name, value) = line.split_once(':').unwrap();
        if name.eq_ignore_ascii_case("content-length") {
            size = value.trim().parse().unwrap();
        }
        if name.eq_ignore_ascii_case("authorization") {
            auth = value.trim() == "Bearer fixture-http-node";
        }
    }
    assert!(auth);
    assert!(size <= MAX_CHUNK_BYTES + 65536);
    let mut body = vec![0; size];
    reader.read_exact(&mut body).unwrap();
    let path = first.split_whitespace().nth(1).unwrap();
    Request {
        path: path.to_owned(),
        body,
    }
}

impl PeerState {
    fn route(&mut self, c: &mut TcpStream, request: &Request, fixture: &Fixture) {
        let path = request.path.as_str();
        let body = request.body.as_slice();
        if path.ends_with("/begin") {
            self.begin(c, body, fixture);
        } else if path.contains('?') {
            self.chunk(c, request, &fixture.central);
        } else if path.ends_with("/finalize") {
            self.finalize(c, body, fixture);
        } else if path == "/v2/worker/result" {
            self.report(c, body);
        } else {
            panic!("unexpected HTTP route {path}");
        }
    }
}
