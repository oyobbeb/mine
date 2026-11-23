# mine 데스크톱 D1 구현 계획: 창 뼈대와 터미널 칸

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `mine-app`을 실행하면 GPUI 창이 뜬다. 창에는 상태별 작업 목록, 세션 탭, 실행 화면, 터미널 칸이 있다. 이것으로 지금 있는 claude·codex·shell 세션을 띄워 TUI와 tmux 대신 하루를 쓸 수 있다.

**Architecture:**
- 저장소를 Cargo 워크스페이스 모노레포로 나눈다: `mine-core`(공용 lib), `mine-cli`(실행 이름 `mine`), `mine-app`(GPUI).
- 앱은 데몬의 클라이언트일 뿐이다. 1단계와 같은 유닉스 소켓 JSON 줄 프로토콜로 붙는다. 데몬 연결은 별도 스레드의 tokio 런타임이 맡고, futures 채널로 GPUI에 넘긴다.
- 로직은 GPUI 없는 순수 모듈에 두고 단위 테스트한다. GPUI 뷰는 얇게 유지하고 창 캡처로 확인한다.

**Tech Stack:**
- 기존: Rust(edition 2024), tokio, alacritty_terminal 0.26, serde_json.
- 새로 추가: gpui 0.2.2(runtime_shaders), gpui-component 0.4.1, futures.

**Spec:** `docs/superpowers/specs/2026-11-23-mine-desktop-design.md` (D1 부분: 5.1의 워크스페이스와 앱 구조, 5.5, 5.6, 5.9). 바탕 문서는 `docs/superpowers/specs/2026-11-20-mine-multiplexer-design.md`이다.

## Global Constraints

- **작업 위치:** 워크트리 `/Users/me/.gitbutler-worktrees/mine/feat-desktop`, 브랜치 `feat/desktop`이다. 이 브랜치는 `feat/rust-core` 끝(3866764)에서 시작한다. 메인 체크아웃(`~/dev/mine`, Codex가 `dev`에서 작업 중)에는 코드를 커밋하지 않는다.
- **패키지:**
  - `crates/mine-core`: lib, 코드 이름 `mine_core`
  - `crates/mine-cli`: 패키지 `mine-cli`, 실행 이름 `mine`
  - `crates/mine-app`: 실행 이름 `mine-app`
  - 워크스페이스 공통 설정은 `edition = "2024"`, `rust-version = "1.89"`이다. 로컬 rustc는 1.94.1이다.
- **GPUI 판 고정:** `gpui = { version = "=0.2.2", features = ["runtime_shaders"] }`, `gpui-component = "=0.4.1"`이다.
  - 둘 다 `mine-app`에만 들어간다. `mine-core`와 `mine-cli`는 GPUI를 끌어오지 않는다.
  - `runtime_shaders`를 켜는 이유: 이 Mac에는 Metal Toolchain이 없다. 이 조합은 rustc 1.94.1에서 약 1분 만에 빌드되고, 창과 한글이 그려지는 것을 컨트롤러가 확인했다.
  - 테스트는 `gpui`의 `test-support` 기능을 dev-dependency로 쓸 수 있다.
- **그 밖의 새 의존성:** `futures`만 더한다(`mine-core`의 데몬 연결과 `mine-app`). 그 밖에 새 의존성이 필요하면 커밋 본문에 이유를 적는다. Python은 쓰지 않는다. 테스트용 가짜 프로그램은 sh나 Rust로 만든다.
- **프로토콜:**
  - 클라이언트↔데몬 `VERSION = 3`이다. D1은 `ToDaemon::Focus { focused: bool }` 하나를 더한다. D2가 채팅 메시지를 더할 때 다시 올린다.
  - 데몬↔보관 `HOLD_VERSION = 1`은 그대로다.
- **색:** 사용자가 승인한 목업 색이다.

  | 용도 | 색 |
  |---|---|
  | 창 바탕 | `#1e2128` |
  | 목록 | `#23252b` |
  | 제목줄·띠 | `#1b1d23` |
  | 선택 줄 | `#2f323a` |
  | 경계 | `#2c2f36` |
  | 글 | `#d0d3da` |
  | 제목 글 | `#e6e6e6` |
  | 흐린 글 | `#8a8f99` / `#6b707a` |
  | 대기 문구 | `#c9a85c` |
  | 승인 ◆ | `#e5c07b` |
  | 칩 A(claude-a) | `#e5c07b` |
  | 칩 B(claude-b) | `#c678dd` |
  | 칩 C(codex) | `#56b6c2` |

  shell에는 칩이 없다.
- **터미널 색:**
  - 글 `#d0d3da`, 바탕 `#1e2128`, 커서 `#d0d3da`. 앱은 붙을 때 이 글과 바탕 색을 `ToDaemon::Colors`로 데몬에 보낸다.
  - ANSI 0–7: `#1e2128 #e06c75 #98c379 #e5c07b #61afef #c678dd #56b6c2 #d0d3da`
  - ANSI 8–15: `#5c6370 #ef8a92 #b5d99c #f0d197 #8cc5f5 #d898e6 #7fcbd5 #ffffff`
  - 16–255는 xterm 기본 6×6×6 큐브와 회색 단계다.
- **글꼴:** UI는 GPUI 시스템 글꼴, 터미널은 `Menlo` 13px다. `ponytail:` 설정은 나중에 붙인다.
- **상태:** 기호와 순서는 `◆ 승인 대기 → ◐ 작업 중 → ✓ 끝남 → ○ 대기`다. 종료된 세션은 끝남 그룹에 넣고 `종료됨`으로 표시한다. 대기 그룹은 처음에 접혀 있다.
- **단축키:** 터미널 칸이 포커스를 가지면 ⌘ 없는 키는 모두 세션으로 간다.

  | 단축키 | 동작 |
  |---|---|
  | ⌘N | 새 작업(실행 화면) |
  | ⌘W | 세션 닫기(확인 창을 거친다) |
  | ⌘1–⌘9 | 탭 고르기 |
  | ⌘[ / ⌘] | 이전 / 다음 세션 |
  | ⌘⇧[ / ⌘⇧] | 이전 / 다음 프로젝트 |
  | ⌘B | 왼쪽 목록 접기 |
  | ⌘C / ⌘V | 복사 / 붙여넣기 |
  | ⌘F | 스크롤백 찾기 |
  | ⌘Q | 앱 끝내기(세션은 남는다) |

