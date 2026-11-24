//! The daemon (`mine daemon`). It holds the session list and screens, and connects holder processes and clients.
//!
//! - For each holder process (`mine hold`) there is one connection with one read task and one write task. Output is
//!   fed to the session's `Screen` and sent to the clients viewing that session. The daemon sends the answers to terminal
//!   queries from the inner app back to the holder process (it answers even if no client is attached).
//! - Each client has one read task and one write task. Messages to send queue up to `QUEUE_CAP`, and the client is dropped if it overflows.
//! - A client that sent `List` receives `Sessions` whenever the list changes after that. `List`, `New` and `Kill` are
//!   answered directly to the requesting client with `Sessions` (or `Error`). This keeps a non-subscribed headless connection from
//!   mistaking a notification about another event for the answer to its own request.
//! - When a client reports the outer terminal's default colors with `Colors`, every session screen (including screens attached later)
//!   answers the inner app's color queries with those colors.
//! - A client reports with `Focus` whether its window is the one the user is looking at. When a session newly needs approval and
//!   no client is focused, the daemon shows a macOS notification (`MINE_NOTIFY`, default `osascript`).
//! - Any client from `MIN_CLIENT_VERSION` up is served, and gets only what its version knows: the profile list goes to v4+ clients
//!   only, right after `Hello` and again to all of them after `ReloadProfiles`. A message type the daemon doesn't know is ignored.
//! - When the daemon starts again, it scans `sessions/*/meta.json`, reattaches to the holder processes that are alive, and rebuilds
//!   the 80x24 screen from the holder process's output buffer. The first `Attach` changes it to the client's size.
//!
//! `proto::recv` is not cancellation-safe, so it isn't put in `select!`. One read task per connection reads in turn.

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
use mine_core::conn::restart_hint;
use mine_core::hooks::{self, Event, EventKind, now_ms};
use mine_core::paths::{State, check_sock_path, ensure_dir};
use mine_core::profiles::{self, ProfileInfo, Profiles};
use mine_core::project::{self, project_key};
use mine_core::proto::{
    DaemonInfo, FromDaemon, HOLD_VERSION, HoldIn, HoldOut, MIN_CLIENT_VERSION, ToDaemon, VERSION,
    recv, send,
};
use mine_core::screen::Screen;
use mine_core::session::{
    self, Kind, Meta, NewSession, Restore, SessionInfo, SessionState, session_id, validate_name,
};
use mine_core::share;

/// The number of messages to send that can be queued for one client. If it overflows, that client is dropped.
const QUEUE_CAP: usize = 1024;
/// File name of the holder process's exit code file (inside the session directory).
const EXIT_FILE: &str = "exit";
/// File name of the time the user last saw the session (Unix milliseconds) (inside the session directory).
const VIEWED_FILE: &str = "viewed";
/// The PTY size of a newly started holder process, and the size used to rebuild a screen when the daemon starts again.
const INITIAL_SIZE: (u16, u16) = (80, 24);
/// Maximum time to wait for the holder connection to end after `Kill`. The holder sends SIGHUP, and after `KILL_GRACE`
/// sends SIGKILL, then reads the remaining output up to `DRAIN_GRACE` and exits. This adds some headroom on top of that.
const KILL_WAIT: Duration = hold::KILL_GRACE
    .saturating_add(hold::DRAIN_GRACE)
    .saturating_add(Duration::from_millis(1500));
/// If a shell session produces no output for this long, it goes back from Working to Idle (design 5.2).
const SHELL_QUIET: Duration = Duration::from_secs(2);
/// Maximum time to wait for a started holder process to accept connections.
const START_WAIT: Duration = Duration::from_secs(5);
/// Maximum time to wait for `Hello` and the output buffer on a holder process connection.
const LINK_WAIT: Duration = Duration::from_secs(5);
/// If a holder process connection that had been up for less than this drops, the process is considered to have ended rather than reattached (to avoid attaching and dropping over and over).
const RELINK_MIN_AGE: Duration = Duration::from_secs(1);
/// After a refused `Hello`, wait this long for a hook's `Poke` (`mine _event` writes both at once).
const POKE_WAIT: Duration = Duration::from_secs(1);
/// If another daemon is just finishing, wait this long for the pid file lock to be released.
const PID_LOCK_WAIT: Duration = Duration::from_secs(1);
/// Read buffer size of a holder process connection. Sets how much backed-up output can be gathered at once.
const HOLD_READ_BUF: usize = 64 * 1024;
/// Maximum bytes of output gathered and fed to the screen at once. The lock-count savings come from merging
/// chunks of a few dozen bytes, so this is kept small.
// ponytail: One output message is under BATCH_MAX plus one holder read (up to 16 KiB), so under 32 KiB, and
// the client queue has 1024 entries, so the output one lagging client piles up in the daemon is at most 32 MiB (snapshots are separate).
// To cut it further, count the queue in bytes.
const BATCH_MAX: usize = 16 * 1024;
/// The features this daemon announces in its hello reply. Clients gate new behavior on these names, not on the version alone.
const FEATURES: &[&str] = &["profiles"];
/// The first client version that knows `FromDaemon::Profiles`.
const PROFILES_SINCE: u32 = 4;

