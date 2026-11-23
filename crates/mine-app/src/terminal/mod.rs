//! 터미널 칸: 붙은 세션의 화면(`Model::screen`)을 그리고, 칸 크기·휠 스크롤·링크 누르기를 모델에 잇는다.
//!
//! 구조는 Zed의 터미널과 같다. [`render`]가 만드는 `div`(포커스, 키 문맥 [`CONTEXT`], 마우스 처리) 안에 `canvas`
//! 하나가 있다.
//! - prepaint: 칸 크기로 행·열을 구해 모델에 알린다(처음이면 `Attach`, 바뀌면 `Resize`). 그다음 보이는 행마다
//!   [`render::row_runs`]로 글자 묶음을 만들어 모양을 잡는다(shape).
//! - paint: 바탕 사각형, 글자, 커서 순서로 그린다.

pub mod render;

use std::time::Instant;

use alacritty_terminal::vte::ansi::CursorShape;
use gpui::{
    App, Bounds, ContentMask, Context, Entity, FocusHandle, FontStyle, FontWeight, Hsla,
    InteractiveElement, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ParentElement, Pixels, Point, ScrollWheelEvent, ShapedLine, StrikethroughStyle, Styled,
    TextRun, TouchPhase, UnderlineStyle, Window, WindowTextSystem, canvas, div, fill, font,
    outline, point, px, rgb, size,
};

use crate::theme::{TERM_BG, TERM_CURSOR};
use crate::workspace::Workspace;
use render::{CellRun, CursorCell};

/// 터미널 칸의 키 문맥. `Workspace` 문맥 안에 있어서 칸에 포커스가 있어도 ⌘ 단축키는 그대로 온다.
pub const CONTEXT: &str = "Terminal";
const FONT_FAMILY: &str = "Menlo";
const FONT_SIZE: Pixels = px(13.);
/// 칸 가장자리 여백.
const PADDING: Pixels = px(6.);
/// 흐린 글(SGR 2)의 불투명도.
const DIM: f32 = 0.6;
/// 막대·밑줄 커서의 두께.
const CARET: Pixels = px(2.);

/// 셀 하나의 크기. Menlo 13px의 'M' 폭과 줄 높이(ascent + descent를 올림)다.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metrics {
    pub cell_w: Pixels,
    pub line_h: Pixels,
}

impl Metrics {
    pub fn measure(window: &Window) -> Metrics {
        let text = window.text_system();
        let id = text.resolve_font(&font(FONT_FAMILY));
        let cell_w = text
            .advance(id, FONT_SIZE, 'M')
            .map_or(FONT_SIZE * 0.6, |advance| advance.width);
        // 글꼴의 descent는 음수다.
        let line_h = (text.ascent(id, FONT_SIZE) + text.descent(id, FONT_SIZE).abs()).ceil();
        Metrics { cell_w, line_h }
    }
}

/// 마지막으로 그린 칸의 자리와 셀 크기. 마우스 자리를 셀로 바꿀 때 쓴다(Task 6의 선택도 쓴다).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    /// 첫 셀(0, 0)의 왼쪽 위, 창 좌표.
    pub origin: Point<Pixels>,
    pub metrics: Metrics,
    pub cols: u16,
    pub rows: u16,
}

impl Geometry {
    /// 창 좌표 `pos`가 놓인 셀(열, 행). 칸 밖이면 없다.
    pub fn cell_at(&self, pos: Point<Pixels>) -> Option<(u16, u16)> {
        let x = f32::from(pos.x - self.origin.x) / f32::from(self.metrics.cell_w);
        let y = f32::from(pos.y - self.origin.y) / f32::from(self.metrics.line_h);
        if !(x >= 0.0 && y >= 0.0) {
            return None;
        }
        let (col, row) = (x as u16, y as u16);
        (col < self.cols && row < self.rows).then_some((col, row))
    }