- **화면 확인:** 캡처할 때는 `mine-app` 창 영역만 찍는다. 창 위치와 크기는 System Events의 `tell process "mine-app" to get {position, size} of window 1`로 얻고, `screencapture -x -R x,y,w,h <png>`로 찍는다. **전체 화면은 절대 찍지 않는다.** 사용자의 다른 창은 사적인 내용이다.
  - 확인용 앱은 짧은 임시 `MINE_STATE_DIR`(`mktemp -d /tmp/mXXXX`)과 임시 프로젝트 폴더(`.git` 폴더 포함)로 띄운다. 끝나면 앱, 그 데몬, 보관 프로세스를 끝낸다.
  - 사용자의 `~/.cargo/bin/mine daemon`은 절대 건드리지 않는다.
- **검사 명령:** `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
- **커밋 규칙:**
  - GitButler로 워크트리에서 `-b` 없이 `but commit -F - <ids>`로 한다. git 쓰기 명령은 쓰지 않는다. 한 태스크가 한 커밋이다.
  - 제목은 `type(scope): 한글 명령형 요약`이다. 본문에는 바꾼 것·이유·검증을 쓴다. 마지막 줄은 `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`이다.

## Review Focus

1. **한글 조합:** 터미널 칸에서 한글을 칠 때, 조합 중인 글자가 커서 자리에 보여야 한다. 확정된 글자는 세션에 정확히 한 번 간다. 자모가 빠지거나 두 번 가면 안 된다. → Task 5 `ime_commits_once`
2. **모드에 따른 키:** Shift+Enter, ⌃C, Esc, 방향키(DECCKM), kitty 플래그 1에서 ⌃A가 세션이 바라는 바이트로 가야 한다. → Task 5 `encode_key_*`
3. **데몬 재시작과 앱 재시작:** 앱을 끄거나 데몬이 다시 떠도 세션은 산다. 앱은 다시 붙어 같은 화면을 보여 준다. → Task 2 `link_reconnects_after_daemon_restart`, Task 4 `reattach_after_reconnect`
4. **창 크기 변경과 목록 접기:** 크기가 바뀌면 세션이 새 행·열 수를 받는다. 같은 크기면 `Resize`를 다시 보내지 않는다. → Task 4 `grid_size_*`, `resize_only_on_change`
5. **앱이 꺼져 있을 때의 승인 요청과 Dock 실행:** 승인 요청이 오면 알림이 뜨고, 앱이 앞에 있으면 안 뜬다. Dock에서 띄운 앱이 데몬을 띄워도 `claude`를 찾는다. → Task 7 `notify_*`, `login_env_*`

---

### Task 1: 모노레포로 나누기

**Files:**
- Create:
  - `Cargo.toml`(워크스페이스)
  - `crates/mine-core/Cargo.toml`
  - `crates/mine-core/src/lib.rs`
  - `crates/mine-core/src/{launch.rs,search.rs,keys.rs,select.rs}`(순수 로직을 옮겨 옴)
  - `crates/mine-cli/Cargo.toml`
- Move:
  - `src/{paths,project,session,proto,screen,hooks,conn}.rs` → `crates/mine-core/src/`
  - `src/client/view.rs` → `crates/mine-core/src/view.rs`
  - `src/{main,daemon,hold}.rs` → `crates/mine-cli/src/`
  - `src/client/*` → `crates/mine-cli/src/client/`
  - `tests/{daemon,hold}.rs` → `crates/mine-cli/tests/`
- Modify: `README.md`(설치 명령), `.gitignore`(필요하면)

**Interfaces:**
- **Produces** (`mine_core`의 공개 모듈): `paths`, `project`, `session`, `proto`, `screen`, `hooks`, `conn`, `view`, 그리고 새로 옮긴 다음 모듈:
  - `launch`: `pub enum Choice`, `pub fn next_name(sessions: &[SessionInfo], project: &Path) -> String`, `pub fn new_session(choice: Choice, name: &str, project: &Path) -> Option<NewSession>`, `pub fn tilde(path: &Path, home: Option<&Path>) -> String`. 지금 `client/launcher.rs`에 있는 것을 그대로 옮긴다. TUI 쪽에는 `choice_of_key`, `hit`, `draw`, `KEYS`가 남는다.
  - `search`: `pub fn find(lines: &[String], query: &str) -> Vec<(usize, usize)>`. `client/copymode.rs`에서 옮긴다.
  - `keys`: `pub struct KeyModes { .. }`와 `pub fn KeyModes::of(screen: &Screen) -> KeyModes`. `client/mod.rs`에서 옮겨 필드를 `pub`으로 연다. `mode_diff`는 TUI에 남는다.
  - `select`: `MouseKind`, `MouseEv`(지금 `client/input.rs`에 있음), `Selection`, `selected_text`, `wants_mouse`, `session_takes`, `encode_sgr`. `osc52`, `local`, `copy`, `paint_selection`, `Select`는 TUI에 남는다.
- **Consumes:** 없음(기계적 이동)

- [ ] **Step 1: 워크스페이스를 만들고 파일을 옮긴다.** 뿌리 `Cargo.toml`은 `[workspace] members = ["crates/*"] resolver = "3"`과 `[workspace.package]`(edition, rust-version)를 갖는다. 의존성은 `[workspace.dependencies]`로 모아도 된다(구현자 선택). `mine-cli`의 `[[bin]] name = "mine"`. `mine-cli`의 코드는 `crate::paths` 대신 `mine_core::paths`처럼 가져다 쓴다.
- [ ] **Step 2: 공용 순수 로직을 core로 옮기고, TUI가 그것을 쓰게 바꾼다.** 동작은 하나도 바꾸지 않는다. 옮긴 함수의 기존 테스트도 함께 옮긴다.
- [ ] **Step 3: 확인한다.** Run: 검사 명령 → 기존 테스트가 하나도 빠지지 않고 통과해야 한다. 지금은 단위 271, daemon 24, hold 12다. 단위 테스트는 `mine-core`와 `mine-cli`로 나뉘지만 합은 271이어야 한다. 합이 바뀌면 이유를 보고한다. 통합 테스트는 `mine_core::…`를 가져다 쓰고, 바이너리는 지금처럼 `env!("CARGO_BIN_EXE_mine")`으로 띄운다. `cargo tree -p mine-cli | grep -c gpui`는 `0`이어야 한다. `cargo run -p mine-cli -- --help`는 기존 도움말을 보여야 한다.
- [ ] **Step 4: README의 설치·개발 명령을 바꾼다.** `cargo install --path crates/mine-cli`, 검사 명령에 `--workspace`.
- [ ] **Step 5: 커밋한다.** `refactor(workspace): mine-core와 mine-cli로 모노레포를 나눈다`

---

### Task 2: mine-app 뼈대, 데몬 연결, 상태별 목록

**Files:**
- Create:
  - `crates/mine-app/Cargo.toml`
  - `crates/mine-app/src/main.rs`(앱 시작, 창 열기)
  - `src/theme.rs`(색 상수, ANSI 팔레트)
  - `crates/mine-core/src/link.rs`(데몬 연결 스레드. GPUI와 무관하므로 core에 두고 `mine-cli`의 통합 테스트로 시험한다)
- Modify: `crates/mine-core/src/conn.rs`(데몬 실행 파일과 환경을 받는 연결 함수)
- Test: `crates/mine-cli/tests/link.rs`
  - `src/model.rs`(순수 상태)
  - `src/workspace.rs`(뿌리 뷰: 목록 | 가운데 | 접힌 Changes 자리)
  - `src/sidebar.rs`

**Interfaces:**
- **Consumes:** `mine_core::proto::{ToDaemon, FromDaemon}`, `mine_core::view::{groups, expand, order, folder_name}`, `mine_core::paths::State`
- **Produces:**
  ```rust
  // mine_core::conn — 지금 spawn_daemon은 current_exe()로 데몬을 띄운다. 앱에서는 그것이 mine-app이라 데몬 하위 명령이 없다.
  pub async fn connect_or_spawn_async_with(state: &State, daemon_exe: &Path, spawn_env: Option<&[(String, String)]>) -> Result<AsyncConn>
  // 기존 connect_or_spawn_async(state)는 current_exe()와 None으로 이것을 부른다. spawn_env가 Some이면 env_clear 뒤 그 환경 + MINE_STATE_DIR로 띄운다(Task 7이 씀).
  // mine_core::link
  pub enum LinkEvent { Connected, Msg(FromDaemon), Disconnected(String) }
  pub struct DaemonLink { pub tx: futures::channel::mpsc::UnboundedSender<ToDaemon>,
                          pub rx: futures::channel::mpsc::UnboundedReceiver<LinkEvent> }
  pub fn start(state: State, daemon_exe: PathBuf, spawn_env: Option<Vec<(String, String)>>) -> DaemonLink
  pub fn find_mine(override_var: Option<OsString> /*MINE_BIN*/, exe_dir: Option<&Path>, path_var: Option<OsString>) -> Option<PathBuf>
  // 찾는 순서: MINE_BIN → 현재 실행 파일 옆의 `mine` → PATH의 `mine`
  // model.rs
  pub enum Effect { Send(ToDaemon) }
  pub struct Model { pub sessions: Vec<SessionInfo>, pub filter: Option<PathBuf>, pub attached: Option<String>,
                     pub toggled: HashSet<usize>, pub sidebar_open: bool, pub connected: bool, /* … */ }
  impl Model { pub fn new(current_project: PathBuf) -> Self;
               pub fn on_link(&mut self, ev: LinkEvent) -> Vec<Effect>;
               pub fn visible_groups(&self) -> Vec<Group>; }
  ```

- [ ] **Step 1: `mine_core::link`를 만들고 conn에 `connect_or_spawn_async_with`를 더한다.**
  - 별도 `std::thread`의 tokio 런타임에서 `connect_or_spawn_async`로 붙는다.
  - 붙으면 `LinkEvent::Connected`를 보내고, 받은 메시지는 `Msg`로 넘긴다.
  - 연결이 끊기면 `Disconnected(사유)`를 보낸다. 그 뒤 0.5초부터 시작해 최대 5초 간격으로 다시 붙는다.
  - 다시 붙은 뒤의 Hello·List·Attach는 모델이 `Connected`를 받고 보낸다.
  - `tx`로 받은 메시지는 연결이 있을 때만 보낸다. 연결이 없을 때 온 메시지는 버린다.
- [ ] **Step 2: 실패하는 테스트를 쓴다.**
  - `crates/mine-app/src/model.rs`의 `#[cfg(test)]`:
    - `connected_subscribes_and_reports_colors`: `Connected`를 받으면 `[Send(List), Send(Colors{fg:"#d0d3da",bg:"#1e2128"})]`를 낸다.
    - `sessions_group_by_state`: Approval·Working·Ended·Idle 세션이 ◆ ◐ ✓ ○ 순서로 묶이고, Idle은 접혀 있다.
    - `disconnect_marks_offline`: `Disconnected`를 받으면 `connected == false`가 되고 세션 목록은 남는다.
  - `mine_core::link`의 `find_mine_order`: `MINE_BIN`이 있으면 그것, 없으면 실행 파일 옆, 없으면 PATH에서 찾는다. 다 없으면 `None`이다(임시 폴더에 가짜 실행 파일을 두고 시험).
  - `crates/mine-cli/tests/link.rs`의 `link_reconnects_after_daemon_restart`:
    - 임시 상태 폴더에서 `start(state, env!("CARGO_BIN_EXE_mine"), None)`하고 `Connected`를 받는다.
    - `daemon.pid`의 데몬을 SIGKILL한다.
    - 30초 안에 `Disconnected`와 다시 `Connected`가 오고, 그 뒤 `List`에 `Sessions`가 응답한다.
    - 테스트가 띄운 데몬은 끝에 정리한다. 기존 `tests/daemon.rs`의 정리 방식을 따른다.