// ponytail: global lock; per-session locks once there are hundreds of sessions
type Shared = Arc<Mutex<Daemon>>;

/// State of the whole daemon.
struct Daemon {
    state: State,
    exe: PathBuf,
    /// Read at startup and on `ReloadProfiles`.
    profiles: Profiles,
    /// What clients show for `profiles`, computed with them.
    // ponytail: login state is read at load, not per Hello (every hook poke says Hello, and Claude's `.claude.json` can be large);
    // a login made outside `mine` shows after the next reload.
    profile_infos: Vec<ProfileInfo>,
    /// The user's home (`HOME`), where the CLIs' default homes are.
    user_home: PathBuf,
    /// When the daemon started (Unix milliseconds), for the hello reply.
    started_ms: u64,
    sessions: BTreeMap<String, Sess>,
    /// Session ids for which `New` is starting a holder process. Keeps the same name from being created twice at once.
    starting: HashSet<String>,
    /// Sessions closed with `Kill` whose holder process connection hasn't ended yet. Until it ends, `New` with the same id is not accepted.
    closing: HashMap<String, Closing>,
    clients: HashMap<u64, Client>,
    next_id: u64,
    /// The list last announced to subscribed clients. Announced again only when it changes.
    last: Vec<SessionInfo>,
    /// The outer terminal's default foreground and background colors reported by a client. Used for newly created screens too.
    colors: Option<(Rgb, Rgb)>,
}

/// One session.
struct Sess {
    meta: Meta,
    screen: Screen,
    /// The current size of the screen and PTY.
    size: (u16, u16),
    /// The holder process connection. `None` if the process has ended or the session couldn't be attached.
    hold: Option<Hold>,
    /// Couldn't attach to the holder process (timeout, version mismatch, etc.) but it may be alive. It isn't treated as ended
    /// and isn't restarted either. The next `Attach` tries to reattach.
    unlinked: bool,
    state: SessionState,
    waiting: Option<String>,
    resume_id: Option<String>,
    viewed_ms: Option<u64>,
    /// A task is watching the synchronized-update time limit.
    sync_timer: bool,
    /// When a shell session last produced output. If `SHELL_QUIET` passes without output it goes back to `None`. Until then it's Working.
    active: Option<Instant>,
    /// The holder process is being restarted or reattached.
    busy: bool,
    /// The hook log said Needs approval at the last refresh. Starts `true` so the first refresh, which rereads old events after a daemon
    /// restart, announces nothing.
    asking: bool,
}

/// A holder process connection. `link` is numbered afresh for each connection, filtering out events sent by a previous connection's tasks.
struct Hold {
    link: u64,
    tx: mpsc::UnboundedSender<HoldIn>,
    since: Instant,
}

/// The holder process connection of a session being closed.
///
/// Holding the connection lets the holder see input EOF without cutting the connection first, and it sends `Exited` after the child ends.
/// Dropping it closes `_done`, and whoever is waiting for `Kill` wakes up.
struct Closing {
    hold: Hold,
    _done: oneshot::Sender<()>,
}

struct Client {
    tx: mpsc::Sender<FromDaemon>,
    writer: AbortHandle,
    /// Sends `List` to receive list changes.
    subscribed: bool,
    attached: Option<String>,
    /// The size this client last reported (design 5.1: the session size follows the client that typed last).
    size: Option<(u16, u16)>,
    /// Reported with `Focus`: this client's window is the one the user is looking at. It goes away with the client.
    focused: bool,
    /// The protocol version from the client's `Hello` (0 until then). Decides which messages it gets.
    version: u32,
    /// An unknown message type from this client was logged already.
    noted_unknown: bool,
}

/// A just-attached holder process connection. It has read through `Hello` and the output buffer.
struct Link {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    ring: Vec<u8>,
}

/// What starting a holder process needs, copied out of the daemon so the start runs outside the lock.
struct Start {
    state: State,
    exe: PathBuf,
    profiles: Profiles,
    user_home: PathBuf,
}

