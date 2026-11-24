//! 클라이언트 화면. 아이콘 줄·목록·터미널 칸·상태줄의 배치와 그리기, 마우스 위치 해석만 맡는다.

use std::path::{Path, PathBuf};

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line as GridLine};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::{Cell as TermCell, Flags};
use alacritty_terminal::vte::ansi::{Color as TermColor, NamedColor};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::client::launcher;
use mine_core::accounts::Labels;
use mine_core::launch::Choice;
use mine_core::screen::Screen;
use mine_core::session::{SessionInfo, SessionState};
use mine_core::view::{self, Group, NEW_USAGE, PROJECT_COLORS, RailItem};

/// 별칭 설정(`config.toml` 내용)으로 만든 이름들. 화면 문자열 테스트가 쓴다. 홈이 없으니 자동 이름은 codex뿐이다.
#[cfg(test)]
pub(super) fn labels_from(config: &str) -> Labels {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), config).unwrap();
    let state = mine_core::paths::State {
        root: dir.path().to_path_buf(),
    };
    Labels::load(&state, None)
}

/// 화면에 그릴 클라이언트 상태.
pub struct Model {
    pub sessions: Vec<SessionInfo>,
    /// 선택한 프로젝트. `None`이면 전체(∗)다.
    pub filter: Option<PathBuf>,
    /// `mine`을 실행한 프로젝트. 세션이 없어도 아이콘 줄에 있다.
    pub current_project: PathBuf,
    /// 붙은 세션 id.
    pub attached: Option<String>,
    pub list_open: bool,
    pub prompt: Option<Prompt>,
    /// 상태줄 오른쪽에 키 안내 대신 띄울 알림. 비어 있으면 키 안내다.
    pub message: String,
    /// 사용자가 머리를 눌러 기본(대기만 접힘)과 반대로 접거나 편 묶음의 상태.
    pub toggled: Vec<SessionState>,
    /// 복사 모드처럼 상태줄 전체를 대신할 글. 있으면 세션 경로·키 안내·알림 대신 이것을 그리고, 세션 커서는 숨긴다.
    pub status: Option<String>,
    /// 시작 화면(`launcher`)을 터미널 칸에 그린다. 붙은 세션이 없을 때와 `⌃a c`로 연 동안 참이다.
    pub launcher: bool,
    /// 계정 id를 글로 보일 때 쓰는 이름. 클라이언트가 시작할 때 읽는다.
    pub labels: Labels,
}

impl Model {
    /// 새 세션을 만들 프로젝트. 선택한 프로젝트, 전체(∗)면 `mine`을 실행한 프로젝트다.
    pub fn project(&self) -> &Path {
        self.filter.as_deref().unwrap_or(&self.current_project)
    }

    /// 목록에 그리는 묶음. 기본 접힘에 `toggled`를 얹고, 붙은 세션의 묶음은 늘 펼친다.
    /// 목록 그리기·`hit`·`⌃a n/p`가 모두 이 묶음을 쓴다.
    pub fn visible_groups(&self) -> Vec<Group> {
        let attached = self
            .attached
            .as_ref()
            .and_then(|id| self.sessions.iter().position(|s| s.meta.id == *id));
        view::expand(
            view::groups(&self.sessions, self.filter.as_deref()),
            &self.toggled,
            attached,
        )
    }

    /// `visible_groups`의 `i`번째 묶음을 접거나 편다(`Hit::Group`).
    pub fn toggle_group(&mut self, i: usize) {
        let Some(state) = self.visible_groups().into_iter().nth(i).map(|g| g.state) else {
            return;
        };
        match self.toggled.iter().position(|s| *s == state) {
            Some(at) => {
                self.toggled.remove(at);
            }
            None => self.toggled.push(state),
        }
    }
}

/// 상태줄에서 받는 입력.
#[derive(Clone, Debug, PartialEq)]
pub enum Prompt {
    /// 시작 화면의 `n`: 새 세션 명세를 입력 중이다.
    New(String),
    /// `⌃a x`: 이 세션 id를 닫을지 묻는다.
    ConfirmKill(String),
}

/// 화면 칸 배치.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Areas {
    pub rail: Rect,
    pub list: Option<Rect>,
    pub term: Rect,
    pub status: Rect,
}

/// 마우스 위치가 가리키는 것. `Rail`은 아이콘 줄 항목, `Group`은 묶음 머리(`Model::visible_groups` 인덱스),
/// `Row`는 `Model::sessions` 인덱스, `Term`은 터미널 칸 안 좌표(0부터)다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Rail(usize),
    Group(usize),
    Row(usize),
    /// 시작 화면의 선택 줄.
    Launch(Choice),
    Term(u16, u16),
    None,
}

/// 세션 화면에서 하이퍼링크가 걸린 연속 칸. `x`, `y`는 바깥 화면 좌표다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkRun {
    pub x: u16,
    pub y: u16,
    pub url: String,
    pub text: String,
}

const RAIL_W: u16 = 4;
const LIST_W: u16 = 30;
/// 목록 줄 오른쪽 끝의 빈 칸. 칩이 터미널 칸에 붙지 않게 띄운다.
const GUTTER: usize = 1;
const KEYS: &str = "⌃a n/p sessions · ( ) projects · c new · x close · b list · d detach";

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// 아이콘과 칩의 글자색.
const DARK: Color = rgb(0x1e2128);
const ALL_BG: Color = rgb(0x3a3d45);
const ALL_FG: Color = rgb(0xd0d3da);
const MARK: Color = rgb(0xe6e6e6);
const YELLOW: Color = rgb(0xe5c07b);
const PURPLE: Color = rgb(0xc678dd);
const CYAN: Color = rgb(0x56b6c2);
const GREEN: Color = rgb(0x98c379);
pub(super) const MUTED: Color = rgb(0x6b707a);
pub(super) const HEAD: Color = rgb(0x8a8f99);
const COUNT: Color = rgb(0x5f646e);
const SELECTED_BG: Color = rgb(0x2f323a);
const WAITING: Color = rgb(0xc9a85c);
/// 패널 바탕(목업 `project-rail.html`). 상태줄은 아이콘 줄과 같은 바탕이다.
const RAIL_BG: Color = rgb(0x1b1d23);
const LIST_BG: Color = rgb(0x23252b);
/// 아이콘 줄·목록의 오른쪽 끝 열 바탕. 패널과 옆 칸을 가른다.
const BORDER: Color = rgb(0x2c2f36);
const LIST_FG: Color = rgb(0xd0d3da);

