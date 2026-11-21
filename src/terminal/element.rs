use std::hash::{Hash, Hasher};
use std::sync::Arc;

use alacritty_terminal::index::Point as GridPoint;
use alacritty_terminal::selection::SelectionRange;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color as AnsiColor, CursorShape, NamedColor};
use gpui::{
    App, BorderStyle, Bounds, ContentMask, Corners, DispatchPhase, Element, ElementId, Entity,
    Font, FontFallbacks, FontStyle, FontWeight, GlobalElementId, Hsla, InspectorElementId,
    IntoElement, LayoutId, LineLayout, MouseButton, MouseMoveEvent, MouseUpEvent, PaintQuad,
    Pixels, Point, RenderImage, ShapedLine, SharedString, StrikethroughStyle, Style, TextRun,
    UnderlineStyle, Window, fill, point, px, quad, relative, size,
};

use super::TerminalPane;
use super::colors::{blend, resolve};
use super::images;
use super::placeholder::{self, ImageCell};
use super::session::TermSize;
use crate::config::Theme;
use crate::config::schema::{FontWeightName, UnfocusedCursor};

/// One cell copied out of the grid while the term lock is held.
struct CellSnap {
    c: char,
    zerowidth: Option<Vec<char>>,
    fg: AnsiColor,
    bg: AnsiColor,
    flags: Flags,
    /// The slice of a picture this cell shows. Such a cell is copied out as
    /// a blank, so nothing downstream shapes its placeholder or its marks.
    image: Option<ImageCell>,
}

/// A rectangle of one placement's cells as they sit on screen: `cols` ×
/// `rows` cells from screen position (`row`, `col`), the top-left one
/// showing `first`. Usually the whole image; less when it's part scrolled
/// off, part overwritten, or wrapped by a narrower window.
#[derive(Debug, PartialEq)]
struct ImageRun {
    first: ImageCell,
    row: usize,
    col: usize,
    cols: usize,
    rows: usize,
}

/// One sprite: the whole image scaled into `bounds`, of which `clip` shows.
struct ImagePaint {
    image: Arc<RenderImage>,
    bounds: Bounds<Pixels>,
    clip: Bounds<Pixels>,
}

struct CursorLayout {
    bounds: Bounds<Pixels>,
    shape: CursorShape,
    color: Hsla,
    /// Bar width / underline height in pixels, from `cursor.thickness`.
    thickness: Pixels,
    /// For block cursors: the glyph underneath, re-shaped in the background color.
    glyph: Option<ShapedLine>,
}

pub struct GridLayout {
    origin: Point<Pixels>,
    cell_height: f32,
    bg_quads: Vec<PaintQuad>,
    images: Vec<ImagePaint>,
    selection_quads: Vec<PaintQuad>,
    lines: Vec<(usize, ShapedLine)>,
    cursor: Option<CursorLayout>,
}

pub struct TerminalElement {
    pane: Entity<TerminalPane>,
    focused: bool,
}

