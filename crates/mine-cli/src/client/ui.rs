//! The client screen. Handles only the layout and drawing of the project rail, list, terminal pane and status line, and interpreting mouse positions.

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
use mine_core::launch::{self, Choice};
use mine_core::profiles::ProfileInfo;
use mine_core::screen::Screen;
use mine_core::session::{SessionInfo, SessionState};
use mine_core::view::{self, Group, NEW_USAGE, PROJECT_COLORS, RailItem};

/// A logged-in profile as the daemon sends it. The chip is the label's first letter, uppercased.
#[cfg(test)]
pub(super) fn profile(
    id: &str,
    kind: mine_core::profiles::ProfileKind,
    label: &str,
    color: &str,
) -> ProfileInfo {
    ProfileInfo {
        id: id.into(),
        kind,
        label: label.into(),
        color: color.into(),
        chip: label.chars().next().unwrap().to_uppercase().to_string(),
        default: false,
        logged_in: true,
    }
}

/// The built-in three, logged in: claude-a `personal` (the default Claude profile), claude-b `work`, and codex `Codex`.
#[cfg(test)]
pub(super) fn builtin() -> Vec<ProfileInfo> {
    use mine_core::profiles::ProfileKind::{Claude, Codex};
    let mut list = vec![
        profile("claude-a", Claude, "personal", "#e5c07b"),
        profile("claude-b", Claude, "work", "#c678dd"),
        profile("codex", Codex, "Codex", "#56b6c2"),
    ];
    list[0].default = true;
    list[2].default = true;
    list
}

/// Client state to draw on screen.
pub struct Model {
    pub sessions: Vec<SessionInfo>,
    /// The selected project. `None` means all (∗).
    pub filter: Option<PathBuf>,
    /// The project `mine` was run in. It's in the project rail even if it has no sessions.
    pub current_project: PathBuf,
    /// The attached session id.
    pub attached: Option<String>,
    pub list_open: bool,
    pub prompt: Option<Prompt>,
    /// A notice to show on the right of the status line in place of the key hints. If empty, the key hints show.
    pub message: String,
    /// State of groups the user collapsed or expanded by clicking the header, opposite to the default (only idle is collapsed).
    pub toggled: Vec<SessionState>,
    /// Text to replace the whole status line, like copy mode. If set, it's drawn instead of the session path, key hints and notice, and the session cursor is hidden.
    pub status: Option<String>,
    /// Draws the launcher (`launcher`) in the terminal pane. True when no session is attached and while it's open via `⌃a c`.
    pub launcher: bool,
    /// The profiles the daemon sent last (`FromDaemon::Profiles`), in launcher order. Empty until the first list arrives.
    pub profiles: Vec<ProfileInfo>,
}

impl Model {
    /// The project to create new sessions in. The selected project; if All (∗), the project `mine` was run in.
    pub fn project(&self) -> &Path {
        self.filter.as_deref().unwrap_or(&self.current_project)
    }

    /// The groups drawn in the list. `toggled` applied on top of the default collapsing, and the attached session's group is always expanded.
    /// List drawing, `hit` and `⌃a n/p` all use these groups.
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

    /// Collapses or expands the `i`-th group of `visible_groups` (`Hit::Group`).
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

/// Input taken in the status line.
#[derive(Clone, Debug, PartialEq)]
pub enum Prompt {
    /// `n` in the launcher: a new session spec is being typed.
    New(String),
    /// `⌃a x`: asks whether to close this session id.
    ConfirmKill(String),
}

/// Placement of the screen areas.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Areas {
    pub rail: Rect,
    pub list: Option<Rect>,
    pub term: Rect,
    pub status: Rect,
}

/// What a mouse position points at. `Rail` is a project rail item, `Group` is a group header (index into `Model::visible_groups`),
/// `Row` is an index into `Model::sessions`, and `Term` is coordinates inside the terminal pane (0-based).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hit {
    Rail(usize),
    Group(usize),
    Row(usize),
    /// A chosen row in the launcher.
    Launch(Choice),
    Term(u16, u16),
    None,
}

/// A run of cells in the session screen with a hyperlink. `x` and `y` are outer screen coordinates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkRun {
    pub x: u16,
    pub y: u16,
    pub url: String,
    pub text: String,
}

