//! 데몬(`mine daemon`). 세션 목록과 화면을 쥐고 보관 프로세스와 클라이언트 사이를 잇는다.
//!
//! - 보관 프로세스(`mine hold`)마다 연결 하나에 읽기 작업과 쓰기 작업을 하나씩 둔다. 출력은 세션 `Screen`에
//!   먹이고 그 세션을 보는 클라이언트에 보낸다. 안쪽 앱의 터미널 질의에 대한 답은 데몬이 보관 프로세스로
//!   돌려보낸다(붙은 클라이언트가 없어도 답한다).
//! - 클라이언트마다 읽기 작업과 쓰기 작업을 하나씩 둔다. 보낼 메시지는 `QUEUE_CAP`개까지 쌓고, 넘치면 끊는다.
//! - `List`를 보낸 클라이언트는 그 뒤로 목록이 바뀔 때마다 `Sessions`를 받는다. `List`·`New`·`Kill`에는
//!   요청한 클라이언트에 `Sessions`(또는 `Error`)로 직접 답한다. 구독하지 않은 헤드리스 연결이 다른 사건의
//!   알림을 자기 요청의 답으로 잘못 읽지 않게 하려는 것이다.
//! - 클라이언트가 `Colors`로 바깥 터미널의 기본색을 알려 주면 모든 세션 화면(앞으로 붙일 화면 포함)이
//!   안쪽 앱의 색 질의에 그 색으로 답한다.
//! - 데몬이 다시 뜨면 `sessions/*/meta.json`을 훑어 살아 있는 보관 프로세스에 다시 붙고, 보관 프로세스의
//!   출력 버퍼로 80x24 화면을 다시 만든다. 첫 `Attach`가 클라이언트 크기로 바꾼다.
//!
//! `proto::recv`는 취소에 안전하지 않으므로 `select!`에 넣지 않는다. 연결마다 읽기 작업 하나가 차례로 읽는다.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write as _};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alacritty_terminal::vte::ansi::Rgb;
use anyhow::{Context, Result, bail};
use tokio::io::BufReader;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::AbortHandle;
use tokio::time::timeout;

use crate::hold;
use crate::hooks::{self, Event, EventKind, now_ms};
use crate::paths::{State, check_sock_path, ensure_dir};
use crate::project::{self, project_key};
use crate::proto::{FromDaemon, HOLD_VERSION, HoldIn, HoldOut, ToDaemon, VERSION, recv, send};
use crate::screen::Screen;
use crate::session::{
    self, Kind, Meta, NewSession, Restore, SessionInfo, SessionState, session_id, validate_name,
};

/// 클라이언트 하나에 쌓아 둘 수 있는 보낼 메시지 수. 넘치면 그 클라이언트를 끊는다.
const QUEUE_CAP: usize = 1024;
/// 보관 프로세스의 종료 코드 파일 이름(세션 폴더 안).
const EXIT_FILE: &str = "exit";
/// 사용자가 세션을 마지막으로 본 시각(유닉스 밀리초) 파일 이름(세션 폴더 안).
const VIEWED_FILE: &str = "viewed";
/// 새로 띄운 보관 프로세스의 PTY 크기이자, 데몬이 다시 떴을 때 화면을 다시 만드는 크기.
const INITIAL_SIZE: (u16, u16) = (80, 24);
/// `Kill` 뒤 보관 프로세스 연결이 끝나기를 기다리는 최대 시간. 보관 프로세스는 SIGHUP을 보내고 `KILL_GRACE`가
/// 지나면 SIGKILL을 보낸 뒤, 남은 출력을 `DRAIN_GRACE`까지 읽고 끝난다. 그 위에 여유를 둔다.
const KILL_WAIT: Duration = hold::KILL_GRACE
    .saturating_add(hold::DRAIN_GRACE)
    .saturating_add(Duration::from_millis(1500));
/// shell 세션이 출력 없이 이만큼 지나면 작업 중에서 대기로 돌아간다(설계 5.2).
const SHELL_QUIET: Duration = Duration::from_secs(2);
/// 띄운 보관 프로세스가 연결을 받기까지 기다리는 최대 시간.
const START_WAIT: Duration = Duration::from_secs(5);
/// 보관 프로세스 연결에서 `Hello`와 출력 버퍼를 기다리는 최대 시간.
const LINK_WAIT: Duration = Duration::from_secs(5);
/// 이보다 짧게 이어졌던 보관 프로세스 연결이 끊기면 다시 붙지 않고 끝난 것으로 본다(붙었다 끊기기를 되풀이하지 않게).
const RELINK_MIN_AGE: Duration = Duration::from_secs(1);
/// 다른 데몬이 막 끝나는 중이면 pid 파일 잠금이 풀리기를 이만큼 기다린다.
const PID_LOCK_WAIT: Duration = Duration::from_secs(1);
/// 보관 프로세스 연결의 읽기 버퍼 크기. 밀린 출력을 한 번에 모을 수 있는 양을 정한다.
const HOLD_READ_BUF: usize = 64 * 1024;
/// 한 번에 모아 화면에 먹이는 출력의 최대 바이트 수. 잠금 횟수를 줄이는 효과는 수십 바이트짜리 조각을
/// 합치는 데서 나오므로 작게 둔다.
// ponytail: 출력 메시지 하나는 BATCH_MAX 미만에 보관 프로세스 읽기 한 번(최대 16 KiB)을 더해 32 KiB 미만이고,
// 클라이언트 큐는 1024개라 처진 클라이언트 하나가 데몬에 쌓는 출력은 32 MiB 이하다(스냅숏은 따로).
// 더 줄이려면 큐를 바이트로 센다.
const BATCH_MAX: usize = 16 * 1024;

// ponytail: 전역 잠금, 세션이 수백 개가 되면 세션별 잠금으로
type Shared = Arc<Mutex<Daemon>>;

/// 데몬 전체 상태.
struct Daemon {
    state: State,
    exe: PathBuf,
    sessions: BTreeMap<String, Sess>,
    /// `New`가 보관 프로세스를 띄우는 중인 세션 id. 같은 이름을 동시에 두 번 만들지 않게 한다.
    starting: HashSet<String>,
    /// `Kill`로 닫았지만 보관 프로세스 연결이 아직 끝나지 않은 세션. 끝날 때까지 같은 id로 `New`를 받지 않는다.
    closing: HashMap<String, Closing>,
    clients: HashMap<u64, Client>,
    next_id: u64,
    /// 구독한 클라이언트에 마지막으로 알린 목록. 바뀌었을 때만 다시 알린다.
    last: Vec<SessionInfo>,
    /// 클라이언트가 알려 준 바깥 터미널의 기본 전경·배경색. 새로 만드는 화면에도 쓴다.
    colors: Option<(Rgb, Rgb)>,
}

