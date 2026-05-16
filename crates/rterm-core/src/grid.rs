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
}

impl Grid {
    pub fn new(size: Size) -> Self {
        let cells = vec![Cell::default(); size.cols as usize * size.rows as usize];
        Self { size, cells }
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
        self.size = new_size;
        self.cells = new_cells;
    }

    pub fn clear(&mut self) {
        self.cells.fill(Cell::default());
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

        let mut evicted = Vec::with_capacity(shift);
        for i in 0..shift {
            let row_idx = top as usize + i;
            let start = row_idx * cols;
            evicted.push(self.cells[start..start + cols].to_vec());
        }

        // Move surviving rows up.
        for i in 0..(region_rows - shift) {
            let src_start = (top as usize + i + shift) * cols;
            let dst_start = (top as usize + i) * cols;
            self.cells.copy_within(src_start..src_start + cols, dst_start);
        }

        // Blank the bottom `shift` rows.
        for i in (region_rows - shift)..region_rows {
            let row_idx = top as usize + i;
            let start = row_idx * cols;
            self.cells[start..start + cols].fill(blank);
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

        // Move surviving rows down, back to front.
        for i in (0..(region_rows - shift)).rev() {
            let src_start = (top as usize + i) * cols;
            let dst_start = (top as usize + i + shift) * cols;
            self.cells.copy_within(src_start..src_start + cols, dst_start);
        }

        for i in 0..shift {
            let row_idx = top as usize + i;
            let start = row_idx * cols;
            self.cells[start..start + cols].fill(blank);
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
    }
}