    /// (`col`, `row`)부터 `cells`칸의 사각형, 창 좌표.
    pub fn cell_bounds(&self, col: u16, row: u16, cells: u16) -> Bounds<Pixels> {
        let Metrics { cell_w, line_h } = self.metrics;
        Bounds::new(
            point(
                self.origin.x + cell_w * f32::from(col),
                self.origin.y + line_h * f32::from(row),
            ),
            size(cell_w * f32::from(cells), line_h),
        )
    }
}

/// 터미널 칸의 뷰 상태. 화면과 붙기·크기는 `Model`에 있고, 여기에는 GPUI 쪽 일만 있다.
pub struct Pane {
    /// 세션이 붙어 실행 화면이 닫혀 있으면 늘 여기에 포커스를 둔다. Task 5의 키 입력과 입력기가 여기에 걸린다.
    pub focus: FocusHandle,
    /// 마지막으로 그린 칸. 그리기 전에는 없다.
    pub geometry: Option<Geometry>,
    /// 한 줄이 안 되는 휠 이동의 나머지(픽셀).
    scroll_acc: f32,
    /// 링크 셀에서 왼쪽 단추를 누른 자리. 같은 자리에서 떼면 연다. 다른 셀로 끌면 버린다.
    press: Option<(u16, u16)>,
    /// 끝나지 않은 동기화 갱신에 건 타이머의 기한. 같은 기한에 두 번 걸지 않는다.
    sync_timer: Option<Instant>,
}

impl Pane {
    pub fn new(cx: &mut App) -> Pane {
        Pane {
            focus: cx.focus_handle(),
            geometry: None,
            scroll_acc: 0.0,
            press: None,
            sync_timer: None,
        }
    }
}

impl Workspace {
    /// 트랙패드·휠: 화면 사본만 스크롤한다. 세션이 마우스 보고를 켰을 때 세션에 넘기는 것은 Task 6이 한다.
    fn term_scroll(&mut self, ev: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let Some(geometry) = self.term.geometry else {
            return;
        };
        if matches!(ev.touch_phase, TouchPhase::Started) {
            self.term.scroll_acc = 0.0;
        }
        let line_h = geometry.metrics.line_h;
        let delta = f32::from(ev.delta.pixel_delta(line_h).y);
        let lines = render::scroll_lines(&mut self.term.scroll_acc, delta, f32::from(line_h));
        if lines != 0 && self.model.scroll(lines) {
            cx.notify();
        }
    }

    fn term_link(&self, (col, row): (u16, u16)) -> Option<String> {
        render::link_at(self.model.screen.as_ref()?, col, row)
    }

    fn term_cell(&self, pos: Point<Pixels>) -> Option<(u16, u16)> {
        self.term.geometry?.cell_at(pos)
    }

    fn term_press(&mut self, ev: &MouseDownEvent) {
        self.term.press = self
            .term_cell(ev.position)
            .filter(|&at| self.term_link(at).is_some());
    }

    fn term_drag(&mut self, ev: &MouseMoveEvent) {
        if self.term.press.is_some()
            && ev.pressed_button == Some(MouseButton::Left)
            && self.term_cell(ev.position) != self.term.press
        {
            self.term.press = None;
        }
    }

    /// 누른 링크 셀에서 끌지 않고 떼면 그 링크를 연다.
    fn term_release(&mut self, ev: &MouseUpEvent, cx: &mut Context<Self>) {
        let Some(at) = self.term.press.take() else {
            return;
        };
        if self.term_cell(ev.position) == Some(at)
            && let Some(url) = self.term_link(at).filter(|url| render::openable(url))
        {
            cx.open_url(&url);
        }
    }