/// The result of starting a holder process.
enum Started {
    Linked(Link),
    /// The child had already ended before attaching (there is an exit file).
    Ended,
}

/// Runs the daemon. At startup it locks `daemon.pid` and writes the pid. Doesn't return (returns if an error occurs).
pub async fn run(state: State) -> Result<()> {
    for dir in [
        state.root.clone(),
        state.sessions_dir(),
        state.run_dir(),
        state.log_dir(),
    ] {
        ensure_dir(&dir)
            .with_context(|| format!("Couldn't create the directory: {}", dir.display()))?;
    }
    let _pid = lock_pid_file(&state)?;
    let sock = state.daemon_sock();
    check_sock_path(&sock)?;
    // The pid lock is held, so a leftover socket file belongs to a dead daemon.
    match fs::remove_file(&sock) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e).with_context(|| {
                format!("Couldn't remove the leftover socket: {}", sock.display())
            });
        }
    }
    let listener = UnixListener::bind(&sock)
        .with_context(|| format!("Couldn't create the socket: {}", sock.display()))?;
    fs::set_permissions(&sock, fs::Permissions::from_mode(0o600))?;
    let exe = std::env::current_exe().context("Couldn't determine the executable path")?;
    let user_home = std::env::home_dir().context("Couldn't determine the home directory")?;
    let (profiles, profile_infos) = load_profiles(&state, &user_home);
    let shared: Shared = Arc::new(Mutex::new(Daemon {
        state,
        exe,
        profiles,
        profile_infos,
        user_home,
        started_ms: now_ms(),
        sessions: BTreeMap::new(),
        starting: HashSet::new(),
        closing: HashMap::new(),
        clients: HashMap::new(),
        next_id: 0,
        last: Vec::new(),
        colors: None,
    }));
    restore(&shared).await;
    eprintln!("Daemon started: pid {}", std::process::id());
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(serve_client(Arc::clone(&shared), stream));
            }
            Err(e) => {
                // Avoid a busy loop on transient errors such as EMFILE.
                eprintln!("Couldn't accept a connection: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// Locks `daemon.pid` and writes the pid. The lock is held as long as the returned file is alive (until the daemon ends).
fn lock_pid_file(state: &State) -> Result<File> {
    let path = state.daemon_pid();
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("Couldn't open the pid file: {}", path.display()))?;
    let deadline = Instant::now() + PID_LOCK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(TryLockError::WouldBlock) => {
                bail!("Another daemon is already running ({})", path.display())
            }
            Err(TryLockError::Error(e)) => {
                return Err(e)
                    .with_context(|| format!("Couldn't lock the pid file: {}", path.display()));
            }
        }
    }
    file.set_len(0)?;
    write!(file, "{}", std::process::id())?;
    file.flush()?;
    Ok(file)
}

/// Revives sessions that haven't been closed. If it can attach to the holder process, the session is alive; if the socket is missing or refuses the connection,
/// the session has ended (an ended shell session is removed from the list and saved with `closed=true`). Any other failure may mean the holder process is alive,
/// so the session is left as it is and the error is logged.
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
                eprintln!("Closing the finished shell session: {id}");
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

/// Reads the profiles and what clients show for them. Warnings go to `daemon.log`; they give a line and a reason, never a value.
fn load_profiles(state: &State, user_home: &Path) -> (Profiles, Vec<ProfileInfo>) {
    let (profiles, warnings) = Profiles::load(state);
    for warning in warnings {
        eprintln!("Warning: {warning}");
    }
    let infos = profiles::infos(&profiles, state, Some(user_home));
    (profiles, infos)
}

fn read_viewed(dir: &Path) -> Option<u64> {
    fs::read_to_string(dir.join(VIEWED_FILE))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Handles one client connection. The first message must be a `Hello` of `MIN_CLIENT_VERSION` or newer, except for a
/// hook's `Poke` (see below).
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
    let (hello, served) = match recv::<_, ToDaemon>(&mut reader).await {
        Ok(Some(ToDaemon::Hello { version })) => {
            (true, shared.lock().await.greet(client, Some(version)))
        }
        Ok(Some(_)) => (false, shared.lock().await.greet(client, None)),
        Ok(None) | Err(_) => (false, false),
    };
    if served {
        while let Ok(Some(msg)) = recv::<_, ToDaemon>(&mut reader).await {
            if !handle(&shared, client, msg).await {
                break;
            }
        }
    }
    // Dropping the sender makes the write task send the remaining messages (errors included) and close the connection.
    shared.lock().await.clients.remove(&client);
    // A session's hook runs the `mine` copy the session was started from, which can be another version than this
    // daemon (a bundle copy and a cargo copy are updated separately). A poke only says "re-read this session's
    // events", and the events file is the real contract, so it is honoured across versions, even one too old to be
    // served. Nothing else is.
    if hello
        && !served
        && let Ok(Ok(Some(ToDaemon::Poke { id }))) =
            timeout(POKE_WAIT, recv::<_, ToDaemon>(&mut reader)).await
    {
        let mut d = shared.lock().await;
        d.refresh(&id);
        d.publish();
    }
}