/// 아래 한 줄은 상태줄, 왼쪽부터 아이콘 줄 4칸, (펼쳤으면) 목록 30칸, 나머지가 터미널 칸이다.
pub fn layout(area: Rect, list_open: bool) -> Areas {
    let body = area.height.saturating_sub(1);
    let rail_w = RAIL_W.min(area.width);
    let list_w = if list_open {
        LIST_W.min(area.width - rail_w)
    } else {
        0
    };
    let term_x = area.x + rail_w + list_w;
    Areas {
        rail: Rect::new(area.x, area.y, rail_w, body),
        list: list_open.then(|| Rect::new(area.x + rail_w, area.y, list_w, body)),
        term: Rect::new(term_x, area.y, area.width - rail_w - list_w, body),
        status: Rect::new(area.x, area.y + body, area.width, area.height - body),
    }
}

pub fn draw(f: &mut Frame, m: &Model, screen: Option<&Screen>) {
    let a = layout(f.area(), m.list_open);
    paint_panels(f.buffer_mut(), &a);
    let rail = view::rail(&m.sessions, &m.current_project);
    let selected = rail
        .iter()
        .position(|item| item.project == m.filter)
        .unwrap_or(0);
    draw_rail(f.buffer_mut(), a.rail, &rail, selected);
    if let Some(list) = a.list {
        let width = usize::from(list.width);
        let lines: Vec<Line> = items(m)
            .iter()
            .map(|item| item_line(m, &rail, item, width))
            .collect();
        f.render_widget(Paragraph::new(lines), list);
    }
    paint_edges(f.buffer_mut(), &a);
    if m.launcher {
        launcher::draw(f.buffer_mut(), a.term, m.project(), &m.labels);
    } else if let Some(sc) = screen {
        draw_term(f.buffer_mut(), a.term, sc);
        if m.attached.is_some()
            && m.status.is_none()
            && let Some(pos) = term_cursor(sc, a.term)
        {
            f.set_cursor_position(pos);
        }
    }
    // 입력 중이면 상태줄이 커서를 가져간다.
    draw_status(f, a.status, m);
}

pub fn hit(a: &Areas, m: &Model, col: u16, row: u16) -> Hit {
    let pos = Position::new(col, row);
    if a.rail.contains(pos) {
        // 줄 0은 여백이고 항목 i는 줄 i+1이다.
        let n = view::rail(&m.sessions, &m.current_project).len();
        return match usize::from(row - a.rail.y).checked_sub(1) {
            Some(i) if i < n => Hit::Rail(i),
            _ => Hit::None,
        };
    }
    if let Some(list) = a.list
        && list.contains(pos)
    {
        return match items(m).get(usize::from(row - list.y)) {
            Some(Item::Head(i, _)) => Hit::Group(*i),
            Some(item) => item.session().map_or(Hit::None, Hit::Row),
            None => Hit::None,
        };
    }
    if a.term.contains(pos) {
        // 시작 화면이 세션 칸을 덮는 동안은 선택 줄만 누를 수 있다.
        if m.launcher {
            return launcher::hit(a.term, col, row, &m.labels).map_or(Hit::None, Hit::Launch);
        }
        return Hit::Term(col - a.term.x, row - a.term.y);
    }
    Hit::None
}

/// 아이콘 줄·목록·상태줄 바탕을 칠한다. 글자를 그리기 전에 부른다.
/// 글자가 없는 칸까지 칠해 패널로 읽히고, 투명한 창에서도 비치지 않는다.
fn paint_panels(buf: &mut Buffer, a: &Areas) {
    buf.set_style(a.rail, Style::new().fg(LIST_FG).bg(RAIL_BG));
    if let Some(list) = a.list {
        buf.set_style(list, Style::new().fg(LIST_FG).bg(LIST_BG));
    }
    buf.set_style(a.status, Style::new().fg(LIST_FG).bg(RAIL_BG));
}

/// 아이콘 줄과 목록의 오른쪽 끝 열 바탕을 경계색으로 바꿔 옆 칸과 가른다.
/// 선택한 줄의 바탕까지 덮도록 글자를 그린 뒤에 부른다.
///
/// ponytail: 글자를 덮지 않게 바탕만 바꾼 한 칸짜리 띠다. 얇은 선 글자는 필요해지면 한다.
fn paint_edges(buf: &mut Buffer, a: &Areas) {
    for r in [Some(a.rail), a.list].into_iter().flatten() {
        let edge = Rect::new(r.right().saturating_sub(1), r.y, r.width.min(1), r.height);
        buf.set_style(edge, Style::new().bg(BORDER));
    }
}

/// 터미널 칸(`area`)에 보이는 세션 화면의 하이퍼링크 구간. 줄을 넘지 않고, 같은 링크가 이어지는 칸끼리 묶는다.
pub fn link_runs(sc: &Screen, area: Rect) -> Vec<LinkRun> {
    let mut runs: Vec<LinkRun> = Vec::new();
    let mut open = None;
    for (x, y, cell) in visible_cells(sc, area) {
        // 제어 문자가 든 주소는 바깥 터미널의 OSC 8을 깰 수 있으므로 링크로 치지 않는다.
        let link = cell
            .hyperlink()
            .filter(|l| !l.uri().chars().any(char::is_control));
        let same = link.is_some() && link == open && runs.last().is_some_and(|r| r.y == y);
        if same {
            if let Some(run) = runs.last_mut() {
                run.text.push_str(&symbol(cell));
            }
        } else if let Some(l) = &link {
            runs.push(LinkRun {
                x,
                y,
                url: l.uri().to_string(),
                text: symbol(cell),
            });
        }
        open = link;
    }
    runs
}