- [ ] **Step 3: 실패를 확인한다.** Run: `cargo test -p mine-app && cargo test -p mine-cli --test link && cargo test -p mine-core link` → FAIL
- [ ] **Step 4: 모델, 테마, 뷰를 구현한다.** 앱은 시작할 때 `find_mine(MINE_BIN, 실행 파일 폴더, PATH)`로 `mine`을 찾는다. 못 찾으면 창에 `mine 실행 파일을 찾지 못했다 · cargo install --path crates/mine-cli`를 띄운다.
  - 창은 1200×800이고 바탕은 `#1e2128`이다.
  - 왼쪽 목록(260px)의 맨 위에는 "＋ 새 작업 ⌘N"과 프로젝트 고르기가 있다. 프로젝트 고르기는 "전체"와 등록된 프로젝트들이고, 승인 대기가 있는 프로젝트에는 `◆`가 붙는다.
  - 그 아래는 상태 그룹이다. 줄은 두 줄로, 첫 줄은 이름과 칩, 둘째 줄은 `프로젝트 · 대기문구|상태 · N분 전`이다. 그룹 머리를 누르면 접히거나 펼쳐진다. 맨 아래에 "＋ 프로젝트 추가"가 있다(Task 3에서 동작을 붙인다).
  - 연결이 끊긴 동안 목록 위에 `데몬 연결이 끊겼다 · 다시 연결 중`을 띄운다.
  - 가운데는 Task 3·4가 채운다. 이 태스크에서는 "세션을 고르세요"라는 자리만 둔다.
  - 오른쪽에는 접힌 Changes 자리(폭 0, D3)를 둔다.
  - `gpui-component`의 `Root`, 테마, 사이드바 부품을 써도 된다. 색은 Global Constraints를 따른다.
  - "N분 전"은 `SessionInfo`에 시각이 없으면 생략하고, 무엇을 썼는지 보고한다.
  - 창을 닫아도 앱은 끝나지 않고 Dock에 남는다(macOS 관례). Dock 아이콘을 누르면 창을 다시 연다(GPUI `on_reopen`). ⌘Q는 앱만 끝내고, 데몬과 세션은 남는다.
