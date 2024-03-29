mod rwlock;

use axum::body::Body;
use axum::extract::{ConnectInfo, Path};
use axum::http::{header, Request, StatusCode};
use axum::response::Html;
use axum::routing::{delete, patch, post};
use axum::{response::IntoResponse, routing::get, Router};
use axum::{Extension, Json};
use axum_extra::TypedHeader;
use libc::getpwnam;
use rwlock::RwLock;
use serde::de::Visitor;
use simple_ringbuf::RingBuffer;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::fmt;
use std::fs::File;
use std::io::Read;
use std::marker::PhantomData;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::os::fd::{AsFd, AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use timeout_readwrite::TimeoutReader;
use tokio::fs;
use tokio::sync::broadcast::Sender;
use tokio::sync::{broadcast, Mutex};
use tokio::task::yield_now;
use tower::{ServiceBuilder, ServiceExt};
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use serde::{Deserialize, Deserializer, Serialize};

static FD_TIMEOUT: u32 = 250_000_000;
static COUNTER: AtomicUsize = AtomicUsize::new(1);
fn get_id() -> usize {
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[derive(Serialize, Deserialize, Default)]
struct SavedState {
    processes: Processes,
    counter: usize,
}

#[derive(Serialize, Deserialize, Debug)]
struct Process {
    name: String,
    dir: String,
    command: String,
    id: usize,
    user: String,
    #[serde(skip)]
    log: RwLock<VecDeque<u8>>,
    status: RwLock<Status>,
    timestamp: RwLock<u128>,
    #[serde(skip)]
    pid: AtomicU32,
    #[serde(skip)]
    tx: Arc<RwLock<Option<Sender<Vec<u8>>>>>,
    autostart: AtomicBool,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
enum Status {
    Running,
    Exited(i32),
}

type Processes = RwLock<HashMap<usize, Arc<Process>>>;
struct GState {
    proccess: Processes,
}

#[tokio::main]
async fn main() {
    // Setup logging & RUST_LOG from args
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", format!("{},hyper=info,mio=info", 1))
    }
    // tracing_subscriber::fmt()
    //     .with_max_level(tracing::Level::DEBUG)
    //     .init();
    // enable console logging
    tracing_subscriber::fmt::init();

    let savedstate = match fs::read_to_string("./cfg.json").await {
        Ok(txt) => serde_json::from_str(&txt).unwrap(),
        Err(_) => SavedState::default(),
    };

    for process in savedstate.processes.0.read().await.values() {
        if process.autostart.load(Ordering::Relaxed) {
            spawn_child_thread(process.clone()).await;
        } else if matches!(*process.status.0.read().await, Status::Running) {
            *process.status.0.write().await = Status::Exited(-1);
        }
    }

    COUNTER.store(savedstate.counter, Ordering::Relaxed);
    let state = GState {
        proccess: savedstate.processes,
    };

    let app = Router::new()
        .route("/api/new", post(new))
        .route("/api/list", get(list))
        .route("/api/:id", get(gets))
        .route("/api/:id", patch(patches))
        .route("/api/:id", delete(deletes))
        .route("/api/:id/restart", post(restart))
        .route("/api/:id/kill", post(kill))
        .route("/api/:id/tail", get(websocket))
        .layer(Extension(Arc::new(state)));
    // .layer(ServiceBuilder::new().layer(TraceLayer::new_for_http()));

    // let sock_addr = SocketAddr::from((
    //     IpAddr::from_str(opt.addr.as_str()).unwrap_or(IpAddr::V6(Ipv6Addr::LOCALHOST)),
    //     opt.port,
    // ));

    // run it with hyper
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8232").await.unwrap();
    tracing::debug!("listening on {}", listener.local_addr().unwrap());
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .unwrap();
}

async fn handle_socket(mut socket: WebSocket, who: SocketAddr, process: Arc<Process>) {
    let tx = process.tx.clone().0.read().await.clone().unwrap();
    let mut rx = tx.subscribe();

    loop {
        let text = rx.recv().await.unwrap();
        socket.send(Message::Binary(text)).await.unwrap();
    }
}
async fn websocket(
    Extension(state): Extension<Arc<GState>>,
    Path(id): Path<usize>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    ws: WebSocketUpgrade,
    user_agent: Option<TypedHeader<axum_extra::headers::UserAgent>>,
) -> impl IntoResponse {
    let processes = state.proccess.0.read().await;
    let process = processes.get(&id).unwrap().clone();

    ws.on_upgrade(move |socket| handle_socket(socket, addr, process))
}

async fn gets(
    Extension(state): Extension<Arc<GState>>,
    Path(id): Path<usize>, // Extension(callbacks): Extension<Arc<RwLock<Callbacks>>>,
                           // ConnectInfo(addr): ConnectInfo<SocketAddr>,
                           // Query(params): Query<LoginParams>,
) -> impl IntoResponse {
    let processes = state.proccess.0.read().await;
    let log = processes.get(&id).unwrap().log.0.read().await;
    // Json(log.clone())
    String::from_utf8(Vec::from(log.clone())).unwrap()
}

async fn killproc(proc: Arc<Process>) {
    proc.autostart.store(false, Ordering::Relaxed);
    let pid = proc.pid.load(Ordering::Relaxed);
    if pid == 0 {
        // don't kill the parent
        return;
    }
    unsafe { libc::kill(pid as i32, 9) };
}
async fn kill(
    Extension(state): Extension<Arc<GState>>,
    Path(id): Path<usize>,
) -> impl IntoResponse {
    let procs = state.proccess.0.read().await;
    let proc = procs.get(&id).unwrap();

    killproc(proc.clone()).await;
    ""
}

async fn restart(
    Extension(state): Extension<Arc<GState>>,
    Path(id): Path<usize>,
) -> impl IntoResponse {
    let procs = state.proccess.0.read().await;
    let proc = procs.get(&id).unwrap();

    *proc.status.0.write().await = Status::Running;
    spawn_child_thread(proc.clone()).await;

    ""
}

async fn deletes(
    Extension(state): Extension<Arc<GState>>,
    Path(id): Path<usize>,
) -> impl IntoResponse {
    let mut processes = state.proccess.0.write().await;
    processes.remove(&id);

    ""
}

async fn patches(
    Extension(state): Extension<Arc<GState>>,

    Path(id): Path<usize>,
    Json(payload): Json<NewRequest>, // Extension(callbacks): Extension<Arc<RwLock<Callbacks>>>
) -> impl IntoResponse {
    let mut procs = state.proccess.0.write().await;

    let proc = procs.get(&id).unwrap();
    killproc(proc.clone()).await;
    procs.remove(&id);

    drop(procs);

    new(Extension(state), Json(payload)).await
}

#[derive(Debug, Serialize, Deserialize)]
struct ListResponse {
    command: String,
    id: usize,
    exited: Option<i32>,
}

async fn list(
    Extension(state): Extension<Arc<GState>>,
    // Extension(callbacks): Extension<Arc<RwLock<Callbacks>>>,
    // ConnectInfo(addr): ConnectInfo<SocketAddr>,
    // Query(params): Query<LoginParams>,
) -> impl IntoResponse {
    let processes = state.proccess.0.read().await;

    // let mut resp = vec![];

    // for (i, process) in processes.iter() {
    //     resp.push(ListResponse {
    //         command: process.command.clone(),
    //         id: *i,
    //         exited: match process.status.0.read().await.clone() {
    //             Status::Exited(status) => Some(status),
    //             Status::Running => None,
    //         },
    //     })
    // }

    Json(processes.clone())
}

#[derive(Debug, Serialize, Deserialize)]
struct NewRequest {
    command: String,
    user: String,
    name: String,
    dir: String,
    autostart: bool,
}
fn timestamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

#[derive(Debug)]
struct NoUserError;
impl fmt::Display for NoUserError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "User doesn't exist!")
    }
}
fn uid_from_username(username: &str) -> Result<u32, NoUserError> {
    unsafe {
        let cstr = CString::new(username).unwrap();
        let ptr = getpwnam(cstr.as_ptr());
        if ptr.is_null() {
            return Err(NoUserError);
        } else {
            return Ok((*ptr).pw_uid);
        }
    }
}