fn draw_rail(buf: &mut Buffer, area: Rect, items: &[RailItem], selected: usize) {
    if area.width < RAIL_W {
        return;
    }
    // 칸 0은 선택 표시, 칸 1~2는 아이콘(넓은 글자면 둘을 채운다), 칸 3은 승인 대기 배지다.
    for (i, (item, y)) in items.iter().zip(area.y + 1..area.bottom()).enumerate() {
        if i == selected {
            buf[(area.x, y)].set_symbol("▌").set_fg(MARK);
        }
        let style = match item.project {
            None => Style::new().fg(ALL_FG).bg(ALL_BG),
            Some(_) => Style::new().fg(DARK).bg(project_color(item.color)),
        }
        .add_modifier(Modifier::BOLD);
        buf.set_string(area.x + 1, y, "  ", style);
        buf.set_stringn(area.x + 1, y, item.initial.to_string(), 2, style);
        if item.approval {
            buf[(area.x + 3, y)].set_symbol("◆").set_fg(YELLOW);
        }
    }
}

fn project_color(i: usize) -> Color {
    PROJECT_COLORS
        .get(i)
        .and_then(|hex| hex.parse().ok())
        .unwrap_or(Color::Reset)
}

/// 목록의 한 줄.
enum Item {
    Title,
    /// `visible_groups` 인덱스와 그 묶음.
    Head(usize, Group),
    Session(usize),
    /// 승인 대기 세션 아래의 대기 문구.
    Waiting(usize),
    /// 이 프로젝트에 세션이 하나도 없다.
    Empty,
    NewHint,
}

impl Item {
    /// 이 줄을 누르면 붙을 세션.
    fn session(&self) -> Option<usize> {
        match self {
            Item::Session(i) | Item::Waiting(i) => Some(*i),
            _ => None,
        }
    }
}

/// 목록 줄 순서. 맨 위 프로젝트 이름, 상태 묶음마다 머리와 (펼쳤으면) 세션 줄(없으면 `(세션 없음)`), 맨 아래 새 세션 안내.
fn items(m: &Model) -> Vec<Item> {
    let mut out = vec![Item::Title];
    let groups = m.visible_groups();
    if groups.is_empty() {
        out.push(Item::Empty);
    }
    for (i, g) in groups.into_iter().enumerate() {
        let rows = if g.collapsed {
            Vec::new()
        } else {
            g.rows.clone()
        };
        out.push(Item::Head(i, g));
        for i in rows {
            out.push(Item::Session(i));
            let s = &m.sessions[i];
            if s.state == SessionState::Approval && s.waiting.is_some() {
                out.push(Item::Waiting(i));
            }
        }
    }
    out.push(Item::NewHint);
    out
}

fn item_line(m: &Model, rail: &[RailItem], item: &Item, width: usize) -> Line<'static> {
    match item {
        Item::Title => {
            let title = match &m.filter {
                Some(p) => view::folder_name(p),
                None => "All projects".to_string(),
            };
            Line::styled(title, Style::new().fg(MARK).add_modifier(Modifier::BOLD))
        }
        Item::Head(_, g) => {
            let (glyph, color, label) = match g.state {
                SessionState::Approval => ("◆", YELLOW, "Needs approval"),
                SessionState::Working => ("◐", CYAN, "Working"),
                SessionState::Done | SessionState::Ended => ("✓", GREEN, "Done"),
                SessionState::Idle => ("○", MUTED, "Idle"),
            };
            let arrow = if g.collapsed { "▸" } else { "▾" };
            let left = vec![
                Span::styled(format!("{arrow} "), HEAD),
                Span::styled(glyph, color),
                Span::styled(format!(" {label}"), HEAD),
            ];
            fill(
                left,
                Some(Span::styled(g.rows.len().to_string(), COUNT)),
                width,
            )
        }
        Item::Session(i) => session_line(m, rail, *i, width),
        Item::Waiting(i) => {
            let waiting = m.sessions[*i].waiting.as_deref().unwrap_or_default();
            Line::styled(format!("    {waiting}"), WAITING)
        }
        Item::Empty => Line::styled("  (no sessions)", MUTED),
        Item::NewHint => Line::styled("＋ New session  ⌃a c", MUTED),
    }
}

/// 세션 줄. 전체 보기면 앞에 프로젝트 색 `■`, 끝난 세션이면 이름 뒤에 `종료됨`, 끝에 계정 칩.
fn session_line(m: &Model, rail: &[RailItem], i: usize, width: usize) -> Line<'static> {
    let s = &m.sessions[i];
    let mut left = vec![Span::raw("  ")];
    if m.filter.is_none() {
        let color = rail
            .iter()
            .find(|r| r.project.as_ref() == Some(&s.meta.project))
            .map_or(Color::Reset, |r| project_color(r.color));
        left.push(Span::styled("■ ", color));
    }
    let ended = (s.state == SessionState::Ended).then(|| Span::styled(" Exited", MUTED));
    let chip = chip(s.meta.account.as_deref());
    let fixed = spans_width(&left)
        + ended.as_ref().map_or(0, Span::width)
        + chip.as_ref().map_or(0, Span::width)
        + GUTTER;
    // 세션 이름은 ASCII라 글자 수가 곧 너비다.
    let name: String = s
        .meta
        .name
        .chars()
        .take(width.saturating_sub(fixed))
        .collect();
    let selected = m.attached.as_ref() == Some(&s.meta.id);
    let name_style = if selected {
        Style::new().add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    };
    left.push(Span::styled(name, name_style));
    left.extend(ended);
    let line = fill(left, chip, width);
    if selected {
        line.style(Style::new().bg(SELECTED_BG))
    } else {
        line
    }
}

/// 계정 칩. claude-a `A` 노랑, claude-b `B` 보라, codex `C` 청록. 그 밖(shell)은 없다.
pub(super) fn chip(account: Option<&str>) -> Option<Span<'static>> {
    let (letter, bg) = match account? {
        "claude-a" => ("A", YELLOW),
        "claude-b" => ("B", PURPLE),
        "codex" => ("C", CYAN),
        _ => return None,
    };
    let style = Style::new().fg(DARK).bg(bg).add_modifier(Modifier::BOLD);
    Some(Span::styled(letter, style))
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(Span::width).sum()
}