/// Handles one message. Returns `false` if the client dropped in the meantime.
async fn handle(shared: &Shared, client: u64, msg: ToDaemon) -> bool {
    match msg {
        ToDaemon::Hello { .. } => {}
        ToDaemon::Unknown => {
            if let Some(c) = shared.lock().await.clients.get_mut(&client)
                && !std::mem::replace(&mut c.noted_unknown, true)
            {
                eprintln!(
                    "A client (protocol {}) sent a message type this daemon doesn't know; ignoring it and any more like it",
                    c.version
                );
            }
        }
        ToDaemon::ReloadProfiles => shared.lock().await.reload_profiles(),
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
        ToDaemon::Focus { focused } => {
            if let Some(c) = shared.lock().await.clients.get_mut(&client) {
                c.focused = focused;
            }
        }
    }
    shared.lock().await.clients.contains_key(&client)
}

/// `New`: creates the session, starts a holder process and attaches. On failure it leaves nothing in the list.
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
    let project = spec.project.canonicalize().with_context(|| {
        format!(
            "Couldn't resolve the project directory: {}",
            spec.project.display()
        )
    })?;
    let id = session_id(&project_key(&project), &spec.name);
    let start = {
        let mut d = shared.lock().await;
        if d.closing.contains_key(&id) {
            bail!(
                "A session with the same name is still closing. Try again in a moment: {}",
                spec.name
            );
        }
        // Before anything is written, so a wrong profile leaves no session directory behind.
        session::profile(spec.kind, spec.account.as_deref(), &d.profiles)?;
        if d.sessions.contains_key(&id) || !d.starting.insert(id.clone()) {
            bail!("A session with the same name already exists: {}", spec.name);
        }
        d.start()
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
    let dir = start.state.session_dir(&id);
    // Start it outside the lock. Other sessions' output and clients don't stall while the holder process starts.
    let started = async {
        project::register(&start.state, &meta.project)?;
        ensure_dir(&dir)?;
        meta.save(&dir)?;
        start_holder(&start, &meta).await
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
        // The shell ended right away. A shell session disappears from the list when it ends.
        Ok(Started::Ended) => close_meta(meta, &dir),
        Err(e) => {
            let _ = close_meta(meta, &dir);
            Err(e)
        }
    }
}

/// `Kill`: closes the session and replies after the holder process connection ends (up to `KILL_WAIT`). Even if the reply is followed immediately by creating
/// the same name again, it doesn't tangle with the holder process that is about to end. While waiting, this client's other requests are still handled.
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
                "The holder process didn't exit within {} seconds. It may still be running: {id}",
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

/// `Attach`: if the session was left unattached, first try reattaching. If it's an ended claude or codex session with a resume id,
/// first start it again. Then attach.
async fn attach(shared: &Shared, client: u64, id: &str, size: (u16, u16)) {
    let retry = shared.lock().await.begin_relink(id);
    if let Some(sock) = retry {
        let link = connect_holder(&sock).await;
        let mut d = shared.lock().await;
        let exists = d.sessions.get_mut(id).map(|s| s.busy = false).is_some();
        match link {
            Ok(Some(link)) if exists => install(&mut d, shared, id, link, false),
            // The session was closed while reattaching. End the holder process too.
            Ok(Some(link)) => kill_link(link),
            Ok(None) => d.gone(id),
            Err(e) => {
                let message = format!("Couldn't reattach to the holder process: {e:#}");
                eprintln!("{id}: {message}");
                d.send(client, FromDaemon::Error { message });
            }
        }
        d.publish();
    }
    let resume = shared.lock().await.prepare_resume(id);
    if let Some((start, meta)) = resume {
        let started = start_holder(&start, &meta).await;
        let mut d = shared.lock().await;
        let exists = d.sessions.get_mut(id).map(|s| s.busy = false).is_some();
        match started {
            Ok(Started::Linked(link)) if exists => install(&mut d, shared, id, link, true),
            // The session was closed while restarting. End the new holder process too.
            Ok(Started::Linked(link)) => kill_link(link),
            Ok(Started::Ended) => d.refresh(id),
            Err(e) => {
                let message = format!("Couldn't restart the session: {e:#}");
                eprintln!("{id}: {message}");
                d.send(client, FromDaemon::Error { message });
            }
        }
        d.publish();
    }
    shared.lock().await.attach(client, id, size);
}