impl TerminalElement {
    pub fn new(pane: Entity<TerminalPane>, focused: bool) -> Self {
        Self { pane, focused }
    }
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = GridLayout;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        style.size.height = relative(1.0).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> GridLayout {
        let focused = self.focused;
        self.pane
            .update(cx, |pane, _cx| layout_grid(pane, bounds, focused, window))
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        layout: &mut GridLayout,
        window: &mut Window,
        cx: &mut App,
    ) {
        // The pane's own mouse handlers stop at its edges. A drag doesn't,
        // whether it's our selection or a program's: outside them it
        // scrolls the view, and ends on release.
        let pane = self.pane.read(cx);
        if pane.selecting || pane.reporting.is_some() {
            let pane = self.pane.clone();
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                if phase == DispatchPhase::Capture
                    && event.pressed_button == Some(MouseButton::Left)
                    && !bounds.contains(&event.position)
                {
                    pane.update(cx, |pane, cx| pane.on_mouse_move(event, window, cx));
                }
            });
            let pane = self.pane.clone();
            window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
                if phase == DispatchPhase::Capture
                    && event.button == MouseButton::Left
                    && !bounds.contains(&event.position)
                {
                    pane.update(cx, |pane, cx| pane.on_mouse_up(event, window, cx));
                }
            });
        }
        for q in layout.bg_quads.drain(..) {
            window.paint_quad(q);
        }
        for image in layout.images.drain(..) {
            let mask = Some(ContentMask { bounds: image.clip });
            window.with_content_mask(mask, |window| {
                window
                    .paint_image(image.bounds, Corners::default(), image.image, 0, false)
                    .ok();
            });
        }
        for q in layout.selection_quads.drain(..) {
            window.paint_quad(q);
        }
        let line_height = px(layout.cell_height);
        for (row, line) in &layout.lines {
            let origin = point(
                layout.origin.x,
                layout.origin.y + px(*row as f32 * layout.cell_height),
            );
            line.paint(origin, line_height, window, cx).ok();
        }
        if let Some(cursor) = &layout.cursor {
            match cursor.shape {
                CursorShape::Block => {
                    window.paint_quad(fill(cursor.bounds, cursor.color));
                    if let Some(glyph) = &cursor.glyph {
                        glyph
                            .paint(cursor.bounds.origin, cursor.bounds.size.height, window, cx)
                            .ok();
                    }
                }
                CursorShape::HollowBlock => {
                    window.paint_quad(quad(
                        cursor.bounds,
                        px(0.0),
                        gpui::transparent_black(),
                        px(1.0),
                        cursor.color,
                        BorderStyle::Solid,
                    ));
                }
                CursorShape::Beam => {
                    let mut b = cursor.bounds;
                    b.size.width = cursor.thickness;
                    window.paint_quad(fill(b, cursor.color));
                }
                CursorShape::Underline => {
                    let mut b = cursor.bounds;
                    b.origin.y = b.origin.y + b.size.height - cursor.thickness;
                    b.size.height = cursor.thickness;
                    window.paint_quad(fill(b, cursor.color));
                }
                CursorShape::Hidden => {}
            }
        }
    }
}

/// The glyph to shape for a cell. Alacritty stores the literal control
/// character in the cell where e.g. a TAB began and pads the span with spaces;
/// shaping a real '\t' would use the font's tab advance instead of one cell
/// and skew everything after it (visible as ragged `ls` columns).
fn display_char(c: char) -> char {
    if (c as u32) < 0x20 || c == '\u{7f}' {
        ' '
    } else {
        c
    }
}

fn base_font(pane: &TerminalPane, bold: bool, italic: bool) -> Font {
    let weight = match (pane.config().font.weight, bold) {
        (_, true) => FontWeight::BOLD,
        (FontWeightName::Normal, false) => FontWeight::NORMAL,
        (FontWeightName::Medium, false) => FontWeight::MEDIUM,
        (FontWeightName::Bold, false) => FontWeight::BOLD,
    };
    let family = &pane.config().font.family;
    Font {
        family: SharedString::from(family.primary().to_string()),
        features: if pane.config().font.ligatures {
            Default::default()
        } else {
            gpui::FontFeatures::disable_ligatures()
        },
        fallbacks: (!family.fallbacks().is_empty())
            .then(|| FontFallbacks::from_fonts(family.fallbacks().to_vec())),
        weight,
        style: if italic {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        },
    }
}

fn color_key(c: Hsla) -> u64 {
    let rgba: gpui::Rgba = c.into();
    let r = (rgba.r * 255.0) as u64;
    let g = (rgba.g * 255.0) as u64;
    let b = (rgba.b * 255.0) as u64;
    let a = (rgba.a * 255.0) as u64;
    (r << 24) | (g << 16) | (b << 8) | a
}