- [ ] **Step 5: 통과를 확인하고 창을 캡처한다.**
  - Run: 검사 명령 → PASS.
  - 임시 상태 폴더에서 `mine new`로 세션 둘을 만든 뒤 `cargo run -p mine-app`을 띄운다. 창 영역만 캡처해 보고서에 경로를 적는다.
- [ ] **Step 6: 커밋한다.** `feat(app): GPUI 창 뼈대와 상태별 작업 목록을 추가한다`

---

### Task 3: 탭, 실행 화면, 단축키, 세션 만들기와 닫기

**Files:**
- Create: `crates/mine-app/src/{tabs.rs,launcher.rs,actions.rs}`
- Modify: `src/model.rs`, `src/workspace.rs`, `src/sidebar.rs`

**Interfaces:**
- **Consumes:** `mine_core::launch::{Choice, next_name, new_session}`, `mine_core::project::register`, Task 2의 `Model`과 `Effect`
- **Produces:**
  - 모델 메서드: `Model::attach(&mut self, id: &str) -> Vec<Effect>`, `Model::step_session(&mut self, delta: i32) -> Vec<Effect>`, `Model::step_project(&mut self, delta: i32)`, `Model::launch(&mut self, choice: Choice) -> Vec<Effect>`, `Model::tabs(&self) -> Vec<usize>`(선택 프로젝트의 세션, 이름순)
  - GPUI 액션: `NewTask`, `CloseSession`, `SelectTab(usize)`, `PrevSession`, `NextSession`, `PrevProject`, `NextProject`, `ToggleSidebar`

- [ ] **Step 1: 실패하는 테스트를 쓴다.** 모두 `model.rs`에 둔다.
  - `launch_creates_t_n_and_attaches_when_listed`: `launch(Claude A)`가 `New{name:"t1",kind:Claude,account:Some("claude-a")}`를 낸다. 그 세션이 목록에 나타나면 `Attach`를 낸다.
  - `tabs_follow_selected_project`: 고른 프로젝트의 세션만 탭에 나오고, 이름순이다.
  - `step_session_wraps`: 다음·이전 세션이 보이는 순서로 돌고, 끝에서 처음으로 넘어간다.
  - `close_requires_confirmation`: `CloseSession`은 확인을 거쳐야 `Kill`을 낸다. 확인 창은 GPUI `window.prompt`를 쓴다. 모델은 확인 결과를 받는 메서드로 시험한다.
  - `no_session_shows_launcher`: 붙은 세션이 없으면 실행 화면이 열린 상태가 된다.