/// Sends `Kill` over a holder process connection that has nowhere to write, and drops it.
fn kill_link(link: Link) {
    let mut writer = link.writer;
    tokio::spawn(async move {
        let _ = send(&mut writer, &HoldIn::Kill).await;
    });
}

/// Starts a holder process as a new session leader and attaches to its socket.
///
/// If the holder process ends before attaching, an exit file means the child ended right away (`Ended`),
/// and no exit file means it couldn't start (an error that carries the log contents).
///
/// First the session's profile is resolved (an unknown one refuses to start) and its home made ready with `share::prepare`.
/// Only a home that can't be created stops the start; any other sharing problem is logged and the session starts anyway.
async fn start_holder(start: &Start, meta: &Meta) -> Result<Started> {
    let Start {
        state,
        exe,
        profiles,
        user_home,
    } = start;
    let launch = session::launch(meta, state, exe, profiles, user_home)?;
    if let Some(profile) = session::profile(meta.kind, meta.account.as_deref(), profiles)? {
        let why = |what: &str| format!("{}: profile {}: {what}", meta.id, profile.id);
        match share::prepare(profile, profiles, state, user_home) {
            Ok(report) => report.warnings.iter().for_each(|w| eprintln!("{}", why(w))),
            Err(e) if !profile.home_dir(state, user_home).is_dir() => return Err(e),
            Err(e) => eprintln!("{}", why(&format!("{e:#}"))),
        }
    }
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
            return Err(e).with_context(|| {
                format!("Couldn't remove the previous exit file: {}", exit.display())
            });
        }
    }
    let log_path = state.log_dir().join(format!("hold-{}.log", meta.id));
    let log = File::create(&log_path)
        .with_context(|| format!("Couldn't open the log file: {}", log_path.display()))?;
    let mut cmd = tokio::process::Command::new(exe);
    // portable-pty passes the holder process's environment to the child as it is, so the environment of the shell that started the daemon is cleared here,
    // along with the variables that would route claude or codex around the profile's login.
    let bypass = session::bypass_vars(meta.kind);
    for (key, _) in std::env::vars_os() {
        if key
            .to_str()
            .is_some_and(|key| session::is_leaked_env(key) || bypass.contains(&key))
        {
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
    // SAFETY: Between fork and exec, only setsid, an async-signal-safe function, is called.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("Couldn't start the holder process")?;
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
            bail!("Couldn't start the session: {log}");
        }
        if Instant::now() >= deadline {
            let _ = child.start_kill();
            reap(child);
            bail!(
                "The holder process didn't start within {} seconds",
                START_WAIT.as_secs()
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Reaps the started holder process when it ends. If the daemon ends first, init reaps it.
fn reap(mut child: tokio::process::Child) {
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
}

/// Attaches to the holder socket and reads `Hello` (version check) and the output buffer.
///
/// If the socket file is missing or refuses the connection, there is no holder process, so `Ok(None)`. Any other failure (timeout,
/// version mismatch, etc.) is an error. In that case the holder process may be alive, so the session must not be treated as ended.
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
            return Err(e).with_context(|| {
                format!("Couldn't connect to the holder socket: {}", sock.display())
            });
        }
    };
    let (read, writer) = stream.into_split();
    let mut reader = BufReader::with_capacity(HOLD_READ_BUF, read);
    match timeout(LINK_WAIT, recv::<_, HoldOut>(&mut reader)).await?? {
        Some(HoldOut::Hello { version, .. }) if version == HOLD_VERSION => {}
        Some(HoldOut::Hello { version, .. }) => {
            bail!("Holder process version mismatch (holder {version}, daemon {HOLD_VERSION})")
        }
        other => bail!("The holder process didn't send Hello: {other:?}"),
    }
    let ring = match timeout(LINK_WAIT, recv::<_, HoldOut>(&mut reader)).await?? {
        Some(HoldOut::Output { data }) => data,
        other => bail!("The holder process didn't send the output buffer: {other:?}"),
    };
    Ok(Some(Link {
        reader,
        writer,
        ring,
    }))
}

/// Attaches a holder process connection to the session and rebuilds the screen from the output buffer.
///
/// `fresh` is a holder process just started. The PTY starts at 80x24, and no one has yet answered the queries in the buffer, so
/// answers are sent. When reattaching (daemon restart, etc.), the queries were already answered, so the answers are discarded and the PTY size is fitted to the screen.
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
    // The screen was rebuilt, so send a snapshot again to the clients that were viewing.
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