const RAIL_W: u16 = 4;
const LIST_W: u16 = 30;
/// Blank space at the right end of a list row. Keeps the chip from touching the terminal pane.
const GUTTER: usize = 1;
const KEYS: &str = "⌃a n/p sessions · ( ) projects · c new · x close · b list · d detach";

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// Text color for icons and chips.
const DARK: Color = rgb(0x1e2128);
const ALL_BG: Color = rgb(0x3a3d45);
const ALL_FG: Color = rgb(0xd0d3da);
const MARK: Color = rgb(0xe6e6e6);
const YELLOW: Color = rgb(0xe5c07b);
const CYAN: Color = rgb(0x56b6c2);
const GREEN: Color = rgb(0x98c379);
pub(super) const MUTED: Color = rgb(0x6b707a);
pub(super) const HEAD: Color = rgb(0x8a8f99);
const COUNT: Color = rgb(0x5f646e);
const SELECTED_BG: Color = rgb(0x2f323a);
const WAITING: Color = rgb(0xc9a85c);
/// Panel background (mockup `project-rail.html`). The status line has the same background as the project rail.
const RAIL_BG: Color = rgb(0x1b1d23);
const LIST_BG: Color = rgb(0x23252b);
/// Background of the rightmost column of the project rail and list. Separates the panel from the neighboring pane.
const BORDER: Color = rgb(0x2c2f36);
const LIST_FG: Color = rgb(0xd0d3da);

/// The bottom row is the status line; from the left, 4 columns of project rail, (if expanded) 30 columns of list, and the rest is the terminal pane.
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
        launcher::draw(f.buffer_mut(), a.term, m.project(), &m.profiles);
    } else if let Some(sc) = screen {
        draw_term(f.buffer_mut(), a.term, sc);
        if m.attached.is_some()
            && m.status.is_none()
            && let Some(pos) = term_cursor(sc, a.term)
        {
            f.set_cursor_position(pos);
        }
    }
    // While typing, the status line takes the cursor.
    draw_status(f, a.status, m);
}

pub fn hit(a: &Areas, m: &Model, col: u16, row: u16) -> Hit {
    let pos = Position::new(col, row);
    if a.rail.contains(pos) {
        // Row 0 is a margin and item i is row i+1.
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
        // While the launcher covers the session pane, only the chosen rows can be clicked.
        if m.launcher {
            return launcher::hit(a.term, col, row, &m.profiles).map_or(Hit::None, Hit::Launch);
        }
        return Hit::Term(col - a.term.x, row - a.term.y);
    }
    Hit::None
}

/// Paints the project rail, list and status line backgrounds. Call it before drawing text.
/// It also paints cells with no text so they read as a panel and don't show through on a transparent window.
fn paint_panels(buf: &mut Buffer, a: &Areas) {
    buf.set_style(a.rail, Style::new().fg(LIST_FG).bg(RAIL_BG));
    if let Some(list) = a.list {
        buf.set_style(list, Style::new().fg(LIST_FG).bg(LIST_BG));
    }
    buf.set_style(a.status, Style::new().fg(LIST_FG).bg(RAIL_BG));
}

/// Changes the background of the rightmost column of the project rail and list to the border color, separating them from the neighboring pane.
/// Call it after drawing the text, so it covers even the background of the selected row.
///
/// ponytail: A one-cell strip that changes only the background so it doesn't cover text. A thin line glyph can wait until it's needed.
fn paint_edges(buf: &mut Buffer, a: &Areas) {
    for r in [Some(a.rail), a.list].into_iter().flatten() {
        let edge = Rect::new(r.right().saturating_sub(1), r.y, r.width.min(1), r.height);
        buf.set_style(edge, Style::new().bg(BORDER));
    }
}