- [ ] **Step 2: 실패를 확인한다.** Run: `cargo test -p mine-app model` → FAIL
- [ ] **Step 3: 구현한다.**
  - **탭:** 가운데 위에 둔다. 탭에는 상태 기호, 이름, 칩을 붙이고, 끝에 "＋"를 둔다.
  - **실행 화면:** 가운데에 둔다. 1단계 TUI와 같은 내용이다.
    - 프로젝트 이름과 `~` 경로, "새 세션"
    - ↵ claude-a [A] · b claude-b [B] · x codex [C] · s shell · n 이름·모델 직접 입력
    - `⌘W 닫기`
  - **실행 화면 동작:**
    - 줄을 클릭해도 고를 수 있다.
    - `n`은 `gpui-component` 입력 상자를 연다. 입력은 `mine_core::view::parse_new` 형식이다.
    - 붙은 세션 위에서 ⌘N으로 열었으면 Esc로 닫는다.
    - D1의 claude·codex 세션은 모두 터미널 모드다. "터미널로 열기" 선택지는 D2에서 더한다.
  - **단축키:** Global Constraints 표대로 묶는다.
  - **"＋ 프로젝트 추가":** 폴더 고르기(`cx.prompt_for_paths`, 폴더만)로 고른 폴더를 `project::register`로 등록하고 그 프로젝트를 고른다. 홈 폴더는 등록하지 않는 규칙을 따른다.
- [ ] **Step 4: 통과를 확인하고 창을 캡처한다.** Run: 검사 명령 → PASS. 실행 화면과 탭이 보이는 창 영역을 캡처한다.
- [ ] **Step 5: 커밋한다.** `feat(app): 세션 탭과 키 하나로 시작하는 실행 화면을 추가한다`

---

### Task 4: 터미널 칸 그리기

**Files:**
- Create:
  - `crates/mine-app/src/terminal/mod.rs`(뷰, 붙기·크기·스크롤 상태)
  - `crates/mine-app/src/terminal/render.rs`(순수: 셀 → 글자 묶음)
- Modify: `src/model.rs`(Snapshot·Output으로 화면 유지), `src/workspace.rs`

**Interfaces:**
- **Consumes:** `mine_core::screen::Screen`(`new`, `feed`, `snapshot`, `resize`, `scroll`, `term`, `cursor`, `mode`, `sync_deadline`, `end_sync`, `take_replies`), `theme`의 ANSI 팔레트
- **Produces:**
  ```rust
  // render.rs
  pub struct CellRun { pub col: u16, pub cells: u16, pub text: String, pub fg: u32 /*0xRRGGBB*/, pub bg: Option<u32>,
                       pub bold: bool, pub italic: bool, pub underline: bool, pub strike: bool, pub dim: bool,
                       pub link: Option<String> }
  pub fn row_runs(screen: &Screen, row: usize) -> Vec<CellRun>   // 화면에 보이는 row(0부터)
  pub fn grid_size(pane_w: f32, pane_h: f32, cell_w: f32, line_h: f32) -> (u16, u16)  // 최소 (1,1)
  pub fn scroll_lines(acc: &mut f32, delta_px: f32, line_h: f32) -> i32
  ```

- [ ] **Step 1: 실패하는 테스트를 쓴다.**
  - `render.rs`:
    - `row_runs_merges_same_style`: `"ab\x1b[31mcd"` → 두 묶음. 둘째의 fg는 `0xe06c75`다.
    - `row_runs_wide_char_counts_two_cells`: `"한a"`에서 `한`은 `cells == 2`이고, 다음 묶음은 `col == 2`다.
    - `row_runs_inverse_swaps_colors`
    - `row_runs_keeps_link`: OSC 8 링크 셀 묶음의 `link == Some(url)`.
    - `row_runs_indexed_and_truecolor`: 인덱스 196과 `38;2;1;2;3`이 각각 `0xff0000`, `0x010203`이다.
    - `grid_size_floors_and_clamps`: `(800.0, 600.0, 8.0, 17.0)` → `(100, 35)`. `(0.0, 0.0, 8.0, 17.0)` → `(1, 1)`.
    - `scroll_lines_accumulates`
  - `model.rs`:
    - `resize_only_on_change`: 같은 크기를 두 번 알리면 `Resize`는 한 번만 나간다.
    - `reattach_after_reconnect`: 붙은 상태에서 `Disconnected` 다음 `Connected`를 받으면 같은 세션에 `Attach`를 다시 낸다.
    - `snapshot_builds_screen_at_pane_size`
- [ ] **Step 2: 실패를 확인한다.** Run: `cargo test -p mine-app` → FAIL
- [ ] **Step 3: 구현한다.**
  - 터미널 칸은 보이는 행만 그린다. 행마다 `row_runs`로 GPUI 글자 묶음과 배경 사각형을 만든다.
  - 커서는 블록, 밑줄, 막대 모양 중 세션 모드를 따른다. 모르면 블록이다.
  - 스크롤해서 과거를 보는 동안에는 커서를 숨긴다.
  - 셀 크기는 `Menlo` 13px의 'M' 폭과 줄 높이로 정한다.
  - 칸 크기가 바뀌면 `grid_size`로 행·열을 구한다. 바뀌었을 때만 `Resize`를 보내고, 처음에는 그 크기로 `Attach`를 보낸다.
  - `Snapshot`을 받으면 그 크기의 `Screen`을 새로 만들어 먹이고, `Output`은 이어서 먹인다. 클라이언트 `Screen`의 질의 응답(`take_replies`)은 버린다. 응답은 데몬이 한다.
  - `sync_deadline`이 지나면 `end_sync` 후 다시 그린다. GPUI 타이머를 쓴다.
  - 트랙패드 스크롤은 `scroll_lines`로 줄 수로 바꿔 `screen.scroll`에 넘긴다. 세션이 마우스 보고를 켰으면 Task 6이 처리한다.
  - 링크 셀에서 누른 자리와 같은 자리에서 손을 떼면(끌지 않으면) `cx.open_url`로 연다.
  - 다시 그리기는 Output이 올 때마다 하되, 한 프레임에 한 번으로 묶는다.