/// A task that reads a holder process connection in turn. Feeds output to the screen inside the global lock.
///
/// When the reader catches up, a macOS PTY hands data over in chunks of a few dozen bytes, so output lines already in the read buffer
/// are gathered (up to `BATCH_MAX`) and handled at once without waiting. This cuts lock counts and client queue entries.
fn spawn_hold_reader(shared: Shared, id: String, link: u64, mut reader: BufReader<OwnedReadHalf>) {
    tokio::spawn(async move {
        'read: loop {
            let mut msg = recv::<_, HoldOut>(&mut reader).await;
            let mut after = None;
            if let Ok(Some(HoldOut::Output { data })) = &mut msg {
                // If the buffer has a line end, the next message is read without waiting on the socket.
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

/// The connection dropped without `Exited`. If the holder process is alive (only the connection dropped), reattach. If the socket is missing or
/// refuses the connection, the session ended; for any other failure, leave the session and leave it to the next `Attach`.
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
    // If there's an exit file, the child ended. If not, check by socket whether the holder process still exists.
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
            eprintln!("Reattached to the holder process: {id}");
            install(&mut d, &shared, &id, new, false);
            d.publish();
        }
        // It dropped again right after attaching. Don't repeat; leave it to the next Attach.
        Ok(Some(_)) => d.unlink(&id, "the connection dropped again right away"),
        Ok(None) => d.gone(&id),
        Err(e) => d.unlink(&id, &format!("{e:#}")),
    }
}

/// Ends a synchronized update when it exceeds its time limit. If the update restarted and the limit was pushed back, wait further until then.
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

/// If a shell session has no output for `SHELL_QUIET`, sets it to Idle and announces it. If there was output in the meantime, wait that much longer.
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

/// Shows a macOS notification for a session that just started waiting for approval. The command is `MINE_NOTIFY` (default `osascript`).
///
/// It is called under the global lock, so the command starts on a thread of its own, which also reaps it. Nothing waits for it.
fn notify(meta: &Meta, waiting: Option<&str>) {
    let folder = meta
        .project
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let script = format!(
        "display notification {} with title \"mine\" subtitle {}",
        applescript_str(
            waiting
                .filter(|w| !w.is_empty())
                .unwrap_or("Needs approval")
        ),
        applescript_str(&format!("{folder}/{}", meta.name)),
    );
    let cmd = std::env::var_os("MINE_NOTIFY")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "osascript".into());
    std::thread::spawn(move || {
        let spawned = std::process::Command::new(&cmd)
            .arg("-e")
            .arg(&script)
            .stdin(Stdio::null())
            .spawn();
        match spawned {
            Ok(mut child) => {
                let _ = child.wait();
            }
            Err(e) => eprintln!(
                "Couldn't show the notification: {} ({e})",
                cmd.to_string_lossy()
            ),
        }
    });
}