/// `left` 뒤를 공백으로 채워 `right`를 오른쪽 끝(`GUTTER` 앞)에 둔다. 선택 줄 바탕이 끝까지 가도록 `GUTTER`도 채운다.
fn fill(mut left: Vec<Span<'static>>, right: Option<Span<'static>>, width: usize) -> Line<'static> {
    let used = spans_width(&left) + right.as_ref().map_or(0, Span::width) + GUTTER;
    left.push(Span::raw(" ".repeat(width.saturating_sub(used))));
    left.extend(right);
    left.push(Span::raw(" ".repeat(GUTTER)));
    Line::from(left)
}

/// 터미널 칸에 보이는 세션 칸과 그 바깥 화면 좌표. 넓은 글자의 뒤 칸(앞 글자가 덮는다)과
/// 칸 끝에 걸려 반만 보일 넓은 글자는 뺀다.
fn visible_cells(sc: &Screen, area: Rect) -> impl Iterator<Item = (u16, u16, &TermCell)> {
    let grid = sc.term().grid();
    let offset = grid.display_offset() as i32;
    let cols = grid.columns().min(usize::from(area.width));
    let rows = grid.screen_lines().min(usize::from(area.height));
    (0..rows).flat_map(move |y| {
        let row = &grid[GridLine(y as i32 - offset)];
        (0..cols).filter_map(move |x| {
            let cell = &row[Column(x)];
            let hidden = cell.flags.contains(Flags::WIDE_CHAR_SPACER)
                || (cell.flags.contains(Flags::WIDE_CHAR) && x + 1 == cols);
            (!hidden).then_some((area.x + x as u16, area.y + y as u16, cell))
        })
    })
}

fn draw_term(buf: &mut Buffer, area: Rect, sc: &Screen) {
    for (x, y, cell) in visible_cells(sc, area) {
        buf[(x, y)].set_symbol(&symbol(cell)).set_style(style(cell));
    }
}

/// 칸의 글자와 그 위에 얹힌 결합 문자. 탭 같은 제어 문자 칸은 공백이다.
fn symbol(cell: &TermCell) -> String {
    let mut s = String::from(if cell.c.is_control() { ' ' } else { cell.c });
    s.extend(cell.zerowidth().unwrap_or_default());
    s
}

const MODIFIERS: [(Flags, Modifier); 6] = [
    (Flags::BOLD, Modifier::BOLD),
    (Flags::DIM, Modifier::DIM),
    (Flags::ITALIC, Modifier::ITALIC),
    (Flags::INVERSE, Modifier::REVERSED),
    (Flags::HIDDEN, Modifier::HIDDEN),
    (Flags::STRIKEOUT, Modifier::CROSSED_OUT),
];

fn style(cell: &TermCell) -> Style {
    let mut modifier = Modifier::empty();
    for (flag, m) in MODIFIERS {
        if cell.flags.contains(flag) {
            modifier |= m;
        }
    }
    if cell.flags.intersects(Flags::ALL_UNDERLINES) {
        modifier |= Modifier::UNDERLINED;
    }
    let style = Style::new()
        .fg(color(cell.fg))
        .bg(color(cell.bg))
        .add_modifier(modifier);
    match cell.underline_color() {
        Some(c) => style.underline_color(color(c)),
        None => style,
    }
}

/// 0~15번 색. 바깥 터미널의 색표를 그대로 쓰도록 이름 있는 색으로 보낸다.
const ANSI: [Color; 16] = [
    Color::Black,
    Color::Red,
    Color::Green,
    Color::Yellow,
    Color::Blue,
    Color::Magenta,
    Color::Cyan,
    Color::Gray,
    Color::DarkGray,
    Color::LightRed,
    Color::LightGreen,
    Color::LightYellow,
    Color::LightBlue,
    Color::LightMagenta,
    Color::LightCyan,
    Color::White,
];

/// alacritty 색을 ratatui 색으로. 기본 전경·배경색은 바깥 터미널의 기본색(`Reset`)이다.
fn color(c: TermColor) -> Color {
    use NamedColor::*;
    let i = match c {
        TermColor::Spec(c) => return Color::Rgb(c.r, c.g, c.b),
        TermColor::Indexed(i) => return Color::Indexed(i),
        TermColor::Named(Foreground | Background | Cursor | BrightForeground | DimForeground) => {
            return Color::Reset;
        }
        TermColor::Named(Black | DimBlack) => 0,
        TermColor::Named(Red | DimRed) => 1,
        TermColor::Named(Green | DimGreen) => 2,
        TermColor::Named(Yellow | DimYellow) => 3,
        TermColor::Named(Blue | DimBlue) => 4,
        TermColor::Named(Magenta | DimMagenta) => 5,
        TermColor::Named(Cyan | DimCyan) => 6,
        TermColor::Named(White | DimWhite) => 7,
        TermColor::Named(BrightBlack) => 8,
        TermColor::Named(BrightRed) => 9,
        TermColor::Named(BrightGreen) => 10,
        TermColor::Named(BrightYellow) => 11,
        TermColor::Named(BrightBlue) => 12,
        TermColor::Named(BrightMagenta) => 13,
        TermColor::Named(BrightCyan) => 14,
        TermColor::Named(BrightWhite) => 15,
    };
    ANSI[i]
}

/// 세션 커서 위치. 스크롤해 과거를 보고 있거나 앱이 커서를 숨겼으면 없다.
fn term_cursor(sc: &Screen, area: Rect) -> Option<Position> {
    if sc.term().grid().display_offset() != 0 || !sc.mode().contains(TermMode::SHOW_CURSOR) {
        return None;
    }
    let (x, y) = sc.cursor();
    (x < area.width && y < area.height).then(|| Position::new(area.x + x, area.y + y))
}