- [ ] **Step 4: 통과를 확인하고 창을 캡처한다.**
  - Run: 검사 명령 → PASS.
  - 임시 상태 폴더에서 shell 세션을 만들고 `printf '\e[31m빨강\e[0m 한글 테스트\n'; ls -la`를 친다. 이 터미널 칸을 캡처한다. 입력은 Task 5 전이므로 `mine new`와 미리 실행할 명령이 있는 가짜 셸로 대신한다.
- [ ] **Step 5: 커밋한다.** `feat(app): 터미널 칸에서 세션 화면을 그리고 크기와 스크롤을 맞춘다`

---

### Task 5: 터미널 입력: 키, 한글 입력기, 붙여넣기

**Files:**
- Create:
  - `crates/mine-app/src/terminal/keys.rs`(순수: 키 → 바이트)
  - `crates/mine-app/src/terminal/ime.rs`(순수: 조합 상태)
- Modify: `src/terminal/mod.rs`(GPUI 키 이벤트와 `EntityInputHandler` 구현)

**Interfaces:**
- **Consumes:** `mine_core::keys::KeyModes`(Task 1), `Screen::mode`(BRACKETED_PASTE)
- **Produces:**
  ```rust
  pub struct KeyIn { pub key: String, pub key_char: Option<String>, pub ctrl: bool, pub alt: bool, pub shift: bool, pub cmd: bool }
  pub fn encode_key(k: &KeyIn, m: &KeyModes) -> Option<Vec<u8>>      // ⌘ 조합과 일반 글자는 None (글자는 입력기 경로)
  pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8>
  pub struct Ime { /* 조합 중 글자 */ }
  impl Ime { pub fn mark(&mut self, text: &str); pub fn commit(&mut self, text: &str) -> Vec<u8>; pub fn unmark(&mut self);
             pub fn marked(&self) -> Option<&str>; }
  ```

- [ ] **Step 1: 실패하는 테스트를 쓴다.**
  - `keys.rs`의 `encode_key_*`. 표의 각 줄이 `assert_eq!` 하나다. "kitty1"은 kitty 플래그 1(disambiguate)이 켜진 모드이고, "mok2"는 modifyOtherKeys 2다.

    | 입력 | 모드 | 기대 |
    |---|---|---|
    | enter | 기본 | `\r` |
    | shift+enter | 기본 | `\r` |
    | shift+enter | kitty1 | `\x1b[13;2u` |
    | shift+enter | mok2 | `\x1b[27;2;13~` |
    | escape | 기본 | `\x1b` |
    | escape | kitty1 | `\x1b[27u` |
    | ctrl+c | 기본 | `\x03` |
    | ctrl+a | kitty1 | `\x1b[97;5u` |
    | up | 기본 | `\x1b[A` |
    | up | DECCKM | `\x1bOA` |
    | shift+up | 기본 | `\x1b[1;2A` |
    | backspace | 기본 | `\x7f` |
    | tab | 기본 | `\t` |
    | shift+tab | 기본 | `\x1b[Z` |
    | home / end / pageup / pagedown / delete | 기본 | `\x1b[H` `\x1b[F` `\x1b[5~` `\x1b[6~` `\x1b[3~` |
    | f1 / f5 | 기본 | `\x1bOP` `\x1b[15~` |
    | cmd+c | 아무 모드 | `None` |
    | 일반 글자 `a` | 기본 | `None` |

  - `paste_bytes_wraps_when_bracketed`
  - `ime.rs`:
    - `ime_commits_once`: `mark("ㅎ")`, `mark("하")`, `mark("한")`, `commit("한")`이 차례로 오면 `commit`이 `"한".as_bytes()`를 돌려준다. 그 뒤 `marked() == None`이다.
    - `ime_unmark_sends_nothing`
- [ ] **Step 2: 실패를 확인한다.** Run: `cargo test -p mine-app terminal` → FAIL
- [ ] **Step 3: 구현한다.**
  - **키 경로:** 터미널 칸의 키 입력에서 `encode_key`가 `Some`이면 `Input`으로 보낸다.
  - **글자 경로:** 글자 입력은 GPUI `EntityInputHandler`로 받는다.
    - `replace_and_mark_text_in_range`는 `Ime::mark`다.
    - `replace_text_in_range`는 `Ime::commit`이고, 돌려받은 바이트를 `Input`으로 보낸다.
    - `bounds_for_range`는 커서 셀 사각형을 돌려준다. 입력기 후보 창 위치에 쓰인다.
  - **조합 표시:** 조합 중인 글자는 커서 자리에 밑줄로 그린다.
  - **kitty 플래그:** 1만 반영한다. `ponytail:` 2·4·8·16은 D1에서 무시하고, 앱이 요청하면 그때 넓힌다.
  - **Option 키:** macOS 기본대로 글자를 만든다. Option을 Meta로 쓰는 설정은 나중에 넣는다(`ponytail:`).
  - **붙여넣기:** ⌘V는 클립보드 글을 `paste_bytes`로 보낸다. 세션의 BRACKETED_PASTE 모드를 따른다.
  - 키를 보낼 때 스크롤해 있었으면 맨 아래로 돌아온다.
- [ ] **Step 4: 통과를 확인한다.**
  - Run: 검사 명령 → PASS.
  - 임시 상태 폴더의 shell 세션에서 `cat -v`를 띄운다. ⌃C, 방향키, Shift+Enter(세션이 `printf '\e[>4;2m'`로 modifyOtherKeys를 켠 뒤)가 기대한 바이트로 가는지 확인한다. 키는 System Events `keystroke`·`key code`로 보내도 된다. 창을 캡처한다.
  - 한글 조합은 자동화가 어려우므로 보고서에 "사용자 확인 필요"로 적는다.
- [ ] **Step 5: 커밋한다.** `feat(app): 터미널 칸에서 세션 모드에 맞춰 키와 한글 입력을 보낸다`

---

### Task 6: 마우스 선택과 복사, 마우스 보고, ⌘F 찾기