/// 세션 하나.
struct Sess {
    meta: Meta,
    screen: Screen,
    /// 화면과 PTY의 지금 크기.
    size: (u16, u16),
    /// 보관 프로세스 연결. 프로세스가 끝났거나 붙지 못한 세션이면 `None`이다.
    hold: Option<Hold>,
    /// 보관 프로세스에 붙지 못했지만(시간 초과, 버전 불일치 등) 살아 있을 수 있다. 끝난 것으로 보지 않고
    /// 다시 띄우지도 않는다. 다음 `Attach`가 다시 붙어 본다.
    unlinked: bool,
    state: SessionState,
    waiting: Option<String>,
    resume_id: Option<String>,
    viewed_ms: Option<u64>,
    /// 동기화 갱신 시간 제한을 지켜보는 작업이 있다.
    sync_timer: bool,
    /// shell 세션이 마지막으로 출력한 시각. 출력 없이 `SHELL_QUIET`가 지나면 `None`으로 돌린다. 그동안 작업 중이다.
    active: Option<Instant>,
    /// 보관 프로세스를 다시 띄우거나 다시 붙는 중이다.
    busy: bool,
}

/// 보관 프로세스 연결. `link`는 연결마다 새로 매겨, 이전 연결 작업이 보낸 사건을 걸러 낸다.
struct Hold {
    link: u64,
    tx: mpsc::UnboundedSender<HoldIn>,
    since: Instant,
}

/// 닫는 중인 세션의 보관 프로세스 연결.
///
/// 연결을 쥐고 있어야 보관 프로세스가 입력 EOF를 보고 연결을 먼저 끊지 않고, 자식이 끝난 뒤 `Exited`를 보낸다.
/// 버리면 `_done`이 닫혀 `Kill`을 기다리는 쪽이 깨어난다.
struct Closing {
    hold: Hold,
    _done: oneshot::Sender<()>,
}

struct Client {
    tx: mpsc::Sender<FromDaemon>,
    writer: AbortHandle,
    /// `List`를 보내 목록 변경을 받는다.
    subscribed: bool,
    attached: Option<String>,
    /// 이 클라이언트가 마지막으로 알린 크기(설계 5.1: 세션 크기는 마지막으로 입력한 클라이언트를 따른다).
    size: Option<(u16, u16)>,
}

/// 막 붙은 보관 프로세스 연결. `Hello`와 출력 버퍼까지 읽은 상태다.
struct Link {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    ring: Vec<u8>,
}

/// 보관 프로세스를 띄운 결과.
enum Started {
    Linked(Link),
    /// 붙기 전에 자식이 이미 끝났다(exit 파일이 있다).
    Ended,
}

/// 데몬을 실행한다. 시작할 때 `daemon.pid`를 잠그고 pid를 쓴다. 돌아오지 않는다(오류가 나면 돌아온다).
pub async fn run(state: State) -> Result<()> {
    for dir in [
        state.root.clone(),
        state.sessions_dir(),
        state.run_dir(),
        state.log_dir(),
    ] {
        ensure_dir(&dir).with_context(|| format!("폴더를 만들 수 없다: {}", dir.display()))?;
    }
    let _pid = lock_pid_file(&state)?;
    let sock = state.daemon_sock();
    check_sock_path(&sock)?;
    // pid 잠금을 잡았으니 남은 소켓 파일은 죽은 데몬의 것이다.
    match fs::remove_file(&sock) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e).with_context(|| format!("남은 소켓을 지울 수 없다: {}", sock.display()));
        }
    }
    let listener = UnixListener::bind(&sock)
        .with_context(|| format!("소켓을 만들지 못했다: {}", sock.display()))?;
    fs::set_permissions(&sock, fs::Permissions::from_mode(0o600))?;
    let exe = std::env::current_exe().context("실행 파일 경로를 알 수 없다")?;
    let shared: Shared = Arc::new(Mutex::new(Daemon {
        state,
        exe,
        sessions: BTreeMap::new(),
        starting: HashSet::new(),
        closing: HashMap::new(),
        clients: HashMap::new(),
        next_id: 0,
        last: Vec::new(),
        colors: None,
    }));
    restore(&shared).await;
    eprintln!("데몬 시작: pid {}", std::process::id());
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(serve_client(Arc::clone(&shared), stream));
            }
            Err(e) => {
                // EMFILE 같은 일시 오류에서 바쁜 반복을 피한다.
                eprintln!("연결을 받지 못했다: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// `daemon.pid`를 잠그고 pid를 쓴다. 잠금은 돌려준 파일이 살아 있는 동안(데몬이 끝날 때까지) 유지된다.
fn lock_pid_file(state: &State) -> Result<File> {
    let path = state.daemon_pid();
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("pid 파일을 열 수 없다: {}", path.display()))?;
    let deadline = Instant::now() + PID_LOCK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(TryLockError::WouldBlock) => {
                bail!("다른 데몬이 이미 돌고 있다({})", path.display())
            }
            Err(TryLockError::Error(e)) => {
                return Err(e)
                    .with_context(|| format!("pid 파일을 잠글 수 없다: {}", path.display()));
            }
        }
    }
    file.set_len(0)?;
    write!(file, "{}", std::process::id())?;
    file.flush()?;
    Ok(file)
}

/// 닫지 않은 세션을 되살린다. 보관 프로세스에 붙으면 살아 있는 세션이고, 소켓이 없거나 연결을 거절하면
/// 끝난 세션이다(끝난 셸 세션은 목록에서 빼고 `closed=true`로 저장한다). 그 밖의 실패는 보관 프로세스가 살아
/// 있을 수 있으므로 세션을 그대로 남기고 오류를 기록한다.
async fn restore(shared: &Shared) {
    let state = shared.lock().await.state.clone();
    let Ok(entries) = fs::read_dir(state.sessions_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        let Ok(mut meta) = Meta::load(&dir) else {
            continue;
        };
        if meta.closed {
            continue;
        }
        let link = connect_holder(&state.hold_sock(&meta.id)).await;
        let mut d = shared.lock().await;
        let id = meta.id.clone();
        match link {
            Ok(Some(link)) => {
                d.sessions
                    .insert(id.clone(), Sess::new(meta, read_viewed(&dir)));
                install(&mut d, shared, &id, link, false);
            }
            Ok(None) if meta.kind == Kind::Shell => {
                eprintln!("끝난 셸 세션을 닫는다: {id}");
                meta.closed = true;
                if let Err(e) = meta.save(&dir) {
                    eprintln!("{e:#}");
                }
            }
            Ok(None) => {
                d.sessions
                    .insert(id.clone(), Sess::new(meta, read_viewed(&dir)));
                d.refresh(&id);
            }
            Err(e) => {
                d.sessions
                    .insert(id.clone(), Sess::new(meta, read_viewed(&dir)));
                d.unlink(&id, &format!("{e:#}"));
            }
        }
    }
    shared.lock().await.publish();
}