/// Hyperlink spans of the session screen visible in the terminal pane (`area`). They don't cross lines, and cells where the same link continues are grouped.
pub fn link_runs(sc: &Screen, area: Rect) -> Vec<LinkRun> {
    let mut runs: Vec<LinkRun> = Vec::new();
    let mut open = None;
    for (x, y, cell) in visible_cells(sc, area) {
        // An address containing control characters can break the outer terminal's OSC 8, so it isn't treated as a link.
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
    // Cell 0 is the selection marker, cells 1–2 are the icon (a wide character fills both), and cell 3 is the needs-approval badge.
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

/// One row of the list.
enum Item {
    Title,
    /// The `visible_groups` index and that group.
    Head(usize, Group),
    Session(usize),
    /// The waiting text under a session that needs approval.
    Waiting(usize),
    /// This project has no sessions at all.
    Empty,
    NewHint,
}

impl Item {
    /// The session to attach to when this row is clicked.
    fn session(&self) -> Option<usize> {
        match self {
            Item::Session(i) | Item::Waiting(i) => Some(*i),
            _ => None,
        }
    }
}

/// List row order. The project name at the top, then for each status group a header and (if expanded) session rows (`(no sessions)` if none), and the new session hint at the bottom.
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

/// A session row. In the All view, a project-colored `■` first; for a session that has ended, `Exited` after the name; and the account chip at the end.
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
    let chip = account_chip(m, s.meta.account.as_deref());
    let fixed = spans_width(&left)
        + ended.as_ref().map_or(0, Span::width)
        + chip.as_ref().map_or(0, Span::width)
        + GUTTER;
    // Session names are ASCII, so the character count is the width.
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

/// A profile's chip: its letter on its color.
pub(super) fn chip(info: &ProfileInfo) -> Span<'static> {
    let style = Style::new()
        .fg(DARK)
        .bg(rgb(info.rgb()))
        .add_modifier(Modifier::BOLD);
    Span::styled(info.chip.clone(), style)
}

/// The chip of a session's profile. None for a shell, or a profile that isn't in the list.
fn account_chip(m: &Model, account: Option<&str>) -> Option<Span<'static>> {
    launch::profile(&m.profiles, account?).map(chip)
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(Span::width).sum()
}

/// Pads after `left` with spaces and puts `right` at the right end (before `GUTTER`). Fills `GUTTER` too so the selected row's background runs all the way.
fn fill(mut left: Vec<Span<'static>>, right: Option<Span<'static>>, width: usize) -> Line<'static> {
    let used = spans_width(&left) + right.as_ref().map_or(0, Span::width) + GUTTER;
    left.push(Span::raw(" ".repeat(width.saturating_sub(used))));
    left.extend(right);
    left.push(Span::raw(" ".repeat(GUTTER)));
    Line::from(left)
}

/// The session cells visible in the terminal pane and their outer screen coordinates. Excludes the trailing cell of a wide character (covered by the leading character) and
/// a wide character that would be only half visible at the edge of the pane.
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

/// A cell's character and the combining characters stacked on it. A control-character cell such as a tab is a space.
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

/// Colors 0–15. Sent as named colors so the outer terminal's color table is used as is.
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

/// alacritty color to ratatui color. The default foreground and background are the outer terminal's default colors (`Reset`).
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

/// The session cursor position. None if scrolled back into the past or the app hid the cursor.
fn term_cursor(sc: &Screen, area: Rect) -> Option<Position> {
    if sc.term().grid().display_offset() != 0 || !sc.mode().contains(TermMode::SHOW_CURSOR) {
        return None;
    }
    let (x, y) = sc.cursor();
    (x < area.width && y < area.height).then(|| Position::new(area.x + x, area.y + y))
}