**Files:**
- Create:
  - `crates/mine-app/src/terminal/mouse.rs`(순수: 위치 → 셀, 선택 상태)
  - `crates/mine-app/src/terminal/find.rs`(찾기 상자 상태)
- Modify: `src/terminal/mod.rs`

**Interfaces:**
- **Consumes:** `mine_core::select::{Selection, selected_text, wants_mouse, session_takes, encode_sgr, MouseEv, MouseKind}`, `mine_core::search::find`, `Screen::history_text`
- **Produces:**
  ```rust
  pub fn cell_at(x: f32, y: f32, cell_w: f32, line_h: f32, cols: u16, rows: u16) -> (u16, u16)  // 칸 안으로 자름
  pub struct FindState { pub query: String, pub hits: Vec<(usize, usize)>, pub current: usize }
  impl FindState { pub fn search(&mut self, lines: &[String]); pub fn next(&mut self) -> Option<(usize,usize)>; pub fn prev(&mut self) -> Option<(usize,usize)>; }
  ```

- [ ] **Step 1: 실패하는 테스트를 쓴다.**
  - `cell_at_clamps`
  - `shift_drag_selects_even_when_session_wants_mouse`: 세션이 마우스를 원해도 Shift를 누르고 끌면 선택이 된다. 모델 수준에서 시험한다.
  - `mouse_forwarded_when_session_wants`: 마우스 보고가 켜진 세션에는 SGR 바이트가 간다. 1-기준 좌표다.
  - `find_wraps_both_ways`
  - `copy_uses_selected_text`: 끌어서 고른 범위가 `selected_text`와 같다. 자동 줄바꿈된 줄은 잇는다.
- [ ] **Step 2: 실패를 확인한다.** Run: `cargo test -p mine-app terminal` → FAIL
- [ ] **Step 3: 구현한다.**
  - **선택:** 끌면 고른다. 고른 영역은 `#2f323a` 바탕에 글을 그대로 둔다. ⌘C는 `cx.write_to_clipboard`로 복사한다. 클릭하면 선택이 풀린다. 새 출력이 와도 선택은 남는다. 화면 셀 기준이므로 스크롤하면 지운다.
  - **마우스 보고:** 세션이 마우스 보고를 켰고 Shift를 누르지 않았으면, 누름·뗌·끌기·휠을 `encode_sgr`로 보낸다. 1단계 Task 10의 규칙과 같다(R26의 칸 밖 뗌 포함).
  - **⌘F:** 터미널 칸 오른쪽 위에 찾기 상자를 연다(`gpui-component` 입력 상자). 입력하면 `find`로 찾는다. Enter는 다음, Shift+Enter는 이전, Esc는 닫기다. 현재 일치 줄이 보이도록 스크롤하고, 일치한 곳을 선택 색으로 칠한다. 대소문자는 가리지 않는다.
- [ ] **Step 4: 통과를 확인하고 창을 캡처한다.** Run: 검사 명령 → PASS. `seq 1 500` 출력에서 ⌘F로 `250`을 찾은 화면을 캡처한다.
- [ ] **Step 5: 커밋한다.** `feat(app): 터미널 칸에 드래그 복사와 마우스 보고, 찾기를 더한다`

---

### Task 7: 데몬 쪽: Focus, 승인 알림, 로그인 셸 환경

**Files:**
- Modify:
  - `crates/mine-core/src/proto.rs`(`VERSION = 3`, `ToDaemon::Focus`)
  - `crates/mine-core/src/conn.rs`(데몬을 띄울 때 환경)
  - `crates/mine-cli/src/daemon.rs`(클라이언트별 focused, 알림)
  - `crates/mine-app/src/{link.rs,workspace.rs}`(창 포커스를 알리기, 로그인 셸 환경으로 띄우기)
- Create: `crates/mine-core/src/env.rs`
- Test: `crates/mine-cli/tests/daemon.rs`

**Interfaces:**
- **Produces:**
  ```rust
  // proto.rs
  ToDaemon::Focus { focused: bool }
  // env.rs
  pub fn parse_env0(bytes: &[u8]) -> Vec<(String, String)>
  pub fn login_env(shell: &str, timeout: std::time::Duration) -> Option<Vec<(String, String)>>   // `$shell -l -c 'env -0'`
  // conn.rs: Task 2의 connect_or_spawn_async_with(state, daemon_exe, spawn_env)를 그대로 쓴다. 이 태스크는 spawn_env를 채운다.
  ```

- [ ] **Step 1: 실패하는 테스트를 쓴다.**
  - `env.rs`:
    - `parse_env0_splits_nul_and_first_equals`: `b"PATH=/a:/b\0X=1=2\0"` → `[("PATH","/a:/b"),("X","1=2")]`.
    - `login_env_reads_fake_shell`: 임시 셸 스크립트가 `printf 'PATH=/fake/bin\0'`를 내면 `PATH`가 들어온다.
    - `login_env_times_out`: 끝나지 않는 셸은 timeout 뒤 `None`이다.
  - `tests/daemon.rs`:
    - `notify_on_approval_when_unfocused`: `MINE_NOTIFY`를 받은 인자를 파일에 덧붙이는 가짜 스크립트로 둔다. 세션을 만들고 `_event`로 승인 훅을 보내면 알림이 한 번 불리고, 인자에 세션 이름이 들어 있다.
    - `no_notify_when_a_client_is_focused`: 클라이언트가 `Focus{focused:true}`를 보낸 뒤에는 불리지 않는다.
    - `spawned_daemon_uses_given_env`: `connect_or_spawn_async_with(state, CARGO_BIN_EXE_mine, Some(PATH=<임시 bin>…))`로 데몬을 띄운다. 그 데몬이 만든 shell 세션의 `env` 덤프에 그 PATH가 있다. 가짜 셸 방식은 기존 env 테스트를 따른다.