fn read_viewed(dir: &Path) -> Option<u64> {
    fs::read_to_string(dir.join(VIEWED_FILE))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// 클라이언트 연결 하나를 처리한다. 첫 메시지는 같은 버전의 `Hello`여야 한다.
async fn serve_client(shared: Shared, stream: UnixStream) {
    let (read, mut write) = stream.into_split();
    let (tx, mut rx) = mpsc::channel::<FromDaemon>(QUEUE_CAP);
    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if send(&mut write, &msg).await.is_err() {
                break;
            }
        }
    });
    let client = shared.lock().await.add_client(tx, writer.abort_handle());
    let mut reader = BufReader::new(read);
    let refused = match recv::<_, ToDaemon>(&mut reader).await {
        Ok(Some(ToDaemon::Hello { version })) if version == VERSION => None,
        Ok(Some(ToDaemon::Hello { version })) => Some(format!(
            "프로토콜 버전이 다르다(클라이언트 {version}, 데몬 {VERSION}). 데몬을 다시 띄운다"
        )),
        Ok(Some(_)) => Some("첫 메시지는 Hello여야 한다".to_string()),
        Ok(None) | Err(_) => Some(String::new()),
    };
    match refused {
        None => {
            while let Ok(Some(msg)) = recv::<_, ToDaemon>(&mut reader).await {
                if !handle(&shared, client, msg).await {
                    break;
                }
            }
        }
        Some(message) if !message.is_empty() => {
            shared
                .lock()
                .await
                .send(client, FromDaemon::Error { message });
        }
        Some(_) => {}
    }
    // 송신자를 버리면 쓰기 작업이 남은 메시지(오류 포함)를 마저 보내고 연결을 닫는다.
    shared.lock().await.clients.remove(&client);
}

/// 메시지 하나를 처리한다. 그 사이 클라이언트가 끊겼으면 `false`다.
async fn handle(shared: &Shared, client: u64, msg: ToDaemon) -> bool {
    match msg {
        ToDaemon::Hello { .. } => {}
        ToDaemon::List => shared.lock().await.list(client),
        ToDaemon::New { spec } => new_session(shared, client, spec).await,
        ToDaemon::Attach { id, cols, rows } => attach(shared, client, &id, (cols, rows)).await,
        ToDaemon::Input { id, data } => shared.lock().await.input(client, &id, data),
        ToDaemon::Resize { id, cols, rows } => {
            shared.lock().await.resize(client, &id, (cols, rows))
        }
        ToDaemon::Kill { id } => kill(shared, client, &id).await,
        ToDaemon::Poke { id } => {
            let mut d = shared.lock().await;
            d.refresh(&id);
            d.publish();
        }
        ToDaemon::Colors { fg, bg } => shared.lock().await.set_colors(&fg, &bg),
    }
    shared.lock().await.clients.contains_key(&client)
}

/// `New`: 세션을 만들고 보관 프로세스를 띄워 붙는다. 실패하면 목록에 아무것도 남기지 않는다.
async fn new_session(shared: &Shared, client: u64, spec: NewSession) {
    let result = create(shared, spec).await;
    let mut d = shared.lock().await;
    match result {
        Ok(()) => {
            d.publish();
            d.reply_sessions(client);
        }
        Err(e) => d.send(
            client,
            FromDaemon::Error {
                message: format!("{e:#}"),
            },
        ),
    }
}

async fn create(shared: &Shared, spec: NewSession) -> Result<()> {
    validate_name(&spec.name)?;
    let project = spec
        .project
        .canonicalize()
        .with_context(|| format!("프로젝트 폴더를 확인할 수 없다: {}", spec.project.display()))?;
    let id = session_id(&project_key(&project), &spec.name);
    let (state, exe) = {
        let mut d = shared.lock().await;
        if d.closing.contains_key(&id) {
            bail!(
                "같은 이름의 세션을 아직 닫는 중이다. 잠시 뒤 다시 만든다: {}",
                spec.name
            );
        }
        if d.sessions.contains_key(&id) || !d.starting.insert(id.clone()) {
            bail!("같은 이름의 세션이 이미 있다: {}", spec.name);
        }
        (d.state.clone(), d.exe.clone())
    };
    let restore = match spec.kind {
        Kind::Shell => Restore::None,
        Kind::Claude | Kind::Codex => Restore::Resume,
    };
    let meta = Meta {
        id: id.clone(),
        project,
        name: spec.name,
        kind: spec.kind,
        account: spec.account,
        model: spec.model,
        restore,
        created_ms: now_ms(),
        resume_id: None,
        closed: false,
    };
    let dir = state.session_dir(&id);
    // 잠금 밖에서 띄운다. 보관 프로세스가 뜨는 동안에도 다른 세션 출력과 클라이언트가 멈추지 않는다.
    let started = async {
        project::register(&state, &meta.project)?;
        ensure_dir(&dir)?;
        meta.save(&dir)?;
        start_holder(&state, &exe, &meta).await
    }
    .await;
    let mut d = shared.lock().await;
    d.starting.remove(&id);
    match started {
        Ok(Started::Linked(link)) => {
            d.sessions.insert(id.clone(), Sess::new(meta, None));
            install(&mut d, shared, &id, link, true);
            Ok(())
        }
        Ok(Started::Ended) if meta.kind != Kind::Shell => {
            d.sessions.insert(id.clone(), Sess::new(meta, None));
            d.refresh(&id);
            Ok(())
        }
        // 셸이 곧바로 끝났다. 셸 세션은 끝나면 목록에서 사라진다.
        Ok(Started::Ended) => close_meta(meta, &dir),
        Err(e) => {
            let _ = close_meta(meta, &dir);
            Err(e)
        }
    }
}