/// `mine │ <location> │ <account>` and the key hints (or notice) on the right. While typing, shows that input and the hints.
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
                    // The profile's label and chip; a profile that isn't in the list shows its id.
                    if let Some(account) = &s.meta.account {
                        left.push(sep());
                        let info = launch::profile(&m.profiles, account);
                        left.push(Span::raw(info.map_or(account, |p| &p.label).clone()));
                        if let Some(info) = info {
                            left.push(Span::raw(" "));
                            left.push(chip(info));
                        }
                    }
                }
                // With no attached session, show the selected project's full path (instead of showing the icon name).
                None => left.push(Span::raw(
                    m.filter
                        .as_ref()
                        .map_or_else(|| "All projects".to_string(), |p| p.display().to_string()),
                )),
            }
            if m.message.is_empty() {
                let keys = if m.launcher {
                    launcher::keys(&m.profiles)
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
    // Put it at the right end, but if it overlaps the left side, start after the left side and let it be cut off.
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
            profiles: builtin(),
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

    /// The characters of one row. Skips the next cell covered by a wide character and strips trailing spaces.
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
            .unwrap_or_else(|| panic!("{needle} not found:\n{}", text(buf)))
    }

    fn kyoto() -> Model {
        model(vec![
            s(
                "/p/kyoto",
                "billing",
                Approval,
                "claude-b",
                Some("Approve npm run deploy?"),
            ),
            s("/p/kyoto", "ui", Working, "codex", None),
        ])
    }

    #[test]
    fn draws_rail_groups_and_chips() {
        let m = kyoto();
        let buf = render(&m, 100, 20);
        assert!(text(&buf).contains("◆ Needs approval"));
        assert!(text(&buf).contains("Approve npm run deploy?"));
        assert_eq!(cell(&buf, 1, 1).symbol(), "∗");
        assert!(row_of(&buf, "billing").ends_with("W"));
    }

    #[test]
    fn rail_marks_selection_badge_and_colors() {
        let buf = render(&kyoto(), 100, 20);
        // Row 0 is a margin, ∗ is row 1, and kyoto is row 2. The marker is left of the selected kyoto, and since it needs approval, the badge is on the right.
        assert_eq!(cell(&buf, 0, 1).symbol(), " ");
        assert_eq!(cell(&buf, 0, 2).symbol(), "▌");
        assert_eq!(cell(&buf, 1, 2).symbol(), "k");
        assert_eq!(cell(&buf, 3, 2).symbol(), "◆");
        assert_eq!(cell(&buf, 3, 2).fg, Color::Rgb(0xe5, 0xc0, 0x7b));
        assert_eq!(cell(&buf, 1, 1).bg, Color::Rgb(0x3a, 0x3d, 0x45));
        assert_eq!(cell(&buf, 1, 2).fg, Color::Rgb(0x1e, 0x21, 0x28));
        // kyoto's color is PROJECT_COLORS[1] (see the view tests). Both icon cells have the same background.
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
        assert!(row_of(&buf, "gone").ends_with('P'));
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
            .unwrap_or_else(|| panic!("{needle} not found:\n{}", text(buf)))
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
            panic!("not an Idle group header");
        };
        m.toggle_group(g);
        let buf = render(&m, 100, 20);
        assert!(row_text(&buf, head).contains("▾ ○ Idle"));
        assert_eq!(hit(&a, &m, list_x, y_of(&buf, "shellx")), Hit::Row(1));
        // Pressing once more collapses it again.
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
        // Even if the user collapses it, the attached session's group stays expanded.
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
    fn status_line_and_launcher_hint_show_profile_labels() {
        let mut m = kyoto();
        m.attached = Some("k-billing".into());
        let status = row_text(&render(&m, 140, 20), 19);
        assert!(
            status.starts_with("mine │ /p/kyoto/billing │ work W"),
            "{status}"
        );
        assert!(!status.contains("claude-b"), "{status}");
        m.attached = Some("k-ui".into());
        let status = row_text(&render(&m, 140, 20), 19);
        assert!(
            status.starts_with("mine │ /p/kyoto/ui │ Codex C"),
            "{status}"
        );
        // A session whose profile isn't in the list shows its id and no chip.
        m.profiles.retain(|p| p.id != "codex");
        let status = row_text(&render(&m, 140, 20), 19);
        assert!(
            status.starts_with("mine │ /p/kyoto/ui │ codex  "),
            "{status}"
        );
        // The launcher rows and key hints use the same names.
        m.profiles = builtin();
        m.launcher = true;
        let buf = render(&m, 140, 20);
        assert!(row_of(&buf, "1  personal").ends_with("P"));
        assert!(row_of(&buf, "2  work").ends_with("W"));
        assert!(row_text(&buf, 19).ends_with("↵ personal · 1–3 profiles · s shell"));
        // Before the list arrives only shell is offered.
        m.profiles.clear();
        let status = row_text(&render(&m, 140, 20), 19);
        assert!(
            status.ends_with(" s shell") && !status.contains('↵'),
            "{status}"
        );
    }

    #[test]
    fn chip_uses_profile_color() {
        let mut m = model(vec![s("/p/kyoto", "deploy", Idle, "zeta", None)]);
        m.profiles = vec![profile(
            "zeta",
            mine_core::profiles::ProfileKind::Claude,
            "ops",
            "#123456",
        )];
        m.attached = Some("k-deploy".into());
        let buf = render(&m, 140, 20);
        // The list row ends with the profile's chip letter on the profile's color.
        let y = y_of(&buf, "deploy");
        let x = (0..LIST_W + RAIL_W)
            .rev()
            .find(|&x| cell(&buf, x, y).symbol() != " ")
            .unwrap();
        assert_eq!(cell(&buf, x, y).symbol(), "O");
        assert_eq!(cell(&buf, x, y).bg, Color::Rgb(0x12, 0x34, 0x56));
        // The status line carries the same chip after the label.
        let status = row_text(&buf, 19);
        assert!(
            status.starts_with("mine │ /p/kyoto/deploy │ ops O"),
            "{status}"
        );
        let x = status.chars().position(|c| c == 'O').unwrap() as u16;
        assert_eq!(cell(&buf, x, 19).bg, Color::Rgb(0x12, 0x34, 0x56));
        // A session on a profile that isn't in the list has no chip.
        m.profiles.clear();
        assert!(
            row_of(&render(&m, 140, 20), "deploy")
                .trim_end()
                .ends_with("deploy")
        );
    }

    #[test]
    fn status_line_shows_location_account_and_prompt() {
        let mut m = kyoto();
        m.attached = Some("k-billing".into());
        let buf = render(&m, 140, 20);
        let status = row_text(&buf, 19);
        assert!(
            status.starts_with("mine │ /p/kyoto/billing │ work"),
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
        // The whole status line is replaced by that text, and the session cursor isn't moved.
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
                // Links opened separately are different spans even with the same address.
                LinkRun {
                    x: 14,
                    y: 3,
                    url: "https://x.example".into(),
                    text: "y".into()
                },
            ]
        );
        // If the pane is narrower than the screen, only the clipped part is returned.
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
        // Paint down to the cells below that have no text.
        for y in [0, 5, 28] {
            assert_eq!((bg(0, y), bg(2, y)), (rail, rail), "rail y={y}");
            assert_eq!((bg(4, y), bg(20, y)), (list, list), "list y={y}");
            // The rightmost column of the project rail and the rightmost column of the list are the border.
            assert_eq!((bg(3, y), bg(33, y)), (border, border), "border y={y}");
        }
        // The terminal pane isn't painted. The status line has the same background as the project rail.
        assert_eq!(bg(34, 10), Color::Reset);
        assert_eq!((bg(0, 29), bg(35, 29), bg(60, 29)), (rail, rail, rail));
        // The selected row has the selection color over the list background, and its last column is still the border.
        assert_eq!(bg(10, 2), Color::Rgb(0x2f, 0x32, 0x3a));
        assert_eq!(bg(33, 2), border);
        // Text colors: list text, title (bold), group header, waiting text.
        let fg = |x, y| cell(&buf, x, y).fg;
        // The list starts at column 4: a blank cell, session name (`billing` is column 6), title, group header, waiting text.
        assert_eq!(fg(10, 8), Color::Rgb(0xd0, 0xd3, 0xda));
        assert_eq!(fg(6, 2), Color::Rgb(0xd0, 0xd3, 0xda));
        assert_eq!(fg(4, 0), Color::Rgb(0xe6, 0xe6, 0xe6));
        assert!(cell(&buf, 4, 0).modifier.contains(Modifier::BOLD));
        assert_eq!(fg(4, 1), Color::Rgb(0x8a, 0x8f, 0x99));
        assert_eq!(fg(8, 3), Color::Rgb(0xc9, 0xa8, 0x5c));
        // The status line text has a color readable over the background.
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
        // The front of the row is the project rail. The list starts at column 4 and is indented two cells like session rows.
        assert!(row_text(&buf, 1).ends_with("  (no sessions)"));
        assert_eq!(cell(&buf, 6, 1).symbol(), "(");
        assert_eq!(cell(&buf, 6, 1).fg, Color::Rgb(0x6b, 0x70, 0x7a));
        assert!(text(&buf).contains("＋ New session  ⌃a c"));
        // It isn't visible when there are sessions. If there are only other projects' sessions, this project is empty.
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
        // The launcher is drawn in the terminal pane instead of the session screen.
        assert!(!text(&buf).contains("hello"));
        assert!(row_of(&buf, "personal").contains("1  personal"));
        assert!(row_text(&buf, 19).ends_with("↵ personal · 1–3 profiles · s shell"));
        // Mouse: a chosen row is `Launch`; the rest of the pane isn't passed to the session.
        let y = y_of(&buf, "work");
        let x = (0..100)
            .find(|&x| cell(&buf, x, y).symbol() == "2")
            .unwrap();
        assert_eq!(
            hit(&a, &m, x, y),
            Hit::Launch(Choice::Profile("claude-b".into()))
        );
        assert_eq!(hit(&a, &m, a.term.x, a.term.y), Hit::None);
        // Turning it off returns to the session screen and the usual hints.
        m.launcher = false;
        m.attached = Some("k-x".into());
        let buf = render_with_screen(&m, &sc, 100, 20);
        assert!(text(&buf).contains("hello") && !text(&buf).contains("personal"));
        assert!(row_text(&buf, 19).ends_with("d detach"));
        assert_eq!(hit(&a, &m, a.term.x, a.term.y), Hit::Term(0, 0));
    }
}