/// An AppleScript string literal. Backslashes and quotes are escaped.
fn applescript_str(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
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
            asking: true,
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

    /// Sends the answers to terminal queries the screen gathered to the holder process as input.
    fn answer_queries(&mut self) {
        let data = self.screen.take_replies();
        if !data.is_empty() {
            self.send_hold(HoldIn::Input { data });
        }
    }

    /// If a synchronized update is in progress and no task is watching, returns its deadline and marks that a task exists.
    fn start_sync_timer(&mut self) -> Option<Instant> {
        if self.sync_timer {
            return None;
        }
        let deadline = self.screen.sync_deadline()?;
        self.sync_timer = true;
        Some(deadline)
    }

    /// Changes the screen and PTY size. Does nothing if the size is the same.
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
        self.clients.insert(
            id,
            Client {
                tx,
                writer,
                subscribed: false,
                attached: None,
                size: None,
                focused: false,
                version: 0,
                noted_unknown: false,
            },
        );
        id
    }

    /// Answers a client's first message (`hello` is its `Hello` version, `None` if it sent something else). Sends `Hello` at the
    /// version both sides speak, then the profile list if the client knows it. A refused client gets an `Error` after the `Hello`,
    /// so an older client still shows its own version-mismatch hint. Returns whether the client is served.
    fn greet(&mut self, client: u64, hello: Option<u32>) -> bool {
        let served = hello.filter(|&v| v >= MIN_CLIENT_VERSION);
        // A v3 client accepts only a Hello of its own version, and a newer client is served at this daemon's.
        let version = served.map_or(VERSION, |v| v.min(VERSION));
        let daemon = DaemonInfo {
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").into(),
            started_ms: self.started_ms,
        };
        let features = FEATURES.iter().map(|f| f.to_string()).collect();
        self.send(
            client,
            FromDaemon::Hello {
                version,
                features,
                daemon: Some(daemon),
            },
        );
        let Some(version) = served else {
            let message = match hello {
                Some(v) => format!(
                    "Protocol version mismatch (client {v}, daemon {VERSION}, oldest client served {MIN_CLIENT_VERSION}). {}",
                    restart_hint(&self.state)
                ),
                None => "The first message must be Hello".into(),
            };
            self.send(client, FromDaemon::Error { message });
            return false;
        };
        if let Some(c) = self.clients.get_mut(&client) {
            c.version = version;
        }
        if version >= PROFILES_SINCE {
            let profiles = self.profile_infos.clone();
            self.send(client, FromDaemon::Profiles { profiles });
        }
        true
    }

    /// `ReloadProfiles`: rereads the profiles and sends the new list to every client that knows it.
    fn reload_profiles(&mut self) {
        (self.profiles, self.profile_infos) = load_profiles(&self.state, &self.user_home);
        let knows: Vec<u64> = self
            .clients
            .iter()
            .filter(|(_, c)| c.version >= PROFILES_SINCE)
            .map(|(&k, _)| k)
            .collect();
        for k in knows {
            let profiles = self.profile_infos.clone();
            self.send(k, FromDaemon::Profiles { profiles });
        }
    }

    /// Puts it in the client queue. If the queue is full or closed, drops that client.
    fn send(&mut self, client: u64, msg: FromDaemon) {
        let Some(c) = self.clients.get(&client) else {
            return;
        };
        if c.tx.try_send(msg).is_err() {
            self.disconnect(client);
        }
    }

    /// Stops the write task and drops the connection right away. After reattaching, it gets a snapshot via `Attach`.
    fn disconnect(&mut self, client: u64) {
        if let Some(c) = self.clients.remove(&client) {
            c.writer.abort();
        }
    }

    /// Sends to every client viewing the session.
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

    /// Project path, list sorted by name.
    fn infos(&self) -> Vec<SessionInfo> {
        let mut infos: Vec<SessionInfo> = self.sessions.values().map(Sess::info).collect();
        infos.sort_by(|a, b| (&a.meta.project, &a.meta.name).cmp(&(&b.meta.project, &b.meta.name)));
        infos
    }

    /// If the list changed, announces it to every subscribed client.
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

    /// Rereads the event log and computes the state. If someone is viewing it, records it as seen now.
    /// Only events after the creation time are looked at, so a session recreated with the same id doesn't inherit the previous session's events.
    fn refresh(&mut self, id: &str) {
        let viewed = self.viewed_by_someone(id);
        let focused = self.clients.values().any(|c| c.focused);
        let dir = self.state.session_dir(id);
        let Some(s) = self.sessions.get_mut(id) else {
            return;
        };
        if viewed {
            let now = now_ms();
            s.viewed_ms = Some(now);
            if let Err(e) = fs::write(dir.join(VIEWED_FILE), now.to_string()) {
                eprintln!("Couldn't save the last-seen time: {id} ({e})");
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
        // A shell session decides Working from output instead of hooks (`active` exists only for shell sessions). If the hook log decided a
        // state other than Idle, follow that.
        if s.active.is_some() && state == SessionState::Idle {
            state = SessionState::Working;
        }
        // Only the step into Needs approval pops a notification, and only while the user isn't looking at a mine window.
        let asking = state == SessionState::Approval;
        let new_request = asking && !s.asking;
        s.asking = asking;
        s.resume_id = resume_id.or_else(|| s.meta.resume_id.clone());
        (s.state, s.waiting) = if s.hold.is_some() || s.unlinked {
            (state, waiting)
        } else {
            (SessionState::Ended, None)
        };
        if new_request && !focused && s.state == SessionState::Approval {
            notify(&s.meta, s.waiting.as_deref());
        }
    }

    /// `List`: recomputes all sessions and replies, and makes this client a subscriber to list changes.
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

    /// Prepares to start an ended session again. If it should be restarted, returns the values needed to start it.
    fn prepare_resume(&mut self, id: &str) -> Option<(Start, Meta)> {
        let dir = self.state.session_dir(id);
        let s = self.sessions.get_mut(id)?;
        // A session that couldn't be attached may have a live process, so don't start another one in the same conversation.
        if s.hold.is_some() || s.unlinked || s.busy || s.meta.kind == Kind::Shell {
            return None;
        }
        s.meta.resume_id = Some(s.resume_id.clone()?);
        s.busy = true;
        if let Err(e) = s.meta.save(&dir) {
            eprintln!("{e:#}");
        }
        let meta = s.meta.clone();
        Some((self.start(), meta))
    }

    fn start(&self) -> Start {
        Start {
            state: self.state.clone(),
            exe: self.exe.clone(),
            profiles: self.profiles.clone(),
            user_home: self.user_home.clone(),
        }
    }

    /// If it's a session left unattached and no other attempt is underway, returns the socket path to reattach to.
    fn begin_relink(&mut self, id: &str) -> Option<PathBuf> {
        let s = self.sessions.get_mut(id)?;
        if !s.unlinked || s.busy {
            return None;
        }
        s.busy = true;
        Some(self.state.hold_sock(id))
    }

    /// Makes the client a session subscriber, fits the size, and sends a snapshot.
    fn attach(&mut self, client: u64, id: &str, size: (u16, u16)) {
        if !self.clients.contains_key(&client) {
            return;
        }
        let Some(s) = self.sessions.get_mut(id) else {
            let message = format!("No such session: {id}");
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

    /// `Input`: sends it to the holder process. If it was waiting for approval, records an `Input` event.
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

    /// `Colors`: makes the outer terminal's colors the default colors of every current screen and of screens to be attached later (`install`).
    /// A screen with no holder process attached has no app to query, and `install` rebuilds the screen when it attaches.
    fn set_colors(&mut self, fg: &str, bg: &str) {
        let (Ok(fg), Ok(bg)) = (fg.parse::<Rgb>(), bg.parse::<Rgb>()) else {
            eprintln!("Invalid color format: {fg} {bg}");
            return;
        };
        self.colors = Some((fg, bg));
        for s in self.sessions.values_mut() {
            s.screen.set_default_colors(fg, bg);
        }
    }

    /// `Resize`: remembers this client's size and fits the session size.
    fn resize(&mut self, client: u64, id: &str, size: (u16, u16)) {
        let Some(c) = self.clients.get_mut(&client) else {
            return;
        };
        c.size = Some(size);
        if let Some(s) = self.sessions.get_mut(id) {
            s.resize(size);
        }
    }

    /// `Kill`: sends `Kill` to the holder process and saves `closed=true`. The directory and history are kept.
    ///
    /// If there was a holder process connection, keeps the session in `closing` until that connection ends, then returns the receiver that closes.
    /// If the session doesn't exist, returns the error text to send to the client.
    fn kill(&mut self, client: u64, id: &str) -> Result<Option<oneshot::Receiver<()>>, String> {
        let Some(s) = self.sessions.remove(id) else {
            return Err(format!("No such session: {id}"));
        };
        s.send_hold(HoldIn::Kill);
        if s.unlinked {
            eprintln!(
                "Closing a session that couldn't attach. The holder process may still be running: {id}"
            );
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

    /// Holder process connection `link` has ended. If it was a connection being closed by `Kill`, removes it from `closing` and wakes whoever is waiting.
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

    /// Holder process output. Feeds it to the screen, answers queries, and sends it to viewing clients.
    /// If a synchronized-update time limit task must be started anew, returns its deadline.
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

    /// The holder process of a shell session sent live output. Announces if it changed from Idle to Working. `true` if a task that watches
    /// for it to go quiet must be started anew.
    ///
    /// The output buffer received on reattaching is fed to the screen by `install`, and answers to terminal queries are input going to the holder process, so
    /// neither comes here. The list isn't recomputed for every output chunk; it's computed and announced only when it changes.
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

    /// Whether the holder process connection attached to the session is `link`.
    fn is_current(&self, id: &str, link: u64) -> bool {
        self.sessions
            .get(id)
            .and_then(|s| s.hold.as_ref())
            .is_some_and(|h| h.link == link)
    }

    /// The holder process of connection `link` reported `Exited`.
    fn on_exit(&mut self, id: &str, link: u64) {
        if self.is_current(id, link) {
            self.gone(id);
        }
    }

    /// Couldn't attach to the holder process, but it may be alive. Leaves the session as it is, without closing it or marking it Done.
    fn unlink(&mut self, id: &str, why: &str) {
        eprintln!("Couldn't connect to the holder process; keeping the session: {id} ({why})");
        if let Some(s) = self.sessions.get_mut(id) {
            s.hold = None;
            s.unlinked = true;
        }
        self.refresh(id);
        self.publish();
    }

    /// The holder process ended. A shell session is removed from the list and closed; claude and codex sessions are left as Done.
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