/// `Kill`: 세션을 닫고, 보관 프로세스 연결이 끝난 뒤(최대 `KILL_WAIT`) 답한다. 답을 받고 곧바로 같은 이름으로
/// 다시 만들어도 끝나 가는 보관 프로세스와 엉키지 않는다. 기다리는 동안에도 이 클라이언트의 다른 요청은 처리한다.
async fn kill(shared: &Shared, client: u64, id: &str) {
    let mut d = shared.lock().await;
    let ended = match d.kill(client, id) {
        Err(message) => return d.send(client, FromDaemon::Error { message }),
        Ok(None) => return d.reply_sessions(client),
        Ok(Some(ended)) => ended,
    };
    let shared = Arc::clone(shared);
    let id = id.to_string();
    tokio::spawn(async move {
        let done = timeout(KILL_WAIT, ended).await.is_ok();
        let mut d = shared.lock().await;
        if done {
            d.reply_sessions(client);
        } else {
            let message = format!(
                "보관 프로세스가 {}초 안에 끝나지 않았다. 아직 돌고 있을 수 있다: {id}",
                KILL_WAIT.as_secs()
            );
            d.send(client, FromDaemon::Error { message });
        }
    });
}

fn close_meta(mut meta: Meta, dir: &Path) -> Result<()> {
    meta.closed = true;
    meta.save(dir)
}

/// `Attach`: 붙지 못한 채 남긴 세션이면 먼저 다시 붙어 본다. 끝난 claude·codex 세션이고 resume id가 있으면
/// 먼저 다시 띄운다. 그다음 붙는다.
async fn attach(shared: &Shared, client: u64, id: &str, size: (u16, u16)) {
    let retry = shared.lock().await.begin_relink(id);
    if let Some(sock) = retry {
        let link = connect_holder(&sock).await;
        let mut d = shared.lock().await;
        let exists = d.sessions.get_mut(id).map(|s| s.busy = false).is_some();
        match link {
            Ok(Some(link)) if exists => install(&mut d, shared, id, link, false),
            // 다시 붙는 사이 세션이 닫혔다. 보관 프로세스도 끝낸다.
            Ok(Some(link)) => kill_link(link),
            Ok(None) => d.gone(id),
            Err(e) => {
                let message = format!("보관 프로세스에 다시 붙지 못했다: {e:#}");
                eprintln!("{id}: {message}");
                d.send(client, FromDaemon::Error { message });
            }
        }
        d.publish();
    }
    let resume = shared.lock().await.prepare_resume(id);
    if let Some((state, exe, meta)) = resume {
        let started = start_holder(&state, &exe, &meta).await;
        let mut d = shared.lock().await;
        let exists = d.sessions.get_mut(id).map(|s| s.busy = false).is_some();
        match started {
            Ok(Started::Linked(link)) if exists => install(&mut d, shared, id, link, true),
            // 다시 띄우는 사이 세션이 닫혔다. 새 보관 프로세스도 끝낸다.
            Ok(Started::Linked(link)) => kill_link(link),
            Ok(Started::Ended) => d.refresh(id),
            Err(e) => d.send(
                client,
                FromDaemon::Error {
                    message: format!("세션을 다시 띄우지 못했다: {e:#}"),
                },
            ),
        }
        d.publish();
    }
    shared.lock().await.attach(client, id, size);
}

/// 쓸 곳이 없는 보관 프로세스 연결로 `Kill`을 보내고 버린다.
fn kill_link(link: Link) {
    let mut writer = link.writer;
    tokio::spawn(async move {
        let _ = send(&mut writer, &HoldIn::Kill).await;
    });
}