    /// 붙은 세션이 끝내지 않은 동기화 갱신(`?2026`)에 GPUI 타이머를 건다. 기한이 지나면 끝내고 다시 그린다.
    pub(crate) fn watch_sync(&mut self, cx: &mut Context<Self>) {
        let Some(deadline) = self.model.sync_deadline() else {
            return;
        };
        if self.term.sync_timer == Some(deadline) {
            return;
        }
        self.term.sync_timer = Some(deadline);
        cx.spawn(async move |this, cx| {
            let wait = deadline.saturating_duration_since(Instant::now());
            cx.background_executor().timer(wait).await;
            let _ = this.update(cx, |ws, cx| {
                ws.term.sync_timer = None;
                if ws.model.end_stale_sync(Instant::now()) {
                    cx.notify();
                }
                // 그사이 새 갱신이 열렸으면 그 기한에 다시 건다.
                ws.watch_sync(cx);
            });
        })
        .detach();
    }
}

/// 붙은 세션의 칸. 칸을 누르면 포커스가 온다.
pub fn render(ws: &Workspace, window: &Window, cx: &mut Context<Workspace>) -> impl IntoElement {
    let entity = cx.entity();
    let focused = ws.term.focus.is_focused(window);
    div()
        .flex_1()
        .min_h_0()
        .p(PADDING)
        .bg(rgb(TERM_BG))
        .track_focus(&ws.term.focus)
        .key_context(CONTEXT)
        .on_scroll_wheel(cx.listener(|ws, ev: &ScrollWheelEvent, _, cx| ws.term_scroll(ev, cx)))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|ws, ev: &MouseDownEvent, _, _| ws.term_press(ev)),
        )
        .on_mouse_move(cx.listener(|ws, ev: &MouseMoveEvent, _, _| ws.term_drag(ev)))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|ws, ev: &MouseUpEvent, _, cx| ws.term_release(ev, cx)),
        )
        .child(
            canvas(
                move |bounds, window, cx| layout(&entity, focused, bounds, window, cx),
                |_, frame, window, cx| frame.paint(window, cx),
            )
            .size_full(),
        )
}

/// prepaint: 칸 크기를 모델에 알리고, 보이는 행의 글자 묶음과 커서 모양을 잡는다.
fn layout(
    ws: &Entity<Workspace>,
    focused: bool,
    bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
) -> Frame {
    let metrics = Metrics::measure(window);
    let (cols, rows) = render::grid_size(
        bounds.size.width.into(),
        bounds.size.height.into(),
        metrics.cell_w.into(),
        metrics.line_h.into(),
    );
    let geometry = Geometry {
        origin: bounds.origin,
        metrics,
        cols,
        rows,
    };
    // 뷰의 render는 끝났으니 뷰를 고칠 수 있다. 화면은 여기서 칸 크기로 맞춰지므로 이번 프레임이 새 크기로 그려진다.
    let (runs, cursor) = ws.update(cx, |ws, _| {
        ws.term.geometry = Some(geometry);
        let effects = ws.model.set_grid(cols, rows);
        ws.apply(effects);
        let Some(screen) = &ws.model.screen else {
            return (Vec::new(), None);
        };
        let runs: Vec<(u16, CellRun)> = (0..rows)
            .flat_map(|row| {
                render::row_runs(screen, usize::from(row))
                    .into_iter()
                    .map(move |run| (row, run))
            })
            .collect();
        (runs, render::cursor(screen))
    });
    let text = window.text_system().clone();
    let cells = runs
        .into_iter()
        .map(|(row, run)| Painted::shape(&text, &geometry, row, &run))
        .collect();
    let caret = cursor
        .filter(|c| c.col < cols && c.row < rows)
        .map(|c| Caret::shape(&text, &geometry, &c, focused));
    Frame {
        bounds,
        line_h: metrics.line_h,
        cells,
        caret,
    }
}

/// 한 프레임에 그릴 것.
struct Frame {
    bounds: Bounds<Pixels>,
    line_h: Pixels,
    cells: Vec<Painted>,
    caret: Option<Caret>,
}