- [ ] **Step 2: 실패를 확인한다.** Run: `cargo test --workspace notify login_env parse_env0 spawned_daemon` → FAIL
- [ ] **Step 3: 구현한다.**
  - **Focus:** 데몬은 클라이언트마다 `focused`를 저장한다. 연결이 끊기면 그 클라이언트의 값은 없어진다.
  - **알림:** 세션이 승인 대기로 **새로 바뀔 때**, 앞에 있는(focused) 클라이언트가 하나도 없으면 알림을 띄운다.
    - 알림 명령은 `MINE_NOTIFY`이고, 기본값은 `osascript`다.
    - 인자는 `-e 'display notification "<대기 문구 또는 승인 대기>" with title "mine" subtitle "<프로젝트 폴더>/<세션>"'`이다. 따옴표와 역슬래시는 이스케이프한다.
    - 전역 잠금 밖에서 띄우고, 기다리지 않는다. 자식은 스레드가 거둔다.
    - 같은 전환에 두 번 띄우지 않는다.
  - **앱 쪽 Focus:** 앱 창이 활성화되거나 비활성화되면 `Focus`를 보낸다.
  - **앱의 데몬 띄우기:** 앱은 시작할 때 `login_env($SHELL 또는 /bin/zsh, 3초)`를 한 번 읽어 `link::start`의 `spawn_env`로 넘긴다. 실패하면 `None`, 곧 앱 자신의 환경을 쓴다. `find_mine`의 PATH도 이 환경의 PATH를 먼저 쓴다.
  - **띄워진 데몬의 환경:** `env_clear` 뒤 주어진 환경에 `MINE_STATE_DIR`만 더한다. 보관 프로세스의 런타임 변수 정리 규칙은 그대로 적용된다.
  - **TUI:** 계속 `None`을 쓴다.
  - **버전 불일치:** 오류 문구는 기존 `restart_hint`를 쓴다.
- [ ] **Step 4: 통과를 확인한다.** Run: 검사 명령 → PASS. 데몬 테스트 묶음을 3번 연속 돌려 흔들리지 않는지 본다. `pgrep -fl "mine (daemon|hold)"`로 테스트가 남긴 프로세스가 없는지 확인한다. 사용자 데몬은 예외로 둔다.
- [ ] **Step 5: 커밋한다.** `feat(daemon): 앱 포커스를 받아 승인 요청을 macOS 알림으로 띄운다`

---

### Task 8: .app 묶음과 `mine app`

**Files:**
- Create: `scripts/bundle-app.sh`
- Modify: `crates/mine-cli/src/main.rs`(`App` 하위 명령), `README.md`(데스크톱 앱 절)

**Interfaces:**
- **Produces:**
  ```rust
  pub enum AppLaunch { Open(PathBuf) /* .app */, Exec(PathBuf) /* mine-app 바이너리 */, Missing }
  pub fn app_launch(bundles: &[PathBuf], exe_dir: Option<&Path>, path_lookup: Option<PathBuf>) -> AppLaunch
  ```

- [ ] **Step 1: 실패하는 테스트를 쓴다.** `app_launch_prefers_bundle_then_sibling_then_path`:
  - 있는 첫 번째 `.app`이면 `Open`이다.
  - 없으면 현재 실행 파일 옆의 `mine-app`이면 `Exec`이다.
  - 그것도 없으면 PATH의 `mine-app`이면 `Exec`이다.
  - 다 없으면 `Missing`이다.
- [ ] **Step 2: 실패를 확인한다.** Run: `cargo test -p mine-cli app_launch` → FAIL
- [ ] **Step 3: 구현한다.**
  - **`scripts/bundle-app.sh [--dest DIR]`**(기본 `$HOME/Applications`):
    - `cargo build --release -p mine-app -p mine-cli`를 하고 `mine.app/Contents/MacOS/`에 `mine-app`과 `mine`을 둘 다 넣는다. 그래서 묶음 앱은 옆의 `mine`으로 데몬을 띄운다(`find_mine`의 두 번째 순서).
    - `Info.plist`에는 `CFBundleName=mine`, `CFBundleIdentifier=dev.mine.app`, `CFBundleExecutable=mine-app`, `CFBundlePackageType=APPL`, `CFBundleShortVersionString=<패키지 판>`, `LSMinimumSystemVersion=13.0`, `NSHighResolutionCapable=true`를 넣는다.
    - 아이콘과 서명은 하지 않는다(`ponytail:` 8단계).
    - `set -euo pipefail`을 쓴다.
  - **`mine app`:**
    - 찾는 묶음 경로는 `~/Applications/mine.app`, `/Applications/mine.app`이다.
    - `Open`이면 `open <path>`를 실행한다.
    - `Exec`이면 그 바이너리를 떼어서(setsid) 띄우고 바로 돌아온다.
    - `Missing`이면 `mine-app이 없다. scripts/bundle-app.sh로 만들거나 cargo install --path crates/mine-app을 실행한다`를 띄우고 0이 아닌 코드로 끝난다.
  - **README:** "데스크톱 앱" 절(만들기, 실행, 단축키 표)을 더한다.
- [ ] **Step 4: 확인한다.**
  - Run: 검사 명령 → PASS.
  - `scripts/bundle-app.sh --dest <임시 폴더>`로 만든 뒤 묶음 구조와 `plutil -lint`를 확인한다.
  - `open -n <임시>/mine.app`을 임시 `MINE_STATE_DIR` 환경으로 띄우는 것은 어려우므로, 묶음 안 바이너리를 그 환경으로 직접 실행해 창이 뜨는지 캡처한다.
  - **사용자 홈의 `~/Applications`에는 설치하지 않는다.** 설치는 컨트롤러가 사용자에게 묻고 한다.
- [ ] **Step 5: 커밋한다.** `feat(app): mine.app 묶음 스크립트와 mine app 명령을 추가한다`