/// 보관 프로세스를 새 세션 리더로 띄우고 소켓에 붙는다.
///
/// 붙기 전에 보관 프로세스가 끝났을 때, exit 파일이 있으면 자식이 곧바로 끝난 것이고(`Ended`),
/// 없으면 시작하지 못한 것이다(오류, 로그 내용을 담는다).
async fn start_holder(state: &State, exe: &Path, meta: &Meta) -> Result<Started> {
    let dir = state.session_dir(&meta.id);
    let sock = state.hold_sock(&meta.id);
    check_sock_path(&sock)?;
    ensure_dir(&state.run_dir())?;
    ensure_dir(&state.log_dir())?;
    let exit = dir.join(EXIT_FILE);
    match fs::remove_file(&exit) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e)
                .with_context(|| format!("지난 exit 파일을 지울 수 없다: {}", exit.display()));
        }
    }
    let log_path = state.log_dir().join(format!("hold-{}.log", meta.id));
    let log = File::create(&log_path)
        .with_context(|| format!("로그 파일을 열 수 없다: {}", log_path.display()))?;
    let launch = session::launch(meta, state, exe);
    let mut cmd = tokio::process::Command::new(exe);
    // portable-pty는 보관 프로세스의 환경을 자식에 그대로 넘기므로, 데몬을 띄운 셸의 환경은 여기서 지운다.
    for (key, _) in std::env::vars_os() {
        if key.to_str().is_some_and(session::is_leaked_env) {
            cmd.env_remove(&key);
        }
    }
    cmd.arg("hold")
        .arg("--sock")
        .arg(&sock)
        .arg("--exit")
        .arg(&exit)
        .arg("--cwd")
        .arg(&launch.cwd)
        .args([
            "--env",
            "TERM=xterm-256color",
            "--env",
            "COLORTERM=truecolor",
        ]);
    for (key, value) in &launch.env {
        cmd.arg("--env").arg(format!("{key}={value}"));
    }
    cmd.arg("--")
        .args(&launch.argv)
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log);
    // SAFETY: fork 뒤 exec 전에 비동기 시그널 안전 함수인 setsid만 부른다.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("보관 프로세스를 띄우지 못했다")?;
    let deadline = Instant::now() + START_WAIT;
    loop {
        if let Ok(Some(link)) = connect_holder(&sock).await {
            reap(child);
            return Ok(Started::Linked(link));
        }
        if child.try_wait()?.is_some() {
            if exit.exists() {
                return Ok(Started::Ended);
            }
            let log = fs::read_to_string(&log_path).unwrap_or_default();
            let log = log.split_whitespace().collect::<Vec<_>>().join(" ");
            bail!("세션을 띄우지 못했다: {log}");
        }
        if Instant::now() >= deadline {
            let _ = child.start_kill();
            reap(child);
            bail!(
                "보관 프로세스가 {}초 안에 뜨지 않았다",
                START_WAIT.as_secs()
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// 띄운 보관 프로세스가 끝나면 거둔다. 데몬이 먼저 끝나면 init이 거둔다.
fn reap(mut child: tokio::process::Child) {
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
}

/// 보관 소켓에 붙어 `Hello`(버전 확인)와 출력 버퍼를 읽는다.
///
/// 소켓 파일이 없거나 연결을 거절하면 보관 프로세스가 없는 것이라 `Ok(None)`이다. 그 밖의 실패(시간 초과,
/// 버전 불일치 등)는 오류다. 그때는 보관 프로세스가 살아 있을 수 있으므로 세션을 끝난 것으로 보면 안 된다.
async fn connect_holder(sock: &Path) -> Result<Option<Link>> {
    check_sock_path(sock)?;
    let stream = match UnixStream::connect(sock).await {
        Ok(stream) => stream,
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(None);
        }
        Err(e) => {
            return Err(e).with_context(|| format!("보관 소켓에 붙지 못했다: {}", sock.display()));
        }
    };
    let (read, writer) = stream.into_split();
    let mut reader = BufReader::with_capacity(HOLD_READ_BUF, read);
    match timeout(LINK_WAIT, recv::<_, HoldOut>(&mut reader)).await?? {
        Some(HoldOut::Hello { version, .. }) if version == HOLD_VERSION => {}
        Some(HoldOut::Hello { version, .. }) => {
            bail!("보관 프로세스 버전이 다르다(보관 {version}, 데몬 {HOLD_VERSION})")
        }
        other => bail!("보관 프로세스가 Hello를 보내지 않았다: {other:?}"),
    }
    let ring = match timeout(LINK_WAIT, recv::<_, HoldOut>(&mut reader)).await?? {
        Some(HoldOut::Output { data }) => data,
        other => bail!("보관 프로세스가 출력 버퍼를 보내지 않았다: {other:?}"),
    };
    Ok(Some(Link {
        reader,
        writer,
        ring,
    }))
}

/// 세션에 보관 프로세스 연결을 붙이고 출력 버퍼로 화면을 다시 만든다.
///
/// `fresh`는 방금 띄운 보관 프로세스다. PTY가 80x24로 시작하고, 버퍼 속 질의는 아직 아무도 답하지 않았으므로
/// 답을 보낸다. 다시 붙는 경우(데몬 재시작 등)에는 질의에 이미 답했으므로 답을 버리고, PTY 크기를 화면에 맞춘다.
fn install(d: &mut Daemon, shared: &Shared, id: &str, link: Link, fresh: bool) {
    let link_id = d.next_id();
    let colors = d.colors;
    let Some(s) = d.sessions.get_mut(id) else {
        return;
    };
    let Link {
        reader,
        writer,
        ring,
    } = link;
    let tx = spawn_hold_writer(writer);
    if fresh {
        s.size = INITIAL_SIZE;
    } else {
        let (cols, rows) = s.size;
        let _ = tx.send(HoldIn::Resize { cols, rows });
    }
    s.screen = Screen::new(s.size.0, s.size.1);
    if let Some((fg, bg)) = colors {
        s.screen.set_default_colors(fg, bg);
    }
    s.screen.feed(&ring);
    s.unlinked = false;
    s.hold = Some(Hold {
        link: link_id,
        tx,
        since: Instant::now(),
    });
    if fresh {
        s.answer_queries();
    } else {
        s.screen.take_replies();
    }
    if let Some(deadline) = s.start_sync_timer() {
        spawn_sync_timer(Arc::clone(shared), id.to_string(), deadline);
    }
    let data = s.screen.snapshot();
    spawn_hold_reader(Arc::clone(shared), id.to_string(), link_id, reader);
    d.refresh(id);
    // 화면을 새로 만들었으니 보던 클라이언트에는 스냅숏을 다시 보낸다.
    d.send_viewers(
        id,
        FromDaemon::Snapshot {
            id: id.to_string(),
            data,
        },
    );
}

fn spawn_hold_writer(mut writer: OwnedWriteHalf) -> mpsc::UnboundedSender<HoldIn> {
    let (tx, mut rx) = mpsc::unbounded_channel::<HoldIn>();
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if send(&mut writer, &msg).await.is_err() {
                break;
            }
        }
    });
    tx
}

/// 보관 프로세스 연결을 차례로 읽는 작업. 출력을 전역 잠금 안에서 화면에 먹인다.
///
/// macOS PTY는 읽는 쪽이 따라잡으면 수십 바이트씩 끊어 주므로, 이미 읽기 버퍼에 와 있는 출력 줄은
/// 기다리지 않고 모아서(`BATCH_MAX`까지) 한 번에 처리한다. 잠금 횟수와 클라이언트 큐 항목 수가 줄어든다.
fn spawn_hold_reader(shared: Shared, id: String, link: u64, mut reader: BufReader<OwnedReadHalf>) {
    tokio::spawn(async move {
        'read: loop {
            let mut msg = recv::<_, HoldOut>(&mut reader).await;
            let mut after = None;
            if let Ok(Some(HoldOut::Output { data })) = &mut msg {
                // 버퍼에 줄 끝이 있으면 다음 메시지는 소켓을 기다리지 않고 읽힌다.
                while data.len() < BATCH_MAX && reader.buffer().contains(&b'\n') {
                    match recv::<_, HoldOut>(&mut reader).await {
                        Ok(Some(HoldOut::Output { data: more })) => data.extend(more),
                        other => {
                            after = Some(other);
                            break;
                        }
                    }
                }
            }
            for msg in std::iter::once(msg).chain(after) {
                match msg {
                    Ok(Some(HoldOut::Output { data })) => {
                        let mut d = shared.lock().await;
                        if let Some(deadline) = d.on_output(&id, link, data) {
                            spawn_sync_timer(Arc::clone(&shared), id.clone(), deadline);
                        }
                        if d.mark_active(&id, link) {
                            spawn_quiet_timer(Arc::clone(&shared), id.clone());
                        }
                    }
                    Ok(Some(HoldOut::Hello { .. })) => {}
                    Ok(Some(HoldOut::Exited { .. })) => {
                        shared.lock().await.on_exit(&id, link);
                        break 'read;
                    }
                    Ok(None) | Err(_) => {
                        lost_holder(Arc::clone(&shared), id.clone(), link).await;
                        break 'read;
                    }
                }
            }
        }
        shared.lock().await.end_closing(&id, link);
    });
}

