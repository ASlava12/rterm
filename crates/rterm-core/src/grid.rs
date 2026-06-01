//! Cell grid model: cells, attributes, fixed-size grid.

use bitflags::bitflags;

use crate::color::Color;

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct CellAttrs: u16 {
        const BOLD             = 1 << 0;
        const DIM              = 1 << 1;
        const ITALIC           = 1 << 2;
        const UNDERLINE        = 1 << 3;
        const BLINK            = 1 << 4;
        const REVERSE          = 1 << 5;
        const HIDDEN           = 1 << 6;
        const STRIKETHROUGH    = 1 << 7;
        const WIDE             = 1 << 8;
        const WIDE_SPACER      = 1 << 9;
        /// Combined with UNDERLINE to indicate a *double* underline
        /// (SGR 21 or SGR 4:2). When this bit is set, UNDERLINE is
        /// also set so existing single-underline rendering still works
        /// as a fallback on renderers that ignore the style bit.
        const UNDERLINE_DOUBLE = 1 << 10;
        /// Curly / wavy underline (SGR 4:3). Pairs with UNDERLINE.
        const UNDERLINE_CURLY  = 1 << 11;
        /// SGR 53 — line drawn ABOVE the cell. Cleared by SGR 55.
        const OVERLINE         = 1 << 12;
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub attrs: CellAttrs,
    /// OSC 8 hyperlink id; `0` = no link. The string-URI lookup lives on
    /// `Terminal` so that cells stay 16 bytes wide.
    pub hyperlink: u32,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: Color::Default,
            bg: Color::Default,
            attrs: CellAttrs::empty(),
            hyperlink: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Position {
    pub col: u16,
    pub row: u16,
}

#[derive(Debug, Clone)]
pub struct Grid {
    size: Size,
    cells: Vec<Cell>,
    /// Per-row soft-wrap flag: `line_wrapped[r]` is true when row `r`
    /// filled to the right margin via autowrap and its text continues
    /// on row `r + 1` with no hard newline between them. Copy uses this
    /// to round-trip a wrapped line as ONE logical line instead of
    /// splicing a spurious `\n` at every visual row boundary.
    line_wrapped: Vec<bool>,
}

impl Grid {
    pub fn new(size: Size) -> Self {
        let cells = vec![Cell::default(); size.cols as usize * size.rows as usize];
        let line_wrapped = vec![false; size.rows as usize];
        Self { size, cells, line_wrapped }
    }

    /// Whether row `r` soft-wraps into `r + 1` (see `line_wrapped`).
    pub fn is_wrapped(&self, row: u16) -> bool {
        self.line_wrapped.get(row as usize).copied().unwrap_or(false)
    }

    /// Mark (or clear) row `r`'s soft-wrap flag. Out-of-range is a no-op.
    pub fn set_wrapped(&mut self, row: u16, wrapped: bool) {
        if let Some(w) = self.line_wrapped.get_mut(row as usize) {
            *w = wrapped;
        }
    }

    pub fn size(&self) -> Size {
        self.size
    }

    pub fn cell(&self, pos: Position) -> Option<&Cell> {
        self.cells.get(self.index(pos)?)
    }

    pub fn cell_mut(&mut self, pos: Position) -> Option<&mut Cell> {
        let idx = self.index(pos)?;
        self.cells.get_mut(idx)
    }

    pub fn resize(&mut self, new_size: Size) {
        let mut new_cells = vec![Cell::default(); new_size.cols as usize * new_size.rows as usize];
        let copy_cols = self.size.cols.min(new_size.cols) as usize;
        let copy_rows = self.size.rows.min(new_size.rows) as usize;
        let old_cols = self.size.cols as usize;
        let new_cols = new_size.cols as usize;
        // Copy one row at a time via `copy_from_slice` (lowers to a
        // single `memcpy`) instead of the cell-by-cell nested loop.
        for row in 0..copy_rows {
            let old_start = row * old_cols;
            let new_start = row * new_cols;
            new_cells[new_start..new_start + copy_cols]
                .copy_from_slice(&self.cells[old_start..old_start + copy_cols]);
        }
        // Carry wrap flags for surviving rows; new rows start unwrapped.
        let mut new_wrapped = vec![false; new_size.rows as usize];
        new_wrapped[..copy_rows].copy_from_slice(&self.line_wrapped[..copy_rows]);
        self.line_wrapped = new_wrapped;
        self.size = new_size;
        self.cells = new_cells;
    }

    pub fn clear(&mut self) {
        self.cells.fill(Cell::default());
        self.line_wrapped.fill(false);
    }

    /// Iterate row `row` as a slice. None if out of bounds.
    pub fn row(&self, row: u16) -> Option<&[Cell]> {
        if row >= self.size.rows {
            return None;
        }
        let cols = self.size.cols as usize;
        let start = row as usize * cols;
        Some(&self.cells[start..start + cols])
    }

    /// Mutable row slice. None if out of bounds.
    ///
    /// Lets callers shift / blank a whole row in-place via slice ops
    /// (`copy_within`, `fill`) instead of allocating an intermediate
    /// `Vec` and writing back cell-by-cell through `cell_mut`.
    pub fn row_mut(&mut self, row: u16) -> Option<&mut [Cell]> {
        if row >= self.size.rows {
            return None;
        }
        let cols = self.size.cols as usize;
        let start = row as usize * cols;
        Some(&mut self.cells[start..start + cols])
    }

    /// Scroll the contents in `[top, bottom]` (inclusive) up by `n` rows.
    /// Vacated rows at the bottom are blanked with `blank`. Returns the rows
    /// that fell off the top, in scroll order (oldest first); callers can push
    /// these into a scrollback buffer.
    pub fn scroll_up(&mut self, top: u16, bottom: u16, n: u16, blank: Cell) -> Vec<Vec<Cell>> {
        if n == 0 || top > bottom || bottom >= self.size.rows {
            return Vec::new();
        }
        let cols = self.size.cols as usize;
        let region_rows = (bottom - top + 1) as usize;
        let shift = (n as usize).min(region_rows);
        let top_row = top as usize;

        // The evicted rows, the surviving block, and the blanked tail
        // each occupy contiguous slices of `cells`, so we can use one
        // chunk / `copy_within` / `fill` per region — no per-row loop.
        let evict_start = top_row * cols;
        let evict_end = evict_start + shift * cols;
        let evicted: Vec<Vec<Cell>> = self.cells[evict_start..evict_end]
            .chunks_exact(cols)
            .map(<[Cell]>::to_vec)
            .collect();

        let survive_len = (region_rows - shift) * cols;
        if survive_len > 0 {
            let src_start = evict_end;
            self.cells.copy_within(src_start..src_start + survive_len, evict_start);
        }

        let blank_start = evict_start + survive_len;
        let blank_end = blank_start + shift * cols;
        self.cells[blank_start..blank_end].fill(blank);

        // Shift wrap flags in lockstep with the cells: region survivors
        // move up by `shift`, the freed tail resets to unwrapped.
        let survive_rows = region_rows - shift;
        for i in 0..survive_rows {
            self.line_wrapped[top_row + i] = self.line_wrapped[top_row + i + shift];
        }
        for i in 0..shift {
            self.line_wrapped[top_row + survive_rows + i] = false;
        }

        evicted
    }

    /// Scroll down (insert blank rows at top of region). Returns nothing —
    /// content shifted past `bottom` is discarded; scrollback only grows on up.
    pub fn scroll_down(&mut self, top: u16, bottom: u16, n: u16, blank: Cell) {
        if n == 0 || top > bottom || bottom >= self.size.rows {
            return;
        }
        let cols = self.size.cols as usize;
        let region_rows = (bottom - top + 1) as usize;
        let shift = (n as usize).min(region_rows);
        let top_row = top as usize;

        // Move the surviving block down in a single `copy_within` and
        // blank the freed top in a single `fill`. `copy_within` handles
        // the overlapping back-to-front move internally.
        let region_start = top_row * cols;
        let blank_end = region_start + shift * cols;
        let survive_len = (region_rows - shift) * cols;
        if survive_len > 0 {
            self.cells
                .copy_within(region_start..region_start + survive_len, blank_end);
        }
        self.cells[region_start..blank_end].fill(blank);

        // Mirror the move for wrap flags: survivors shift down by
        // `shift`, the inserted top rows reset to unwrapped.
        let survive_rows = region_rows - shift;
        for i in (0..survive_rows).rev() {
            self.line_wrapped[top_row + shift + i] = self.line_wrapped[top_row + i];
        }
        for i in 0..shift {
            self.line_wrapped[top_row + i] = false;
        }
    }

    fn index(&self, pos: Position) -> Option<usize> {
        if pos.col >= self.size.cols || pos.row >= self.size.rows {
            return None;
        }
        Some(pos.row as usize * self.size.cols as usize + pos.col as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_and_size_stay_compact() {
        // Position is `{row: u16, col: u16}` = 4 bytes. Size has
        // the same shape. These end up packed into hot paths
        // (selection rects, grid index calcs); pinning size
        // catches an accidental upgrade to u32 / usize.
        assert_eq!(std::mem::size_of::<Position>(), 4);
        assert_eq!(std::mem::size_of::<Size>(), 4);
    }

    #[test]
    fn resize_preserves_overlap_blanks_growth() {
        // Seed 3×3 with unique chars so we can spot mis-copies.
        let mut g = Grid::new(Size { cols: 3, rows: 3 });
        for r in 0..3u16 {
            for c in 0..3u16 {
                let cell = g.cell_mut(Position { col: c, row: r }).unwrap();
                cell.ch = char::from_u32(b'a' as u32 + (r * 3 + c) as u32).unwrap();
            }
        }
        // Grow cols, shrink rows: overlap = 2 rows × 3 cols.
        g.resize(Size { cols: 5, rows: 2 });
        // Originals stay in place at their (row, col).
        for r in 0..2u16 {
            for c in 0..3u16 {
                let expected = char::from_u32(b'a' as u32 + (r * 3 + c) as u32).unwrap();
                assert_eq!(
                    g.cell(Position { col: c, row: r }).unwrap().ch,
                    expected,
                    "lost cell at ({c},{r})"
                );
            }
        }
        // New columns are blanks (space).
        for r in 0..2u16 {
            for c in 3..5u16 {
                assert_eq!(g.cell(Position { col: c, row: r }).unwrap().ch, ' ');
            }
        }
    }

    #[test]
    fn row_mut_writes_visible_through_row() {
        let mut g = Grid::new(Size { cols: 3, rows: 2 });
        // Write via row_mut, read back via row.
        {
            let r = g.row_mut(1).unwrap();
            assert_eq!(r.len(), 3);
            r[0].ch = 'x';
            r[2].ch = 'z';
        }
        let r = g.row(1).unwrap();
        assert_eq!(r[0].ch, 'x');
        assert_eq!(r[1].ch, ' ');
        assert_eq!(r[2].ch, 'z');
        // Untouched row stays default.
        assert_eq!(g.row(0).unwrap()[0].ch, ' ');
        // Out-of-bounds row returns None.
        assert!(g.row_mut(2).is_none());
    }

    #[test]
    fn scroll_up_blanks_vacated_rows_and_evicts_top() {
        let mut g = Grid::new(Size { cols: 2, rows: 3 });
        for r in 0..3u16 {
            for c in 0..2u16 {
                g.cell_mut(Position { col: c, row: r }).unwrap().ch =
                    char::from_u32(b'a' as u32 + (r * 2 + c) as u32).unwrap();
            }
        }
        let blank = Cell { ch: '.', ..Cell::default() };
        let evicted = g.scroll_up(0, 2, 1, blank);
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].iter().map(|c| c.ch).collect::<String>(), "ab");
        // Surviving rows shifted up.
        assert_eq!(g.row(0).unwrap().iter().map(|c| c.ch).collect::<String>(), "cd");
        assert_eq!(g.row(1).unwrap().iter().map(|c| c.ch).collect::<String>(), "ef");
        // Bottom row blanked with `blank`.
        assert_eq!(g.row(2).unwrap().iter().map(|c| c.ch).collect::<String>(), "..");
    }

    #[test]
    fn scroll_down_inserts_blanks_at_top() {
        let mut g = Grid::new(Size { cols: 2, rows: 3 });
        for r in 0..3u16 {
            for c in 0..2u16 {
                g.cell_mut(Position { col: c, row: r }).unwrap().ch =
                    char::from_u32(b'a' as u32 + (r * 2 + c) as u32).unwrap();
            }
        }
        let blank = Cell { ch: '.', ..Cell::default() };
        g.scroll_down(0, 2, 1, blank);
        // Top row blanked.
        assert_eq!(g.row(0).unwrap().iter().map(|c| c.ch).collect::<String>(), "..");
        // Previously top + middle rows shifted down by one.
        assert_eq!(g.row(1).unwrap().iter().map(|c| c.ch).collect::<String>(), "ab");
        assert_eq!(g.row(2).unwrap().iter().map(|c| c.ch).collect::<String>(), "cd");
    }

    #[test]
    fn scroll_up_full_region_blanks_everything() {
        // shift == region_rows: no row survives the scroll. The new
        // single-`fill` path has to cover the whole region without
        // tripping over a zero-length `copy_within`.
        let mut g = Grid::new(Size { cols: 2, rows: 3 });
        for r in 0..3u16 {
            for c in 0..2u16 {
                g.cell_mut(Position { col: c, row: r }).unwrap().ch =
                    char::from_u32(b'a' as u32 + (r * 2 + c) as u32).unwrap();
            }
        }
        let blank = Cell { ch: '.', ..Cell::default() };
        // Region is just rows 1..=2 (two rows), and we scroll by 5 —
        // clamps to region_rows so both rows are evicted + blanked.
        let evicted = g.scroll_up(1, 2, 5, blank);
        assert_eq!(evicted.len(), 2);
        assert_eq!(evicted[0].iter().map(|c| c.ch).collect::<String>(), "cd");
        assert_eq!(evicted[1].iter().map(|c| c.ch).collect::<String>(), "ef");
        // Row outside the region is untouched.
        assert_eq!(g.row(0).unwrap().iter().map(|c| c.ch).collect::<String>(), "ab");
        // The two scrolled rows are now blanks.
        assert_eq!(g.row(1).unwrap().iter().map(|c| c.ch).collect::<String>(), "..");
        assert_eq!(g.row(2).unwrap().iter().map(|c| c.ch).collect::<String>(), "..");
    }

    #[test]
    fn scroll_down_full_region_blanks_everything() {
        let mut g = Grid::new(Size { cols: 2, rows: 3 });
        for r in 0..3u16 {
            for c in 0..2u16 {
                g.cell_mut(Position { col: c, row: r }).unwrap().ch =
                    char::from_u32(b'a' as u32 + (r * 2 + c) as u32).unwrap();
            }
        }
        let blank = Cell { ch: '.', ..Cell::default() };
        g.scroll_down(0, 1, 5, blank);
        assert_eq!(g.row(0).unwrap().iter().map(|c| c.ch).collect::<String>(), "..");
        assert_eq!(g.row(1).unwrap().iter().map(|c| c.ch).collect::<String>(), "..");
        // Row outside the region kept.
        assert_eq!(g.row(2).unwrap().iter().map(|c| c.ch).collect::<String>(), "ef");
    }

    #[test]
    fn cell_struct_stays_compact() {
        // Memory cost of a grid is `cols * rows * size_of::<Cell>()`,
        // and the scrollback ring keeps thousands of rows worth of
        // cells per pane. The comment on `Cell::hyperlink` claims
        // cells stay 16 bytes wide — that's no longer literally
        // true (Color is an enum, adds discriminants) but
        // pinning a generous-but-finite ceiling guards against
        // an accidental `String`/`Vec` field that would bloat
        // every cell by 24+ bytes.
        let sz = std::mem::size_of::<Cell>();
        assert!(
            sz <= 32,
            "Cell grew to {sz} bytes — pin the bloat with intent",
        );
        // Lock the current packed layout so a stealth field add
        // (or a careless `#[repr]` swap) gets flagged. Update this
        // figure deliberately when intentionally widening the
        // cell. As of writing: ch(4) + fg(4) + bg(4) + attrs(2) +
        // pad(2) + hyperlink(4) = 20 bytes.
        assert_eq!(
            sz, 20,
            "Cell layout changed — update the config doc + memory \
             math (default.toml says ~cell_size bytes/cell)",
        );
    }
}