impl Frame {
    fn paint(self, window: &mut Window, cx: &mut App) {
        let mask = ContentMask {
            bounds: self.bounds,
        };
        window.with_content_mask(Some(mask), |window| {
            for cell in &self.cells {
                if let Some(bg) = cell.bg {
                    window.paint_quad(fill(cell.bounds, bg));
                }
            }
            if let Some(caret) = &self.caret
                && caret.filled()
            {
                window.paint_quad(fill(caret.bounds, hsla(TERM_CURSOR)));
            }
            for cell in &self.cells {
                let _ = cell.line.paint(cell.text_at, self.line_h, window, cx);
            }
            if let Some(caret) = &self.caret {
                caret.paint(self.line_h, window, cx);
            }
        });
    }
}

/// 모양을 잡은 글자 묶음 하나와 그 자리.
struct Painted {
    bounds: Bounds<Pixels>,
    bg: Option<Hsla>,
    line: ShapedLine,
    /// 글자를 놓을 자리. 넓은 글자는 두 칸 가운데로 민다.
    text_at: Point<Pixels>,
}

impl Painted {
    fn shape(text: &WindowTextSystem, geometry: &Geometry, row: u16, run: &CellRun) -> Painted {
        let mut color = hsla(run.fg);
        if run.dim {
            color = color.opacity(DIM);
        }
        let mut face = font(FONT_FAMILY);
        if run.bold {
            face.weight = FontWeight::BOLD;
        }
        if run.italic {
            face.style = FontStyle::Italic;
        }
        let style = TextRun {
            len: run.text.len(),
            font: face,
            color,
            background_color: None,
            underline: run.underline.then_some(UnderlineStyle {
                color: Some(color),
                thickness: px(1.),
                wavy: false,
            }),
            strikethrough: run.strike.then_some(StrikethroughStyle {
                color: Some(color),
                thickness: px(1.),
            }),
        };
        let force = glyph_width(run, geometry.metrics.cell_w);
        let bounds = geometry.cell_bounds(run.col, row, run.cells);
        let line = text.shape_line(run.text.clone().into(), FONT_SIZE, &[style], force);
        // 넓은 글자(한글 등)는 대체 글꼴의 폭이 두 칸보다 좁다. 왼쪽에 붙이면 글자 뒤만 비니 두 칸 가운데에 놓는다.
        // 줄의 폭은 칸에 맞추기 전 글꼴이 놓은 폭이다.
        let mut text_at = bounds.origin;
        let chars = run.text.chars().count();
        if usize::from(run.cells) == 2 * chars {
            let slot = geometry.metrics.cell_w * 2.;
            let natural = line.width / chars as f32;
            if natural < slot {
                text_at.x += (slot - natural) / 2.;
            }
        }
        Painted {
            bounds,
            bg: run.bg.map(hsla),
            line,
            text_at,
        }
    }
}

/// 글자 하나가 차지할 폭. 글자마다 셀(넓은 글자는 두 칸)에 맞춰 놓아야 대체 글꼴(한글 등)이 섞여도 칸이 맞는다.
/// 결합 문자가 얹힌 셀은 글자 수와 칸 수가 맞지 않으니 글꼴이 놓은 대로 둔다.
///
/// ponytail: 넓은 글자 하나에 결합 문자 하나가 얹힌 셀은 좁은 글자 둘과 구별하지 못해 결합 문자가 반 칸 왼쪽에
/// 놓인다. 드물어서 둔다.
fn glyph_width(run: &CellRun, cell_w: Pixels) -> Option<Pixels> {
    let chars = run.text.chars().count();
    match usize::from(run.cells) {
        cells if cells == chars => Some(cell_w),
        cells if cells == 2 * chars => Some(cell_w * 2.),
        _ => None,
    }
}

/// 그릴 커서. 블록은 칸에 포커스가 있을 때 칠하고 그 글자를 바탕색으로 다시 쓴다. 포커스가 없으면 빈 블록이다.
struct Caret {
    bounds: Bounds<Pixels>,
    shape: CursorShape,
    focused: bool,
    glyph: Option<ShapedLine>,
}