/// `Exited` 없이 연결이 끊겼다. 보관 프로세스가 살아 있으면(연결만 끊긴 경우) 다시 붙는다. 소켓이 없거나
/// 연결을 거절하면 끝난 세션이고, 그 밖의 실패면 세션을 남겨 두고 다음 `Attach`에 맡긴다.
async fn lost_holder(shared: Shared, id: String, link: u64) {
    let (sock, exit, old_enough) = {
        let d = shared.lock().await;
        match d.sessions.get(&id).and_then(|s| s.hold.as_ref()) {
            Some(h) if h.link == link => (
                d.state.hold_sock(&id),
                d.state.session_dir(&id).join(EXIT_FILE),
                h.since.elapsed() >= RELINK_MIN_AGE,
            ),
            _ => return,
        }
    };
    // exit 파일이 있으면 자식이 끝났다. 없으면 보관 프로세스가 아직 있는지 소켓으로 확인한다.
    let relink = if exit.exists() {
        Ok(None)
    } else {
        connect_holder(&sock).await
    };
    let mut d = shared.lock().await;
    if !d.is_current(&id, link) {
        return;
    }
    match relink {
        Ok(Some(new)) if old_enough => {
            eprintln!("보관 프로세스에 다시 붙었다: {id}");
            install(&mut d, &shared, &id, new, false);
            d.publish();
        }
        // 붙자마자 또 끊겼다. 되풀이하지 않고 다음 Attach에 맡긴다.
        Ok(Some(_)) => d.unlink(&id, "연결이 곧바로 다시 끊겼다"),
        Ok(None) => d.gone(&id),
        Err(e) => d.unlink(&id, &format!("{e:#}")),
    }
}

/// 동기화 갱신이 시간 제한을 넘기면 끝낸다. 갱신이 다시 시작돼 제한이 늦춰졌으면 그때까지 더 기다린다.
fn spawn_sync_timer(shared: Shared, id: String, mut deadline: Instant) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep_until(deadline.into()).await;
            let mut d = shared.lock().await;
            let Some(s) = d.sessions.get_mut(&id) else {
                return;
            };
            match s.screen.sync_deadline() {
                Some(next) if next > Instant::now() => deadline = next,
                Some(_) => {
                    s.screen.end_sync();
                    s.answer_queries();
                    s.sync_timer = false;
                    return;
                }
                None => {
                    s.sync_timer = false;
                    return;
                }
            }
        }
    });
}

/// shell 세션이 `SHELL_QUIET` 동안 출력이 없으면 대기로 돌리고 알린다. 그사이 출력이 있었으면 그만큼 더 기다린다.
fn spawn_quiet_timer(shared: Shared, id: String) {
    tokio::spawn(async move {
        let mut deadline = Instant::now() + SHELL_QUIET;
        loop {
            tokio::time::sleep_until(deadline.into()).await;
            let mut d = shared.lock().await;
            let Some(s) = d.sessions.get_mut(&id) else {
                return;
            };
            match s.active {
                Some(at) if at + SHELL_QUIET > Instant::now() => deadline = at + SHELL_QUIET,
                Some(_) => {
                    s.active = None;
                    d.refresh(&id);
                    d.publish();
                    return;
                }
                None => return,
            }
        }
    });
}

impl Sess {
    fn new(meta: Meta, viewed_ms: Option<u64>) -> Sess {
        Sess {
            meta,
            screen: Screen::new(INITIAL_SIZE.0, INITIAL_SIZE.1),
            size: INITIAL_SIZE,
            hold: None,
            state: SessionState::Idle,
            waiting: None,
            resume_id: None,
            viewed_ms,
            sync_timer: false,
            active: None,
            unlinked: false,
            busy: false,
        }
    }

    fn info(&self) -> SessionInfo {
        SessionInfo {
            meta: self.meta.clone(),
            state: self.state.clone(),
            waiting: self.waiting.clone(),
            alive: self.hold.is_some() || self.unlinked,
        }
    }

    fn send_hold(&self, msg: HoldIn) {
        if let Some(hold) = &self.hold {
            let _ = hold.tx.send(msg);
        }
    }

    /// 화면이 모은 터미널 질의 답을 보관 프로세스에 입력으로 보낸다.
    fn answer_queries(&mut self) {
        let data = self.screen.take_replies();
        if !data.is_empty() {
            self.send_hold(HoldIn::Input { data });
        }
    }

    /// 동기화 갱신 중이고 지켜보는 작업이 없으면 그 데드라인을 돌려주고 작업이 있다고 표시한다.
    fn start_sync_timer(&mut self) -> Option<Instant> {
        if self.sync_timer {
            return None;
        }
        let deadline = self.screen.sync_deadline()?;
        self.sync_timer = true;
        Some(deadline)
    }

    /// 화면과 PTY 크기를 바꾼다. 같은 크기면 아무것도 하지 않는다.
    fn resize(&mut self, size: (u16, u16)) {
        if self.size == size {
            return;
        }
        self.size = size;
        self.screen.resize(size.0, size.1);
        self.send_hold(HoldIn::Resize {
            cols: size.0,
            rows: size.1,
        });
    }
}

impl Daemon {
    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn add_client(&mut self, tx: mpsc::Sender<FromDaemon>, writer: AbortHandle) -> u64 {
        let id = self.next_id();
        let _ = tx.try_send(FromDaemon::Hello { version: VERSION });
        self.clients.insert(
            id,
            Client {
                tx,
                writer,
                subscribed: false,
                attached: None,
                size: None,
            },
        );
        id
    }

    /// 클라이언트 큐에 넣는다. 큐가 꽉 찼거나 닫혔으면 그 클라이언트를 끊는다.
    fn send(&mut self, client: u64, msg: FromDaemon) {
        let Some(c) = self.clients.get(&client) else {
            return;
        };
        if c.tx.try_send(msg).is_err() {
            self.disconnect(client);
        }
    }

    /// 쓰기 작업을 멈춰 연결을 바로 끊는다. 다시 붙으면 `Attach`로 스냅숏을 받는다.
    fn disconnect(&mut self, client: u64) {
        if let Some(c) = self.clients.remove(&client) {
            c.writer.abort();
        }
    }

    /// 세션을 보고 있는 클라이언트 모두에 보낸다.
    fn send_viewers(&mut self, id: &str, msg: FromDaemon) {
        let full: Vec<u64> = self
            .clients
            .iter()
            .filter(|(_, c)| c.attached.as_deref() == Some(id))
            .filter(|(_, c)| c.tx.try_send(msg.clone()).is_err())
            .map(|(&k, _)| k)
            .collect();
        for k in full {
            self.disconnect(k);
        }
    }