async fn new(
    Extension(state): Extension<Arc<GState>>,
    Json(payload): Json<NewRequest>, // Extension(callbacks): Extension<Arc<RwLock<Callbacks>>>,
                                     // ConnectInfo(addr): ConnectInfo<SocketAddr>,
                                     // Query(params): Query<LoginParams>,
) -> impl IntoResponse {
    let id = get_id();

    // to validate
    uid_from_username(&payload.user).unwrap();

    let process = Process {
        name: payload.name,
        dir: payload.dir,
        command: payload.command,
        user: payload.user,
        id,
        log: RwLock::new(VecDeque::new()),
        status: RwLock::new(Status::Running),
        pid: AtomicU32::new(0),
        tx: Arc::new(RwLock::new(None)),
        timestamp: RwLock::new(timestamp()),
        autostart: payload.autostart.into(),
    };

    let procref = Arc::new(process);

    spawn_child_thread(procref.clone()).await;
    state.proccess.0.write().await.insert(id, procref);

    save(state.clone()).await;
    format!("{}", id)
}
async fn spawn_child_thread(process: Arc<Process>) {
    let (tx, _) = broadcast::channel(16);
    *process.tx.0.write().await = Some(tx);
    tokio::spawn(async move {
        loop {
            let mut s = Command::new("/usr/bin/env")
                .arg("script")
                .arg("-q") // quiet
                .arg("-c")
                .arg(process.command.clone())
                .arg("/dev/null")
                .env("SHELL", "/bin/bash") // this is slightly rancid, will fix later
                .env("TERM", "xterm") // remember on the other end we have an xterm capable emulator
                .current_dir(process.dir.clone())
                .uid(uid_from_username(&process.user).unwrap())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn();
            let Ok(mut child) = s else {
                let mut status = process.status.0.write().await;
                *status = Status::Exited(-1);
                *process.log.0.write().await = VecDeque::from(
                    "Permission Denied while spawning child (did you use a privledged user?)"
                        .as_bytes()
                        .to_vec(),
                );
                return;
            };
            process.pid.store(child.id(), Ordering::Relaxed);

            let fd = child.stdout.as_mut().unwrap().as_raw_fd().clone();
            let fderr = child.stderr.as_mut().unwrap().as_raw_fd().clone();

            let stdout = unsafe { File::from_raw_fd(fd) };
            let stderr = unsafe { File::from_raw_fd(fderr) };
            *process.timestamp.0.write().await = timestamp();

            // time out so we have a chance to respond to exit
            let mut rdr = TimeoutReader::new(stdout, Duration::new(0, FD_TIMEOUT));
            let mut rdrerr = TimeoutReader::new(stderr, Duration::new(0, FD_TIMEOUT));
            loop {
                let mut buf = [0; 1024];
                let bytes = rdr.read(&mut buf).unwrap_or(0);

                if bytes > 0 {
                    let out = &buf[0..bytes];
                    let mut writer = process.log.0.write().await;

                    let tx = process.tx.clone().0.read().await.clone().unwrap();
                    if tx.receiver_count() > 0 {
                        tx.send(out.to_vec()).unwrap();
                    }
                    for c in out.to_vec() {
                        writer.push_back(c);
                        if writer.len() > 4000 {
                            writer.pop_front();
                        }
                    }
                }

                let bytes = rdrerr.read(&mut buf).unwrap_or(0);
                if bytes > 0 {
                    let out = &buf[0..bytes];
                    let mut writer = process.log.0.write().await;

                    let tx = process.tx.clone().0.read().await.clone().unwrap();
                    if tx.receiver_count() > 0 {
                        tx.send(out.to_vec()).unwrap();
                    }

                    for c in out.to_vec() {
                        writer.push_back(c);
                        if writer.len() > 4000 {
                            writer.pop_front();
                        }
                    }
                }

                match child.try_wait() {
                    Ok(Some(exit)) => {
                        let mut status = process.status.0.write().await;
                        *status = Status::Exited(exit.code().unwrap_or(-1));
                        // *process.timestamp.0.write().await = timestamp();
                        break;
                    }
                    Ok(None) => (),
                    Err(e) => panic!("{}", e),
                }

                // we're polling, give some time back to the scheduler
                yield_now().await;
            }
            if !process.autostart.load(Ordering::Acquire) {
                break;
            }
            yield_now().await;
            if timestamp() - process.timestamp.0.read().await.clone() < 30000 {
                process.tx.clone().0.read().await.clone().unwrap().send(
                    "[pm] !!!! exited within 30 seconds of starting, will not restart !!!!\n"
                        .as_bytes()
                        .to_vec(),
                );
                let mut writer = process.log.0.write().await;

                for c in "[pm] !!!! exited within 30 seconds of starting, will not restart !!!!\n"
                    .as_bytes()
                    .to_vec()
                {
                    writer.push_back(c);
                }
                break;
            } else {
                process.tx.clone().0.read().await.clone().unwrap().send(
                    "[pm] autostart was on, restarting process\n"
                        .as_bytes()
                        .to_vec(),
                );
                let mut writer = process.log.0.write().await;

                for c in "[pm] autostart was on, restarting process\n"
                    .as_bytes()
                    .to_vec()
                {
                    writer.push_back(c);
                }
            }
        }
    });
}
// async fn child_thread(process: Arc<Process>) {}
async fn save(state: Arc<GState>) {
    let procs = state.proccess.0.read().await;

    let saved = SavedState {
        processes: RwLock::new(procs.clone()),
        counter: COUNTER.load(Ordering::Relaxed),
    };
    let str = serde_json::to_string(&saved).unwrap();
    fs::write("./cfg.json", str).await.unwrap();
}