fn layout_grid(
    pane: &mut TerminalPane,
    bounds: Bounds<Pixels>,
    focused: bool,
    window: &mut Window,
) -> GridLayout {
    let theme: Theme = (**pane.theme()).clone();
    let pad_x = pane.config().window.padding.x;
    let pad_y = pane.config().window.padding.y;
    let font_size = px((pane.config().font.size + pane.font_delta).max(6.0));
    let line_height_mult = pane.config().font.line_height.max(1.0);

    // Measure the cell. cell_width stays the exact glyph advance so painted
    // runs, background quads, and the cursor all use the same column math;
    // cell_height is rounded to whole pixels so rows don't accumulate error.
    let font = base_font(pane, false, false);
    let text_system = window.text_system().clone();
    let font_id = text_system.resolve_font(&font);
    let cell_width = text_system
        .advance(font_id, font_size, 'm')
        .map(|s| f32::from(s.width))
        .unwrap_or(8.0);
    let cell_height = (f32::from(font_size) * line_height_mult).round();

    let origin = point(bounds.origin.x + px(pad_x), bounds.origin.y + px(pad_y));
    let avail_w = f32::from(bounds.size.width) - pad_x * 2.0;
    let avail_h = f32::from(bounds.size.height) - pad_y * 2.0;
    let columns = ((avail_w / cell_width).floor() as usize).max(2);
    let screen_lines = ((avail_h / cell_height).floor() as usize).max(1);

    let scale = window.scale_factor();
    let new_size = TermSize {
        columns,
        screen_lines,
        cell_width,
        cell_height,
        scale,
    };
    let grid_changed = new_size != pane.size;
    let columns_changed = columns != pane.size.columns;
    pane.size = new_size;
    if columns_changed {
        // A paged preview was laid out for the old width: render it again
        // and have less reload the file (`R`).
        if let Some(code) = pane.preview_render.as_ref().and_then(|render| render(columns)) {
            pane.preview_code = code;
            if let Some(session) = &pane.session {
                session.write_input(b"R".as_slice());
            }
        }
    }
    if grid_changed {
        // Resize only when the grid changed, not on every pixel of a window
        // drag — this is the debounce that prevents SIGWINCH storms. The
        // cell's size in pixels counts as the grid: a font zoom or a move
        // to another display can keep the cell counts and still change the
        // pixel size programs draw images against.
        if let Some(session) = &pane.session {
            session.resize(new_size);
        }
    }

    let mut layout = GridLayout {
        origin,
        cell_height,
        bg_quads: Vec::new(),
        images: Vec::new(),
        selection_quads: Vec::new(),
        lines: Vec::with_capacity(screen_lines),
        cursor: None,
    };
    let Some(session) = &pane.session else {
        pane.last_layout = Some(super::LastLayout {
            bounds,
            cell_width,
            cell_height,
            display_offset: 0,
        });
        return layout;
    };

    // --- Lock the term, copy out, release. Never hold this into shaping. ---
    let mut rows: Vec<Vec<CellSnap>> = (0..screen_lines).map(|_| Vec::new()).collect();
    let (cursor, display_offset, selection, mode, cursor_style);
    {
        let term = session.term.lock();
        let content = term.renderable_content();
        display_offset = content.display_offset;
        selection = content.selection;
        cursor = content.cursor;
        mode = content.mode;
        // The image cell to the left on the same row: one that leaves out
        // its row or column takes them from there.
        let mut left: Option<(i32, ImageCell)> = None;
        for indexed in content.display_iter {
            let row = indexed.point.line.0 + display_offset as i32;
            if row < 0 || row as usize >= screen_lines {
                continue;
            }
            let cell = &indexed.cell;
            let image = placeholder::decode(
                cell.c,
                cell.zerowidth(),
                cell.fg,
                cell.underline_color(),
                left.filter(|(left_row, _)| *left_row == row)
                    .map(|(_, cell)| cell),
            );
            left = image.map(|image| (row, image));
            rows[row as usize].push(match image {
                Some(_) => CellSnap {
                    c: ' ',
                    zerowidth: None,
                    // Its real colour is the image id, not a colour.
                    fg: AnsiColor::Named(NamedColor::Foreground),
                    bg: cell.bg,
                    flags: cell.flags,
                    image,
                },
                None => CellSnap {
                    c: cell.c,
                    zerowidth: cell.zerowidth().map(|z| z.to_vec()),
                    fg: cell.fg,
                    bg: cell.bg,
                    flags: cell.flags,
                    image: None,
                },
            });
        }
        cursor_style = term.cursor_style();
        drop(term);
    }

    pane.last_layout = Some(super::LastLayout {
        bounds,
        cell_width,
        cell_height,
        display_offset,
    });

    // --- Images: one sprite per rectangle of a placement's cells. ---
    pane.images.begin_frame();
    for run in image_runs(&rows) {
        let Some((image, image_size, spec)) = pane.images.lookup(&run.first) else {
            // Still decoding, failed, or deleted: the cells stay blank.
            continue;
        };
        // Worked out in device pixels, the unit the image was placed in.
        let (whole, shown) =
            images::layout(&spec, image_size, (cell_width * scale, cell_height * scale));
        // The run's corner, less how far into the box its first cell is.
        let box_x = origin.x + px((run.col as f32 - run.first.col as f32) * cell_width);
        let box_y = origin.y + px((run.row as f32 - run.first.row as f32) * cell_height);
        let in_box = |[x, y, w, h]: [f32; 4]| Bounds {
            origin: point(box_x + px(x / scale), box_y + px(y / scale)),
            size: size(px(w / scale), px(h / scale)),
        };
        let cells = Bounds {
            origin: point(
                origin.x + px(run.col as f32 * cell_width),
                origin.y + px(run.row as f32 * cell_height),
            ),
            size: size(
                px(run.cols as f32 * cell_width),
                px(run.rows as f32 * cell_height),
            ),
        };
        layout.images.push(ImagePaint {
            image,
            bounds: in_box(whole),
            clip: in_box(shown).intersect(&cells),
        });
    }

    // --- Shape rows (with a per-frame cache) and build quads. ---
    pane.prev_shape_cache = std::mem::take(&mut pane.shape_cache);

    let default_bg = theme.background;
    for (row_idx, row) in rows.iter().enumerate() {
        let row_y = origin.y + px(row_idx as f32 * cell_height);

        // Background + selection quads, coalescing adjacent same-color cells.
        let mut col = 0usize;
        let mut open_bg: Option<(usize, usize, Hsla)> = None; // (start, end_exclusive, color)
        let mut open_sel: Option<(usize, usize, Hsla)> = None;
        for cell in row {
            let width = if cell.flags.contains(Flags::WIDE_CHAR) {
                2
            } else {
                1
            };
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let mut fg = resolve(cell.fg, &theme);
            let mut bg = resolve(cell.bg, &theme);
            if cell.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if color_key(bg) != color_key(default_bg) {
                open_bg = match open_bg {
                    Some((start, end, color))
                        if end == col && color_key(color) == color_key(bg) =>
                    {
                        Some((start, col + width, color))
                    }
                    Some((start, end, color)) => {
                        layout.bg_quads.push(cell_run_quad(
                            origin,
                            row_y,
                            start,
                            end,
                            cell_width,
                            cell_height,
                            color,
                        ));
                        Some((col, col + width, bg))
                    }
                    None => Some((col, col + width, bg)),
                };
            } else if let Some((start, end, color)) = open_bg.take() {
                layout.bg_quads.push(cell_run_quad(
                    origin,
                    row_y,
                    start,
                    end,
                    cell_width,
                    cell_height,
                    color,
                ));
            }

            let grid_point = GridPoint::new(
                alacritty_terminal::index::Line(row_idx as i32 - display_offset as i32),
                alacritty_terminal::index::Column(col),
            );
            let selected = selection.is_some_and(|r| r.contains(grid_point));
            if selected {
                // The selection is painted over images; opaque, it would
                // hide the picture for as long as it's selected.
                let sel = match cell.image {
                    Some(_) => theme.selection_bg.opacity(0.4),
                    None => theme.selection_bg,
                };
                open_sel = match open_sel {
                    Some((start, end, color))
                        if end == col && color_key(color) == color_key(sel) =>
                    {
                        Some((start, col + width, color))
                    }
                    Some((start, end, color)) => {
                        layout.selection_quads.push(cell_run_quad(
                            origin,
                            row_y,
                            start,
                            end,
                            cell_width,
                            cell_height,
                            color,
                        ));
                        Some((col, col + width, sel))
                    }
                    None => Some((col, col + width, sel)),
                };
            } else if let Some((start, end, color)) = open_sel.take() {
                layout.selection_quads.push(cell_run_quad(
                    origin,
                    row_y,
                    start,
                    end,
                    cell_width,
                    cell_height,
                    color,
                ));
            }
            col += width;
        }
        if let Some((start, end, color)) = open_bg {
            layout.bg_quads.push(cell_run_quad(
                origin,
                row_y,
                start,
                end,
                cell_width,
                cell_height,
                color,
            ));
        }
        if let Some((start, end, color)) = open_sel {
            layout.selection_quads.push(cell_run_quad(
                origin,
                row_y,
                start,
                end,
                cell_width,
                cell_height,
                color,
            ));
        }

        // Text runs: coalesce consecutive cells sharing style.
        let selected_line = alacritty_terminal::index::Line(row_idx as i32 - display_offset as i32);
        let shaped = shape_row(
            pane,
            row,
            &theme,
            font_size,
            cell_width,
            &text_system,
            selection.as_ref(),
            selected_line,
        );
        if let Some(shaped) = shaped {
            layout.lines.push((row_idx, shaped));
        }
    }

    // --- cmd-hover underline. ---
    if let Some(span) = pane.hover
        && span.row < screen_lines
        && span.end > span.start
    {
        let y = origin.y + px((span.row + 1) as f32 * cell_height - 1.5);
        layout.selection_quads.push(fill(
            Bounds {
                origin: point(origin.x + px(span.start as f32 * cell_width), y),
                size: size(px((span.end - span.start) as f32 * cell_width), px(1.0)),
            },
            theme.foreground,
        ));
    }

    // --- Cursor. ---
    let cursor_row = cursor.point.line.0 + display_offset as i32;
    let cursor_on_screen = cursor_row >= 0 && (cursor_row as usize) < screen_lines;
    let vi_mode = mode.contains(TermMode::VI);
    let cursor_config = &pane.config().cursor;
    let unfocused_hidden = !focused && cursor_config.unfocused == UnfocusedCursor::Hidden;
    if (mode.contains(TermMode::SHOW_CURSOR) || vi_mode)
        && cursor_on_screen
        && pane.child_exited.is_none()
        && !unfocused_hidden
    {
        let row_idx = cursor_row as usize;
        let col = cursor.point.column.0;
        let cell = rows.get(row_idx).and_then(|r| {
            let mut c = 0usize;
            for snap in r {
                if snap.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                let width = if snap.flags.contains(Flags::WIDE_CHAR) {
                    2
                } else {
                    1
                };
                if col >= c && col < c + width {
                    return Some((snap, c));
                }
                c += width;
            }
            None
        });
        let wide = cell.is_some_and(|(snap, _)| snap.flags.contains(Flags::WIDE_CHAR));
        let width_cells = if wide { 2.0 } else { 1.0 };
        // The vi cursor is always a block, never blinks, and stays visible
        // unfocused, so the copy-mode position is never in doubt.
        let shape = if vi_mode {
            CursorShape::Block
        } else if focused || cursor_config.unfocused == UnfocusedCursor::Solid {
            cursor.shape
        } else {
            CursorShape::HollowBlock
        };
        let shape =
            match cursor_style.blinking && focused && !vi_mode && shape != CursorShape::Hidden {
                true if !pane.blink_show => CursorShape::Hidden,
                _ => shape,
            };
        let thickness = px((cursor_config.thickness * cell_width).max(1.0).round());
        let cursor_color = if vi_mode { theme.ansi[3] } else { theme.cursor };
        let cursor_bounds = Bounds {
            origin: point(
                origin.x + px(col as f32 * cell_width),
                origin.y + px(row_idx as f32 * cell_height),
            ),
            size: size(px(cell_width * width_cells), px(cell_height)),
        };
        let glyph = cell.and_then(|(snap, _)| {
            if shape != CursorShape::Block || snap.c == ' ' {
                return None;
            }
            let mut text = String::new();
            text.push(display_char(snap.c));
            if let Some(zw) = &snap.zerowidth {
                text.extend(zw.iter());
            }
            let run = TextRun {
                len: text.len(),
                font: base_font(
                    pane,
                    snap.flags.contains(Flags::BOLD),
                    snap.flags.contains(Flags::ITALIC),
                ),
                color: theme.background,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            Some(text_system.shape_line(SharedString::from(text), font_size, &[run], None))
        });
        layout.cursor = Some(CursorLayout {
            bounds: cursor_bounds,
            shape,
            color: cursor_color,
            thickness,
            glyph,
        });
    }

    layout
}

/// Gather the image cells on screen into rectangles: along each row while
/// the cells are consecutive columns of the same image row, then down while
/// the next row holds the same columns of the next image row. An image that
/// is whole and on screen comes out as one.
fn image_runs(rows: &[Vec<CellSnap>]) -> Vec<ImageRun> {
    // Add a finished row's run, or grow the run it continues.
    fn close(runs: &mut Vec<ImageRun>, above: &[usize], here: &mut Vec<usize>, run: ImageRun) {
        let continued = above.iter().copied().find(|&ix| {
            let top = &runs[ix];
            (top.col, top.cols) == (run.col, run.cols)
                && (top.first.image, top.first.placement, top.first.col)
                    == (run.first.image, run.first.placement, run.first.col)
                && top.first.row as usize + top.rows == run.first.row as usize
        });
        match continued {
            Some(ix) => {
                runs[ix].rows += 1;
                here.push(ix);
            }
            None => {
                here.push(runs.len());
                runs.push(run);
            }
        }
    }

    let mut runs: Vec<ImageRun> = Vec::new();
    // The runs that reach down to the previous row; only they can continue.
    let mut above: Vec<usize> = Vec::new();
    for (row_idx, row) in rows.iter().enumerate() {
        let mut here = Vec::new();
        let mut open: Option<ImageRun> = None;
        let mut col = 0usize;
        for cell in row {
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let continues = |run: &ImageRun, cell: &ImageCell| {
                (cell.image, cell.placement, cell.row)
                    == (run.first.image, run.first.placement, run.first.row)
                    && cell.col as usize == run.first.col as usize + run.cols
            };
            match (&mut open, cell.image) {
                (Some(run), Some(image)) if continues(run, &image) => run.cols += 1,
                (_, image) => {
                    if let Some(run) = open.take() {
                        close(&mut runs, &above, &mut here, run);
                    }
                    open = image.map(|first| ImageRun {
                        first,
                        row: row_idx,
                        col,
                        cols: 1,
                        rows: 1,
                    });
                }
            }
            col += if cell.flags.contains(Flags::WIDE_CHAR) {
                2
            } else {
                1
            };
        }
        if let Some(run) = open {
            close(&mut runs, &above, &mut here, run);
        }
        above = here;
    }
    runs
}

fn cell_run_quad(
    origin: Point<Pixels>,
    row_y: Pixels,
    start: usize,
    end: usize,
    cell_width: f32,
    cell_height: f32,
    color: Hsla,
) -> PaintQuad {
    fill(
        Bounds {
            origin: point(origin.x + px(start as f32 * cell_width), row_y),
            size: size(px((end - start) as f32 * cell_width), px(cell_height)),
        },
        color,
    )
}

fn shape_row(
    pane: &mut TerminalPane,
    row: &[CellSnap],
    theme: &Theme,
    font_size: Pixels,
    cell_width: f32,
    text_system: &std::sync::Arc<gpui::WindowTextSystem>,
    selection: Option<&SelectionRange>,
    line: alacritty_terminal::index::Line,
) -> Option<ShapedLine> {
    // Trim trailing default-styled blanks so we don't shape padding.
    let last = row.iter().rposition(|cell| {
        !(display_char(cell.c) == ' '
            && cell.zerowidth.is_none()
            && !cell.flags.intersects(
                Flags::INVERSE
                    | Flags::UNDERLINE
                    | Flags::DOUBLE_UNDERLINE
                    | Flags::UNDERCURL
                    | Flags::STRIKEOUT,
            ))
    })?;

    let mut text = String::new();
    let mut runs: Vec<TextRun> = Vec::new();
    // Each cell's first byte in `text` and its column, for snap_to_grid.
    let mut cells: Vec<(usize, usize)> = Vec::with_capacity(last + 1);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    f32::from(font_size).to_bits().hash(&mut hasher);
    cell_width.to_bits().hash(&mut hasher);

    // Only a configured selection_fg recolours selected text; otherwise the
    // selection is just the background quad and the cache key is untouched.
    let selection_fg = theme.selection_fg.zip(selection);
    let mut col = 0usize;
    for cell in &row[..=last] {
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let width = if cell.flags.contains(Flags::WIDE_CHAR) {
            2
        } else {
            1
        };
        let cell_col = col;
        col += width;
        let mut fg = resolve(cell.fg, theme);
        let mut bg = resolve(cell.bg, theme);
        // Brighten bold indexed colors 0-7 to 8-15.
        if cell.flags.contains(Flags::BOLD) {
            if let AnsiColor::Indexed(i @ 0..=7) = cell.fg {
                fg = theme.ansi[i as usize + 8];
            } else if let AnsiColor::Named(named) = cell.fg {
                let idx = named as usize;
                if idx < 8 {
                    fg = theme.ansi[idx + 8];
                }
            }
        }
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if cell.flags.contains(Flags::HIDDEN) {
            fg = bg;
        }
        if cell.flags.contains(Flags::DIM) {
            fg = blend(fg, bg, 0.4);
        }
        if let Some((sel_fg, range)) = selection_fg
            && range.contains(GridPoint::new(
                line,
                alacritty_terminal::index::Column(cell_col),
            ))
        {
            fg = sel_fg;
        }

        let bold = cell.flags.contains(Flags::BOLD);
        let italic = cell.flags.contains(Flags::ITALIC);
        let underline = if cell
            .flags
            .intersects(Flags::UNDERLINE | Flags::DOUBLE_UNDERLINE | Flags::UNDERCURL)
        {
            Some(UnderlineStyle {
                thickness: px(1.0),
                color: Some(fg),
                wavy: cell.flags.contains(Flags::UNDERCURL),
            })
        } else {
            None
        };
        let strikethrough = if cell.flags.contains(Flags::STRIKEOUT) {
            Some(StrikethroughStyle {
                thickness: px(1.0),
                color: Some(fg),
            })
        } else {
            None
        };

        let start_len = text.len();
        cells.push((start_len, cell_col));
        text.push(display_char(cell.c));
        if let Some(zw) = &cell.zerowidth {
            text.extend(zw.iter());
        }
        let added = text.len() - start_len;

        let style_key = (
            color_key(fg),
            bold,
            italic,
            underline.is_some(),
            cell.flags.contains(Flags::UNDERCURL),
            strikethrough.is_some(),
        );
        match runs.last_mut() {
            Some(run)
                if color_key(run.color) == style_key.0
                    && run.font.weight
                        == if bold {
                            FontWeight::BOLD
                        } else {
                            base_font(pane, false, italic).weight
                        }
                    && (run.font.style == FontStyle::Italic) == italic
                    && run.underline.is_some() == underline.is_some()
                    && run.strikethrough.is_some() == strikethrough.is_some() =>
            {
                run.len += added;
            }
            _ => {
                runs.push(TextRun {
                    len: added,
                    font: base_font(pane, bold, italic),
                    color: fg,
                    background_color: None,
                    underline,
                    strikethrough,
                });
            }
        }
        style_key.hash(&mut hasher);
    }

    if text.trim_end().is_empty()
        && runs
            .iter()
            .all(|r| r.underline.is_none() && r.strikethrough.is_none())
    {
        // A row of plain spaces with non-default colors still got bg quads;
        // nothing to shape.
        if row[..=last].iter().all(|c| display_char(c.c) == ' ') {
            return None;
        }
    }

    text.hash(&mut hasher);
    let key = hasher.finish();
    if let Some(line) = pane.prev_shape_cache.remove(&key) {
        pane.shape_cache.insert(key, line.clone());
        return Some(line);
    }
    if let Some(line) = pane.shape_cache.get(&key) {
        return Some(line.clone());
    }
    let mut line = text_system.shape_line(SharedString::from(text), font_size, &runs, None);
    snap_to_grid(&mut line, &cells, col, cell_width);
    pane.shape_cache.insert(key, line.clone());
    Some(line)
}

/// Pin every glyph to its cell. Shaping places each glyph after the advance
/// of the one before, so a glyph that isn't exactly a cell wide (braille or a
/// symbol from a fallback font, a CJK glyph that isn't quite two cells)
/// carries the rest of the row off the grid while the background quads stay
/// put. GPUI's `force_width` can't do this: it counts glyphs, not columns, so
/// a wide character or a combining mark would throw off everything after it.
fn snap_to_grid(line: &mut ShapedLine, cells: &[(usize, usize)], cols: usize, cell_width: f32) {
    let glyphs = || line.runs.iter().flat_map(|run| run.glyphs.iter());
    let xs = grid_positions(
        glyphs().map(|glyph| (glyph.index, f32::from(glyph.position.x))),
        cells,
        cell_width,
    );
    if glyphs()
        .zip(&xs)
        .all(|(glyph, &x)| (f32::from(glyph.position.x) - x).abs() < 0.01)
    {
        // Already on the grid, the usual case: keep the shared layout.
        return;
    }
    let mut runs = line.runs.clone();
    for (glyph, x) in runs
        .iter_mut()
        .flat_map(|run| run.glyphs.iter_mut())
        .zip(xs)
    {
        glyph.position.x = px(x);
    }
    let layout = LineLayout {
        font_size: line.font_size,
        width: px(cols as f32 * cell_width),
        ascent: line.ascent,
        descent: line.descent,
        runs,
        len: line.len,
    };
    **line = Arc::new(layout);
}

/// Where each glyph goes: the left edge of its cell. `glyphs` is each
/// glyph's byte index into the row's text and its shaped x, in paint order;
/// `cells` is each cell's first byte and column. Later glyphs in a cell
/// (combining marks) move with the first, so they stay over their base.
fn grid_positions(
    glyphs: impl Iterator<Item = (usize, f32)>,
    cells: &[(usize, usize)],
    cell_width: f32,
) -> Vec<f32> {
    let mut cell = usize::MAX;
    let mut shift = 0.0;
    glyphs
        .map(|(index, x)| {
            let i = cells
                .partition_point(|&(start, _)| start <= index)
                .saturating_sub(1);
            if i != cell {
                cell = i;
                shift = cells
                    .get(i)
                    .map_or(0.0, |&(_, col)| col as f32 * cell_width - x);
            }
            x + shift
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A screen from a picture of it: a digit is the cell in that column
    /// of image 1's row (the screen row less `shift`), a letter is text.
    fn screen(lines: &[&str], shift: usize) -> Vec<Vec<CellSnap>> {
        lines
            .iter()
            .enumerate()
            .map(|(row, line)| {
                line.chars()
                    .map(|c| CellSnap {
                        c: ' ',
                        zerowidth: None,
                        fg: AnsiColor::Named(NamedColor::Foreground),
                        bg: AnsiColor::Named(NamedColor::Background),
                        flags: Flags::empty(),
                        image: c.to_digit(10).map(|col| ImageCell {
                            image: 1,
                            placement: 1,
                            row: (row - shift) as u16,
                            col: col as u16,
                        }),
                    })
                    .collect()
            })
            .collect()
    }

    fn run(first: (u16, u16), row: usize, col: usize, cols: usize, rows: usize) -> ImageRun {
        ImageRun {
            first: ImageCell {
                image: 1,
                placement: 1,
                row: first.0,
                col: first.1,
            },
            row,
            col,
            cols,
            rows,
        }
    }

    #[test]
    fn a_whole_image_is_one_run_and_a_broken_one_is_its_pieces() {
        // Three columns by three rows, one cell in from the left, starting
        // on the second screen row.
        let whole = screen(&["a    ", " 012 ", " 012 ", " 012b"], 1);
        assert_eq!(image_runs(&whole), vec![run((0, 0), 1, 1, 3, 3)]);

        // Text over the middle cell splits that row, and the rows around
        // it no longer share its columns.
        let broken = screen(&[" 012 ", " 0x2 ", " 012 "], 0);
        assert_eq!(
            image_runs(&broken),
            vec![
                run((0, 0), 0, 1, 3, 1),
                run((1, 0), 1, 1, 1, 1),
                run((1, 2), 1, 3, 1, 1),
                run((2, 0), 2, 1, 3, 1),
            ]
        );

        // Scrolled half off the top: what remains starts at image row 1.
        let mut scrolled = screen(&["x", " 012 ", " 012 "], 0);
        scrolled.remove(0);
        assert_eq!(image_runs(&scrolled), vec![run((1, 0), 0, 1, 3, 2)]);
    }

    #[test]
    fn control_chars_render_as_single_cell_blanks() {
        // macOS `ls` pads columns with TABs; shaping a real tab glyph would
        // advance by the font's tab width instead of one cell.
        assert_eq!(display_char('\t'), ' ');
        assert_eq!(display_char('\r'), ' ');
        assert_eq!(display_char('\u{0}'), ' ');
        assert_eq!(display_char('\u{7f}'), ' ');
        // Printable characters, including wide and combining ones, pass through.
        assert_eq!(display_char('a'), 'a');
        assert_eq!(display_char('漢'), '漢');
        assert_eq!(display_char('\u{e0b0}'), '\u{e0b0}');
    }

    #[test]
    fn glyphs_sit_in_their_cells() {
        // "⣿⣿.|" with fallback braille 10px wide in an 8px cell: each braille
        // glyph used to push the rest of the row 2px further right.
        let cells = [(0, 0), (3, 1), (6, 2), (7, 3)];
        let shaped = [(0, 0.0), (3, 10.0), (6, 20.0), (7, 28.0)];
        assert_eq!(
            grid_positions(shaped.into_iter(), &cells, 8.0),
            [0.0, 8.0, 16.0, 24.0]
        );

        // "ae\u{301}中b": a wide glyph takes two columns, and the accent
        // keeps its offset from the 'e' it sits on.
        let cells = [(0, 0), (1, 1), (4, 2), (7, 4)];
        let shaped = [(0, 0.0), (1, 9.0), (2, 9.5), (4, 17.0), (7, 34.0)];
        assert_eq!(
            grid_positions(shaped.into_iter(), &cells, 8.0),
            [0.0, 8.0, 8.5, 16.0, 32.0]
        );
    }
}