    fn viewed_by_someone(&self, id: &str) -> bool {
        self.clients
            .values()
            .any(|c| c.attached.as_deref() == Some(id))
    }

    /// 프로젝트 경로, 이름 순 목록.
    fn infos(&self) -> Vec<SessionInfo> {
        let mut infos: Vec<SessionInfo> = self.sessions.values().map(Sess::info).collect();
        infos.sort_by(|a, b| (&a.meta.project, &a.meta.name).cmp(&(&b.meta.project, &b.meta.name)));
        infos
    }

    /// 목록이 바뀌었으면 구독한 클라이언트 모두에 알린다.
    fn publish(&mut self) {
        let infos = self.infos();
        if infos == self.last {
            return;
        }
        let subscribed: Vec<u64> = self
            .clients
            .iter()
            .filter(|(_, c)| c.subscribed)
            .map(|(&k, _)| k)
            .collect();
        for k in subscribed {
            self.send(
                k,
                FromDaemon::Sessions {
                    sessions: infos.clone(),
                },
            );
        }
        self.last = infos;
    }

    fn reply_sessions(&mut self, client: u64) {
        let sessions = self.infos();
        self.send(client, FromDaemon::Sessions { sessions });
    }

    /// 이벤트 기록을 다시 읽어 상태를 계산한다. 누가 보고 있으면 지금 본 것으로 기록한다.
    /// 같은 id로 다시 만든 세션이 지난 세션의 이벤트를 물려받지 않게 만든 시각 이후 이벤트만 본다.
    fn refresh(&mut self, id: &str) {
        let viewed = self.viewed_by_someone(id);
        let dir = self.state.session_dir(id);
        let Some(s) = self.sessions.get_mut(id) else {
            return;
        };
        if viewed {
            let now = now_ms();
            s.viewed_ms = Some(now);
            if let Err(e) = fs::write(dir.join(VIEWED_FILE), now.to_string()) {
                eprintln!("본 시각을 저장하지 못했다: {id} ({e})");
            }
        }
        let events: Vec<Event> = hooks::read_all(&dir)
            .unwrap_or_else(|e| {
                eprintln!("{e:#}");
                Vec::new()
            })
            .into_iter()
            .filter(|ev| ev.ms >= s.meta.created_ms)
            .collect();
        let (mut state, waiting, resume_id) = hooks::fold(&events, s.viewed_ms);
        // shell 세션은 훅 대신 출력으로 작업 중을 정한다(`active`는 shell 세션에만 생긴다). 훅 기록이 대기 말고 다른
        // 상태를 정했으면 그쪽을 따른다.
        if s.active.is_some() && state == SessionState::Idle {
            state = SessionState::Working;
        }
        s.resume_id = resume_id.or_else(|| s.meta.resume_id.clone());
        (s.state, s.waiting) = if s.hold.is_some() || s.unlinked {
            (state, waiting)
        } else {
            (SessionState::Ended, None)
        };
    }

    /// `List`: 모든 세션을 다시 계산해 답하고, 이 클라이언트를 목록 변경 구독자로 삼는다.
    fn list(&mut self, client: u64) {
        let ids: Vec<String> = self.sessions.keys().cloned().collect();
        for id in &ids {
            self.refresh(id);
        }
        self.publish();
        if let Some(c) = self.clients.get_mut(&client) {
            c.subscribed = true;
        }
        self.reply_sessions(client);
    }

    /// 끝난 세션을 다시 띄울 준비를 한다. 다시 띄울 세션이면 띄우는 데 필요한 값을 돌려준다.
    fn prepare_resume(&mut self, id: &str) -> Option<(State, PathBuf, Meta)> {
        let dir = self.state.session_dir(id);
        let s = self.sessions.get_mut(id)?;
        // 붙지 못한 세션은 프로세스가 살아 있을 수 있으므로 같은 대화로 하나 더 띄우지 않는다.
        if s.hold.is_some() || s.unlinked || s.busy || s.meta.kind == Kind::Shell {
            return None;
        }
        s.meta.resume_id = Some(s.resume_id.clone()?);
        s.busy = true;
        if let Err(e) = s.meta.save(&dir) {
            eprintln!("{e:#}");
        }
        Some((self.state.clone(), self.exe.clone(), s.meta.clone()))
    }

    /// 붙지 못한 채 남긴 세션이고 다른 시도가 없으면 다시 붙을 소켓 경로를 돌려준다.
    fn begin_relink(&mut self, id: &str) -> Option<PathBuf> {
        let s = self.sessions.get_mut(id)?;
        if !s.unlinked || s.busy {
            return None;
        }
        s.busy = true;
        Some(self.state.hold_sock(id))
    }

    /// 클라이언트를 세션 구독자로 정하고, 크기를 맞춘 뒤 스냅숏을 보낸다.
    fn attach(&mut self, client: u64, id: &str, size: (u16, u16)) {
        if !self.clients.contains_key(&client) {
            return;
        }
        let Some(s) = self.sessions.get_mut(id) else {
            let message = format!("세션이 없다: {id}");
            return self.send(client, FromDaemon::Error { message });
        };
        s.resize(size);
        let data = s.screen.snapshot();
        if let Some(c) = self.clients.get_mut(&client) {
            c.attached = Some(id.to_string());
            c.size = Some(size);
        }
        self.send(
            client,
            FromDaemon::Snapshot {
                id: id.to_string(),
                data,
            },
        );
        self.refresh(id);
        self.publish();
    }

    /// `Input`: 보관 프로세스에 보낸다. 승인 대기 중이었으면 `Input` 이벤트를 기록한다.
    fn input(&mut self, client: u64, id: &str, data: Vec<u8>) {
        let Some(c) = self.clients.get(&client) else {
            return;
        };
        let size = c.size.filter(|_| c.attached.as_deref() == Some(id));
        let dir = self.state.session_dir(id);
        let Some(s) = self.sessions.get_mut(id) else {
            return;
        };
        if s.hold.is_none() {
            return;
        }
        if let Some(size) = size {
            s.resize(size);
        }
        s.send_hold(HoldIn::Input { data });
        if s.state == SessionState::Approval {
            let ev = Event {
                ms: now_ms(),
                kind: EventKind::Input,
                text: None,
                resume_id: None,
            };
            if let Err(e) = hooks::append(&dir, &ev) {
                eprintln!("{e:#}");
            }
            self.refresh(id);
            self.publish();
        }
    }