/// `mine │ <위치> │ <계정>`과 오른쪽 키 안내(또는 알림). 입력 중이면 그 입력과 안내를 보여 준다.
fn draw_status(f: &mut Frame, area: Rect, m: &Model) {
    if let Some(text) = &m.status {
        f.render_widget(Paragraph::new(text.as_str()), area);
        return;
    }
    let sep = || Span::styled(" │ ", MUTED);
    let mut left = vec![
        Span::styled("mine", Style::new().add_modifier(Modifier::BOLD)),
        sep(),
    ];
    let mut cursor = None;
    let right = match &m.prompt {
        Some(Prompt::New(input)) => {
            left.push(Span::raw(format!("New session: {input}")));
            cursor = Some(spans_width(&left));
            Span::styled(format!("{NEW_USAGE} · Enter create · Esc cancel"), MUTED)
        }
        Some(Prompt::ConfirmKill(id)) => {
            let name = m
                .sessions
                .iter()
                .find(|s| s.meta.id == *id)
                .map_or(id.as_str(), |s| s.meta.name.as_str());
            left.push(Span::styled(format!("Close session {name}?"), YELLOW));
            Span::styled("y close · any other key cancels", MUTED)
        }
        None => {
            match m
                .attached
                .as_ref()
                .and_then(|id| m.sessions.iter().find(|s| s.meta.id == *id))
            {
                Some(s) => {
                    let path = format!("{}/{}", s.meta.project.display(), s.meta.name);
                    left.push(Span::raw(path));
                    if let Some(account) = &s.meta.account {
                        left.push(sep());
                        left.push(Span::raw(m.labels.get(account).to_string()));
                    }
                }
                // 붙은 세션이 없으면 선택한 프로젝트의 전체 경로를 보여 준다(아이콘 이름 띄우기 대신).
                None => left.push(Span::raw(
                    m.filter
                        .as_ref()
                        .map_or_else(|| "All projects".to_string(), |p| p.display().to_string()),
                )),
            }
            if m.message.is_empty() {
                let keys = if m.launcher {
                    launcher::keys(&m.labels)
                } else {
                    KEYS.to_string()
                };
                Span::styled(keys, MUTED)
            } else {
                Span::styled(m.message.clone(), YELLOW)
            }
        }
    };
    let left_w = spans_width(&left) as u16;
    f.render_widget(Paragraph::new(Line::from(left)), area);
    // 오른쪽 끝에 붙이되, 왼쪽과 겹치면 왼쪽 뒤에서 시작해 잘리게 둔다.
    let right_w = right.width() as u16;
    let x = area
        .right()
        .saturating_sub(right_w)
        .max(area.x + left_w + 2);
    if x < area.right() {
        f.render_widget(
            Paragraph::new(Line::from(right)),
            Rect::new(x, area.y, area.right() - x, area.height),
        );
    }
    if let Some(cx) = cursor
        && (cx as u16) < area.width
    {
        f.set_cursor_position((area.x + cx as u16, area.y));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mine_core::session::{Kind, Meta, Restore, SessionState::*};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::{Buffer, Cell};

    fn s(
        project: &str,
        name: &str,
        state: SessionState,
        account: &str,
        waiting: Option<&str>,
    ) -> SessionInfo {
        let kind = match account {
            "codex" => Kind::Codex,
            "" => Kind::Shell,
            _ => Kind::Claude,
        };
        SessionInfo {
            meta: Meta {
                id: format!("k-{name}"),
                project: PathBuf::from(project),
                name: name.into(),
                kind,
                account: (!account.is_empty()).then(|| account.to_string()),
                model: None,
                restore: Restore::None,
                created_ms: 0,
                resume_id: None,
                closed: false,
            },
            state,
            waiting: waiting.map(str::to_string),
            alive: true,
        }
    }

    fn model(sessions: Vec<SessionInfo>) -> Model {
        Model {
            sessions,
            filter: Some(PathBuf::from("/p/kyoto")),
            current_project: PathBuf::from("/p/kyoto"),
            attached: None,
            list_open: true,
            prompt: None,
            message: String::new(),
            toggled: Vec::new(),
            status: None,
            launcher: false,
            labels: Labels::default(),
        }
    }

    fn draw_buf(m: &Model, screen: Option<&Screen>, w: u16, h: u16) -> Buffer {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, m, screen)).unwrap();
        t.backend().buffer().clone()
    }

    fn render(m: &Model, w: u16, h: u16) -> Buffer {
        draw_buf(m, None, w, h)
    }

    fn render_with_screen(m: &Model, sc: &Screen, w: u16, h: u16) -> Buffer {
        draw_buf(m, Some(sc), w, h)
    }

    fn cell(buf: &Buffer, x: u16, y: u16) -> &Cell {
        &buf[(x, y)]
    }

    /// 한 줄의 글자. 넓은 글자가 가린 다음 칸은 건너뛰고, 오른쪽 공백은 뗀다.
    fn row_text(buf: &Buffer, y: u16) -> String {
        let mut out = String::new();
        let mut x = 0;
        while x < buf.area.width {
            let sym = buf[(x, y)].symbol();
            out.push_str(sym);
            x += Span::raw(sym).width().max(1) as u16;
        }
        out.trim_end().to_string()
    }

    fn text(buf: &Buffer) -> String {
        (0..buf.area.height)
            .map(|y| row_text(buf, y))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn row_of(buf: &Buffer, needle: &str) -> String {
        (0..buf.area.height)
            .map(|y| row_text(buf, y))
            .find(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("{needle}이 없다:\n{}", text(buf)))
    }

    fn kyoto() -> Model {
        model(vec![
            s(
                "/p/kyoto",
                "billing",
                Approval,
                "claude-b",
                Some("npm run deploy 승인?"),
            ),
            s("/p/kyoto", "ui", Working, "codex", None),
        ])
    }

    #[test]
    fn draws_rail_groups_and_chips() {
        let m = kyoto();
        let buf = render(&m, 100, 20);
        assert!(text(&buf).contains("◆ Needs approval"));
        assert!(text(&buf).contains("npm run deploy 승인?"));
        assert_eq!(cell(&buf, 1, 1).symbol(), "∗");
        assert!(row_of(&buf, "billing").ends_with("B"));
    }

    #[test]
    fn rail_marks_selection_badge_and_colors() {
        let buf = render(&kyoto(), 100, 20);
        // 줄 0은 여백, ∗는 줄 1, kyoto는 줄 2다. 선택한 kyoto 왼쪽에 표시, 승인 대기라 오른쪽에 배지.
        assert_eq!(cell(&buf, 0, 1).symbol(), " ");
        assert_eq!(cell(&buf, 0, 2).symbol(), "▌");
        assert_eq!(cell(&buf, 1, 2).symbol(), "k");
        assert_eq!(cell(&buf, 3, 2).symbol(), "◆");
        assert_eq!(cell(&buf, 3, 2).fg, Color::Rgb(0xe5, 0xc0, 0x7b));
        assert_eq!(cell(&buf, 1, 1).bg, Color::Rgb(0x3a, 0x3d, 0x45));
        assert_eq!(cell(&buf, 1, 2).fg, Color::Rgb(0x1e, 0x21, 0x28));
        // kyoto의 색은 PROJECT_COLORS[1]이다(view 테스트 참고). 아이콘 두 칸이 같은 바탕이다.
        assert_eq!(cell(&buf, 1, 2).bg, Color::Rgb(0x98, 0xc3, 0x79));
        assert_eq!(cell(&buf, 2, 2).bg, cell(&buf, 1, 2).bg);
    }

    #[test]
    fn list_shows_title_ended_chips_and_all_view_squares() {
        let mut m = model(vec![
            s("/p/kyoto", "gone", Ended, "claude-a", None),
            s("/p/kyoto", "sh", Idle, "", None),
            s("/p/mine", "core", Working, "codex", None),
        ]);
        let buf = render(&m, 100, 20);
        assert_eq!(row_text(&buf, 0).trim(), "kyoto");
        assert!(row_of(&buf, "gone").contains("gone Exited"));
        assert!(row_of(&buf, "gone").ends_with('A'));
        assert!(text(&buf).contains("▸ ○ Idle"));
        assert!(!text(&buf).contains("core"));
        m.filter = None;
        let buf = render(&m, 100, 20);
        assert_eq!(row_text(&buf, 0).trim(), "All projects");
        assert!(row_of(&buf, "core").contains("■ core"));
        assert!(row_of(&buf, "core").ends_with('C'));
    }

    #[test]
    fn collapsed_list_leaves_only_rail() {
        let a = layout(Rect::new(0, 0, 100, 20), false);
        assert!(a.list.is_none());
        assert_eq!(a.term.x, 4);
        let b = layout(Rect::new(0, 0, 100, 20), true);
        assert_eq!((b.rail.width, b.list.unwrap().width, b.term.x), (4, 30, 34));
        assert_eq!((b.status.y, b.status.height, b.term.height), (19, 1, 19));
    }

    #[test]
    fn hit_maps_rows_and_rail() {
        let m = kyoto();
        let a = layout(Rect::new(0, 0, 100, 20), true);
        assert_eq!(hit(&a, &m, 1, 2), Hit::Rail(1));
        assert_eq!(hit(&a, &m, 0, 1), Hit::Rail(0));
        assert_eq!(hit(&a, &m, 1, 0), Hit::None);
        assert_eq!(hit(&a, &m, 1, 3), Hit::None);
        let buf = render(&m, 100, 20);
        let y = |needle: &str| {
            (0..20)
                .find(|&y| row_text(&buf, y).contains(needle))
                .unwrap()
        };
        let list_x = a.list.unwrap().x + 3;
        assert_eq!(hit(&a, &m, list_x, y("billing")), Hit::Row(0));
        assert_eq!(hit(&a, &m, list_x, y("deploy")), Hit::Row(0));
        assert_eq!(hit(&a, &m, list_x, y("ui")), Hit::Row(1));
        assert_eq!(hit(&a, &m, list_x, y("Needs approval")), Hit::Group(0));
        assert_eq!(hit(&a, &m, list_x, y("Working")), Hit::Group(1));
        assert_eq!(hit(&a, &m, list_x, 0), Hit::None);
        assert_eq!(hit(&a, &m, a.term.x + 3, a.term.y + 2), Hit::Term(3, 2));
        assert_eq!(hit(&a, &m, 50, a.status.y), Hit::None);
    }

    fn y_of(buf: &Buffer, needle: &str) -> u16 {
        (0..buf.area.height)
            .find(|&y| row_text(buf, y).contains(needle))
            .unwrap_or_else(|| panic!("{needle}이 없다:\n{}", text(buf)))
    }

    #[test]
    fn header_click_toggles_collapsed_group() {
        let mut m = model(vec![
            s("/p/kyoto", "ui", Working, "codex", None),
            s("/p/kyoto", "shellx", Idle, "", None),
        ]);
        let a = layout(Rect::new(0, 0, 100, 20), true);
        let list_x = a.list.unwrap().x + 3;
        let buf = render(&m, 100, 20);
        assert!(!text(&buf).contains("shellx"));
        let head = y_of(&buf, "○ Idle");
        let Hit::Group(g) = hit(&a, &m, list_x, head) else {
            panic!("대기 묶음 머리가 아니다");
        };
        m.toggle_group(g);
        let buf = render(&m, 100, 20);
        assert!(row_text(&buf, head).contains("▾ ○ Idle"));
        assert_eq!(hit(&a, &m, list_x, y_of(&buf, "shellx")), Hit::Row(1));
        // 한 번 더 누르면 다시 접힌다.
        m.toggle_group(g);
        assert!(!text(&render(&m, 100, 20)).contains("shellx"));
    }

    #[test]
    fn attached_session_group_is_drawn_expanded() {
        let mut m = model(vec![s("/p/kyoto", "shellx", Idle, "", None)]);
        m.attached = Some("k-shellx".into());
        let a = layout(Rect::new(0, 0, 100, 20), true);
        let list_x = a.list.unwrap().x + 3;
        let buf = render(&m, 100, 20);
        assert!(text(&buf).contains("▾ ○ Idle"));
        assert_eq!(hit(&a, &m, list_x, y_of(&buf, "shellx")), Hit::Row(0));
        // 사용자가 접어도 붙은 세션의 묶음은 펼친 채다.
        m.toggle_group(0);
        assert!(text(&render(&m, 100, 20)).contains("▾ ○ Idle"));
    }

    #[test]
    fn term_render_handles_wide_chars() {
        let mut sc = Screen::new(20, 3);
        sc.feed("한글a".as_bytes());
        let buf = render_with_screen(&model(vec![]), &sc, 60, 5);
        let x = layout(Rect::new(0, 0, 60, 5), true).term.x;
        assert_eq!(cell(&buf, x, 0).symbol(), "한");
        assert_eq!(cell(&buf, x + 2, 0).symbol(), "글");
        assert_eq!(cell(&buf, x + 4, 0).symbol(), "a");
    }

    #[test]
    fn term_render_maps_colors_and_attributes() {
        let mut sc = Screen::new(20, 3);
        sc.feed(b"\x1b[1;31mR\x1b[0;38;5;200;48;2;1;2;3mI\x1b[0;3;4;7;2mX\x1b[0mn");
        let buf = render_with_screen(&model(vec![]), &sc, 60, 5);
        let x = layout(Rect::new(0, 0, 60, 5), true).term.x;
        let (r, i, a, n) = (
            cell(&buf, x, 0),
            cell(&buf, x + 1, 0),
            cell(&buf, x + 2, 0),
            cell(&buf, x + 3, 0),
        );
        assert_eq!((r.fg, r.modifier), (Color::Red, Modifier::BOLD));
        assert_eq!((i.fg, i.bg), (Color::Indexed(200), Color::Rgb(1, 2, 3)));
        let want = Modifier::ITALIC | Modifier::UNDERLINED | Modifier::REVERSED | Modifier::DIM;
        assert_eq!(a.modifier, want);
        assert_eq!(
            (n.fg, n.bg, n.modifier),
            (Color::Reset, Color::Reset, Modifier::empty())
        );
    }

    #[test]
    fn status_line_and_launcher_hint_show_account_labels() {
        let mut m = kyoto();
        m.labels = labels_from("[accounts.claude-b]\nlabel = \"회사\"\n");
        m.attached = Some("k-billing".into());
        let status = row_text(&render(&m, 140, 20), 19);
        assert!(
            status.starts_with("mine │ /p/kyoto/billing │ 회사"),
            "{status}"
        );
        assert!(!status.contains("claude-b"), "{status}");
        // codex 세션은 자동 이름이다.
        m.attached = Some("k-ui".into());
        let status = row_text(&render(&m, 140, 20), 19);
        assert!(status.starts_with("mine │ /p/kyoto/ui │ Codex"), "{status}");
        // 시작 화면의 줄과 키 안내도 같은 이름이다.
        m.launcher = true;
        let buf = render(&m, 140, 20);
        assert!(row_of(&buf, "↵").contains("↵  claude-a"));
        assert!(row_of(&buf, "b  회사").ends_with("B"));
        assert!(row_text(&buf, 19).ends_with("↵ claude-a · b 회사 · x Codex · s shell"));
    }

    #[test]
    fn status_line_shows_location_account_and_prompt() {
        let mut m = kyoto();
        m.attached = Some("k-billing".into());
        let buf = render(&m, 140, 20);
        let status = row_text(&buf, 19);
        assert!(
            status.starts_with("mine │ /p/kyoto/billing │ claude-b"),
            "{status}"
        );
        assert!(status.ends_with("d detach"), "{status}");
        m.prompt = Some(Prompt::New("shell de".into()));
        assert!(row_text(&render(&m, 140, 20), 19).contains("shell de"));
        m.prompt = Some(Prompt::ConfirmKill("k-billing".into()));
        assert!(row_text(&render(&m, 140, 20), 19).contains("billing"));
    }

    #[test]
    fn status_override_replaces_the_status_line_and_hides_the_session_cursor() {
        use ratatui::backend::Backend as _;
        let mut m = kyoto();
        m.attached = Some("k-billing".into());
        let mut sc = Screen::new(20, 3);
        sc.feed(b"ab");
        let drawn = |m: &Model| {
            let mut t = Terminal::new(TestBackend::new(140, 20)).unwrap();
            t.draw(|f| draw(f, m, Some(&sc))).unwrap();
            let pos = t.backend_mut().get_cursor_position().unwrap();
            (row_text(t.backend().buffer(), 19), pos)
        };
        let term_x = layout(Rect::new(0, 0, 140, 20), true).term.x;
        let (status, pos) = drawn(&m);
        assert!(status.starts_with("mine │ /p/kyoto/billing"), "{status}");
        assert_eq!(pos, Position::new(term_x + 2, 0));
        // 상태줄 전체가 그 글로 바뀌고, 세션 커서는 옮기지 않는다.
        m.status = Some("Copy mode │ 1/3 │ v select".into());
        let (status, pos) = drawn(&m);
        assert_eq!(status, "Copy mode │ 1/3 │ v select");
        assert_eq!(pos, Position::ORIGIN);
    }

    #[test]
    fn link_runs_find_hyperlinked_cells() {
        let mut sc = Screen::new(30, 3);
        sc.feed(b"see \x1b]8;;file:///a.rs\x1b\\a.rs\x1b]8;;\x1b\\ and\r\n");
        sc.feed("\x1b]8;;https://x.example\x1b\\한x\x1b]8;;\x1b\\ \x1b]8;;https://x.example\x1b\\y\x1b]8;;\x1b\\".as_bytes());
        let runs = link_runs(&sc, Rect::new(10, 2, 30, 3));
        assert_eq!(
            runs,
            vec![
                LinkRun {
                    x: 14,
                    y: 2,
                    url: "file:///a.rs".into(),
                    text: "a.rs".into()
                },
                LinkRun {
                    x: 10,
                    y: 3,
                    url: "https://x.example".into(),
                    text: "한x".into()
                },
                // 주소가 같아도 따로 연 링크는 다른 구간이다.
                LinkRun {
                    x: 14,
                    y: 3,
                    url: "https://x.example".into(),
                    text: "y".into()
                },
            ]
        );
        // 칸이 화면보다 좁으면 잘린 만큼만 돌려준다.
        let runs = link_runs(&sc, Rect::new(0, 0, 6, 3));
        assert_eq!(runs[0].text, "a.");
    }

    #[test]
    fn panels_are_filled_with_the_mockup_colors_even_far_below_the_content() {
        let mut m = kyoto();
        m.attached = Some("k-billing".into());
        let buf = render(&m, 100, 30);
        let bg = |x, y| cell(&buf, x, y).bg;
        let rail = Color::Rgb(0x1b, 0x1d, 0x23);
        let list = Color::Rgb(0x23, 0x25, 0x2b);
        let border = Color::Rgb(0x2c, 0x2f, 0x36);
        // 글자가 없는 아래쪽 칸까지 칠한다.
        for y in [0, 5, 28] {
            assert_eq!((bg(0, y), bg(2, y)), (rail, rail), "rail y={y}");
            assert_eq!((bg(4, y), bg(20, y)), (list, list), "list y={y}");
            // 아이콘 줄의 오른쪽 끝 열과 목록의 오른쪽 끝 열이 경계다.
            assert_eq!((bg(3, y), bg(33, y)), (border, border), "border y={y}");
        }
        // 터미널 칸은 칠하지 않는다. 상태줄은 아이콘 줄과 같은 바탕이다.
        assert_eq!(bg(34, 10), Color::Reset);
        assert_eq!((bg(0, 29), bg(35, 29), bg(60, 29)), (rail, rail, rail));
        // 선택한 줄은 목록 바탕 위에 선택색이고, 마지막 열은 그래도 경계다.
        assert_eq!(bg(10, 2), Color::Rgb(0x2f, 0x32, 0x3a));
        assert_eq!(bg(33, 2), border);
        // 글자색: 목록 글자, 제목(굵게), 묶음 머리, 대기 문구.
        let fg = |x, y| cell(&buf, x, y).fg;
        // 목록은 4열부터다: 빈 칸, 세션 이름(`billing`은 6열), 제목, 묶음 머리, 대기 문구.
        assert_eq!(fg(10, 8), Color::Rgb(0xd0, 0xd3, 0xda));
        assert_eq!(fg(6, 2), Color::Rgb(0xd0, 0xd3, 0xda));
        assert_eq!(fg(4, 0), Color::Rgb(0xe6, 0xe6, 0xe6));
        assert!(cell(&buf, 4, 0).modifier.contains(Modifier::BOLD));
        assert_eq!(fg(4, 1), Color::Rgb(0x8a, 0x8f, 0x99));
        assert_eq!(fg(8, 3), Color::Rgb(0xc9, 0xa8, 0x5c));
        // 상태줄 글자는 바탕 위에서 읽히는 색이다.
        assert_eq!(fg(0, 29), Color::Rgb(0xd0, 0xd3, 0xda));
    }

    #[test]
    fn list_boundary_follows_the_list_and_the_rail_keeps_its_own_when_collapsed() {
        let mut m = kyoto();
        m.list_open = false;
        let buf = render(&m, 100, 30);
        let border = Color::Rgb(0x2c, 0x2f, 0x36);
        assert_eq!(cell(&buf, 3, 12).bg, border);
        assert_eq!(cell(&buf, 4, 12).bg, Color::Reset);
        assert_eq!(cell(&buf, 33, 12).bg, Color::Reset);
    }

    #[test]
    fn empty_project_list_says_so_in_muted_text() {
        let buf = render(&model(vec![]), 100, 20);
        assert_eq!(row_text(&buf, 0).trim(), "kyoto");
        // 줄 앞쪽은 아이콘 줄이다. 목록은 4열부터고 세션 줄처럼 두 칸 들여 쓴다.
        assert!(row_text(&buf, 1).ends_with("  (no sessions)"));
        assert_eq!(cell(&buf, 6, 1).symbol(), "(");
        assert_eq!(cell(&buf, 6, 1).fg, Color::Rgb(0x6b, 0x70, 0x7a));
        assert!(text(&buf).contains("＋ New session  ⌃a c"));
        // 세션이 있으면 보이지 않는다. 다른 프로젝트 세션만 있으면 이 프로젝트는 빈 것이다.
        assert!(!text(&render(&kyoto(), 100, 20)).contains("(no sessions)"));
        let other = model(vec![s("/p/mine", "core", Working, "codex", None)]);
        assert!(text(&render(&other, 100, 20)).contains("(no sessions)"));
    }

    #[test]
    fn launcher_replaces_the_term_pane_and_the_status_hint() {
        let mut sc = Screen::new(20, 3);
        sc.feed(b"hello");
        let mut m = model(vec![]);
        m.launcher = true;
        let a = layout(Rect::new(0, 0, 100, 20), true);
        let buf = render_with_screen(&m, &sc, 100, 20);
        // 세션 화면 대신 시작 화면이 터미널 칸에 그려진다.
        assert!(!text(&buf).contains("hello"));
        assert!(row_of(&buf, "claude-a").contains("↵  claude-a"));
        assert!(row_text(&buf, 19).ends_with("↵ claude-a · b claude-b · x codex · s shell"));
        // 마우스: 선택 줄은 `Launch`, 칸의 나머지는 세션에 넘기지 않는다.
        let y = y_of(&buf, "claude-b");
        let x = (0..100)
            .find(|&x| cell(&buf, x, y).symbol() == "b")
            .unwrap();
        assert_eq!(hit(&a, &m, x, y), Hit::Launch(Choice::ClaudeB));
        assert_eq!(hit(&a, &m, a.term.x, a.term.y), Hit::None);
        // 끄면 세션 화면과 평소 안내로 돌아온다.
        m.launcher = false;
        m.attached = Some("k-x".into());
        let buf = render_with_screen(&m, &sc, 100, 20);
        assert!(text(&buf).contains("hello") && !text(&buf).contains("claude-a"));
        assert!(row_text(&buf, 19).ends_with("d detach"));
        assert_eq!(hit(&a, &m, a.term.x, a.term.y), Hit::Term(0, 0));
    }
}