impl Caret {
    fn shape(text: &WindowTextSystem, geometry: &Geometry, c: &CursorCell, focused: bool) -> Caret {
        let glyph =
            (focused && c.shape == CursorShape::Block && !c.text.trim().is_empty()).then(|| {
                let style = TextRun {
                    len: c.text.len(),
                    font: font(FONT_FAMILY),
                    color: hsla(TERM_BG),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                text.shape_line(c.text.clone().into(), FONT_SIZE, &[style], None)
            });
        Caret {
            bounds: geometry.cell_bounds(c.col, c.row, c.cells),
            shape: c.shape,
            focused,
            glyph,
        }
    }

    /// 글자 밑에 칠하는 블록인가.
    fn filled(&self) -> bool {
        self.focused && self.shape == CursorShape::Block
    }

    fn paint(&self, line_h: Pixels, window: &mut Window, cx: &mut App) {
        let Bounds { origin, size: cell } = self.bounds;
        let color = hsla(TERM_CURSOR);
        match self.shape {
            CursorShape::Block if self.focused => {
                if let Some(glyph) = &self.glyph {
                    // 셀 가운데에 놓는다(넓은 글자는 두 칸 가운데).
                    let dx = ((cell.width - glyph.width) / 2.).max(px(0.));
                    let _ = glyph.paint(point(origin.x + dx, origin.y), line_h, window, cx);
                }
            }
            CursorShape::Block | CursorShape::HollowBlock => {
                window.paint_quad(outline(self.bounds, color, Default::default()));
            }
            CursorShape::Underline => {
                let at = point(origin.x, origin.y + cell.height - CARET);
                window.paint_quad(fill(Bounds::new(at, size(cell.width, CARET)), color));
            }
            CursorShape::Beam => {
                window.paint_quad(fill(Bounds::new(origin, size(CARET, cell.height)), color));
            }
            CursorShape::Hidden => {}
        }
    }
}

fn hsla(color: u32) -> Hsla {
    rgb(color).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry() -> Geometry {
        Geometry {
            origin: point(px(100.), px(50.)),
            metrics: Metrics {
                cell_w: px(8.),
                line_h: px(16.),
            },
            cols: 10,
            rows: 5,
        }
    }

    #[test]
    fn cell_at_maps_window_points_to_cells() {
        let g = geometry();
        assert_eq!(g.cell_at(point(px(100.), px(50.))), Some((0, 0)));
        assert_eq!(g.cell_at(point(px(115.9), px(81.9))), Some((1, 1)));
        assert_eq!(g.cell_at(point(px(179.), px(129.))), Some((9, 4)));
        // 칸 밖(왼쪽 위 여백, 오른쪽 아래 끝 너머)은 셀이 없다.
        assert_eq!(g.cell_at(point(px(99.), px(60.))), None);
        assert_eq!(g.cell_at(point(px(120.), px(49.))), None);
        assert_eq!(g.cell_at(point(px(180.), px(60.))), None);
        assert_eq!(g.cell_at(point(px(120.), px(130.))), None);
        assert_eq!(
            g.cell_bounds(2, 1, 2),
            Bounds::new(point(px(116.), px(66.)), size(px(16.), px(16.)))
        );
    }

    #[test]
    fn glyphs_snap_to_cells_unless_combining() {
        let run = |text: &str, cells: u16| CellRun {
            col: 0,
            cells,
            text: text.into(),
            fg: 0,
            bg: None,
            bold: false,
            italic: false,
            underline: false,
            strike: false,
            dim: false,
            link: None,
        };
        let w = px(8.);
        assert_eq!(glyph_width(&run("ab", 2), w), Some(px(8.)));
        assert_eq!(glyph_width(&run("한글", 4), w), Some(px(16.)));
        assert_eq!(glyph_width(&run("e\u{301}", 1), w), None);
    }
}