    /// `Colors`: 바깥 터미널 색을 지금 있는 모든 화면과 앞으로 붙일 화면(`install`)의 기본색으로 삼는다.
    /// 보관 프로세스가 붙지 않은 화면은 질의할 앱이 없고, 붙을 때 `install`이 화면을 새로 만든다.
    fn set_colors(&mut self, fg: &str, bg: &str) {
        let (Ok(fg), Ok(bg)) = (fg.parse::<Rgb>(), bg.parse::<Rgb>()) else {
            eprintln!("색 형식이 올바르지 않다: {fg} {bg}");
            return;
        };
        self.colors = Some((fg, bg));
        for s in self.sessions.values_mut() {
            s.screen.set_default_colors(fg, bg);
        }
    }

    /// `Resize`: 이 클라이언트의 크기를 기억하고 세션 크기를 맞춘다.
    fn resize(&mut self, client: u64, id: &str, size: (u16, u16)) {
        let Some(c) = self.clients.get_mut(&client) else {
            return;
        };
        c.size = Some(size);
        if let Some(s) = self.sessions.get_mut(id) {
            s.resize(size);
        }
    }

    /// `Kill`: 보관 프로세스에 `Kill`을 보내고 `closed=true`로 저장한다. 폴더와 기록은 남긴다.
    ///
    /// 보관 프로세스 연결이 있었으면 그 연결이 끝날 때까지 세션을 `closing`에 두고, 끝나면 닫히는 수신자를
    /// 돌려준다. 세션이 없으면 클라이언트에 보낼 오류 문구를 돌려준다.
    fn kill(&mut self, client: u64, id: &str) -> Result<Option<oneshot::Receiver<()>>, String> {
        let Some(s) = self.sessions.remove(id) else {
            return Err(format!("세션이 없다: {id}"));
        };
        s.send_hold(HoldIn::Kill);
        if s.unlinked {
            eprintln!("붙지 못한 세션을 닫는다. 보관 프로세스가 남아 있을 수 있다: {id}");
        }
        self.detach_all(id);
        if let Err(e) = close_meta(s.meta, &self.state.session_dir(id)) {
            let message = format!("{e:#}");
            self.send(client, FromDaemon::Error { message });
        }
        self.publish();
        let Some(hold) = s.hold else {
            return Ok(None);
        };
        let (done, ended) = oneshot::channel();
        self.closing
            .insert(id.to_string(), Closing { hold, _done: done });
        Ok(Some(ended))
    }

    /// 보관 프로세스 연결 `link`가 끝났다. `Kill`로 닫는 중이던 연결이면 `closing`에서 빼고 기다리는 쪽을 깨운다.
    fn end_closing(&mut self, id: &str, link: u64) {
        if self.closing.get(id).is_some_and(|c| c.hold.link == link) {
            self.closing.remove(id);
        }
    }

    fn detach_all(&mut self, id: &str) {
        for c in self.clients.values_mut() {
            if c.attached.as_deref() == Some(id) {
                c.attached = None;
            }
        }
    }

    /// 보관 프로세스 출력. 화면에 먹이고, 질의에 답하고, 보는 클라이언트에 보낸다.
    /// 동기화 갱신 시간 제한 작업을 새로 띄워야 하면 그 데드라인을 돌려준다.
    fn on_output(&mut self, id: &str, link: u64, data: Vec<u8>) -> Option<Instant> {
        let s = self.sessions.get_mut(id)?;
        if s.hold.as_ref()?.link != link {
            return None;
        }
        s.screen.feed(&data);
        s.answer_queries();
        let deadline = s.start_sync_timer();
        self.send_viewers(
            id,
            FromDaemon::Output {
                id: id.to_string(),
                data,
            },
        );
        deadline
    }

    /// shell 세션의 보관 프로세스가 실시간 출력을 보냈다. 대기에서 작업 중으로 바뀌면 알린다. 조용해지기를
    /// 지켜보는 작업을 새로 띄워야 하면 `true`다.
    ///
    /// 다시 붙을 때 받는 출력 버퍼는 `install`이 화면에 먹이고, 터미널 질의 답은 보관 프로세스로 가는 입력이라
    /// 둘 다 여기에 오지 않는다. 출력 조각마다 목록을 다시 계산하지 않고, 바뀔 때만 계산해 알린다.
    fn mark_active(&mut self, id: &str, link: u64) -> bool {
        let Some(s) = self.sessions.get_mut(id) else {
            return false;
        };
        if s.meta.kind != Kind::Shell || s.hold.as_ref().is_none_or(|h| h.link != link) {
            return false;
        }
        if s.active.replace(Instant::now()).is_some() {
            return false;
        }
        self.refresh(id);
        self.publish();
        true
    }

    /// 세션에 붙은 보관 프로세스 연결이 `link`인가.
    fn is_current(&self, id: &str, link: u64) -> bool {
        self.sessions
            .get(id)
            .and_then(|s| s.hold.as_ref())
            .is_some_and(|h| h.link == link)
    }

    /// 연결 `link`의 보관 프로세스가 `Exited`를 알렸다.
    fn on_exit(&mut self, id: &str, link: u64) {
        if self.is_current(id, link) {
            self.gone(id);
        }
    }

    /// 보관 프로세스에 붙지 못했지만 살아 있을 수 있다. 세션을 닫거나 끝남으로 돌리지 않고 남긴다.
    fn unlink(&mut self, id: &str, why: &str) {
        eprintln!("보관 프로세스에 붙지 못했다. 세션은 남긴다: {id} ({why})");
        if let Some(s) = self.sessions.get_mut(id) {
            s.hold = None;
            s.unlinked = true;
        }
        self.refresh(id);
        self.publish();
    }

    /// 보관 프로세스가 끝났다. 셸 세션은 목록에서 빼고 닫고, claude·codex 세션은 끝남으로 남긴다.
    fn gone(&mut self, id: &str) {
        let Some(s) = self.sessions.get_mut(id) else {
            return;
        };
        s.hold = None;
        s.unlinked = false;
        if s.meta.kind == Kind::Shell {
            if let Some(s) = self.sessions.remove(id) {
                let _ = close_meta(s.meta, &self.state.session_dir(id))
                    .inspect_err(|e| eprintln!("{e:#}"));
            }
            self.detach_all(id);
        } else {
            self.refresh(id);
        }
        self.publish();
    }
}
