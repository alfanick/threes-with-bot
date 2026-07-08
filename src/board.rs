use serde::{Deserialize, Serialize};

pub const SIZE: usize = 4;
pub const CELLS: usize = SIZE * SIZE;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

impl Direction {
    pub const ALL: [Self; 4] = [Self::Up, Self::Down, Self::Left, Self::Right];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::Left => "left",
            Self::Right => "right",
        }
    }

    pub fn mask(self) -> u8 {
        match self {
            Self::Up => 1 << 0,
            Self::Down => 1 << 1,
            Self::Left => 1 << 2,
            Self::Right => 1 << 3,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Board {
    cells: [u16; CELLS],
}

impl Default for Board {
    fn default() -> Self {
        Self::empty()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SlideResult {
    pub board: Board,
    pub moved_lines: [bool; SIZE],
    pub changed: bool,
    pub merged: bool,
    pub merged_cells: [bool; CELLS],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LineSlide {
    line: [u16; SIZE],
    changed: bool,
    merged: bool,
    merged_cells: [bool; SIZE],
}

impl Board {
    pub const fn empty() -> Self {
        Self { cells: [0; CELLS] }
    }

    pub const fn from_ranks(cells: [u16; CELLS]) -> Self {
        Self { cells }
    }

    pub fn ranks(&self) -> [u16; CELLS] {
        self.cells
    }

    pub fn face_values(&self) -> [u64; CELLS] {
        self.cells.map(rank_to_face)
    }

    pub fn get(&self, row: usize, col: usize) -> u16 {
        self.cells[index(row, col)]
    }

    pub fn set(&mut self, row: usize, col: usize, rank: u16) {
        self.cells[index(row, col)] = rank;
    }

    pub fn get_index(&self, idx: usize) -> u16 {
        self.cells[idx]
    }

    pub fn set_index(&mut self, idx: usize, rank: u16) {
        self.cells[idx] = rank;
    }

    pub fn high_rank(&self) -> u16 {
        self.cells.into_iter().max().unwrap_or(0)
    }

    pub fn high_face(&self) -> u64 {
        rank_to_face(self.high_rank())
    }

    pub fn score(&self) -> u64 {
        self.cells
            .into_iter()
            .map(rank_score)
            .fold(0u64, u64::saturating_add)
    }

    pub fn empty_indices(&self) -> Vec<usize> {
        self.cells
            .iter()
            .enumerate()
            .filter_map(|(idx, rank)| (*rank == 0).then_some(idx))
            .collect()
    }

    pub fn legal_moves_mask(&self) -> u8 {
        let mut mask = 0;
        for direction in Direction::ALL {
            if self.slide(direction).changed {
                mask |= direction.mask();
            }
        }
        mask
    }

    pub fn legal_directions(&self) -> Vec<Direction> {
        Direction::ALL
            .into_iter()
            .filter(|direction| self.slide(*direction).changed)
            .collect()
    }

    pub fn has_legal_move(&self) -> bool {
        self.legal_moves_mask() != 0
    }

    pub fn slide(&self, direction: Direction) -> SlideResult {
        let mut next = *self;
        let mut moved_lines = [false; SIZE];
        let mut changed = false;
        let mut merged = false;
        let mut merged_cells = [false; CELLS];

        for (line_idx, moved_line) in moved_lines.iter_mut().enumerate() {
            let line = self.line_front_to_back(direction, line_idx);
            let slide = slide_line_toward_front(line);
            if slide.changed {
                *moved_line = true;
                changed = true;
            }
            for (offset, merged_cell) in slide.merged_cells.into_iter().enumerate() {
                if merged_cell {
                    let idx = index_for_line(direction, line_idx, offset);
                    merged_cells[idx] = true;
                }
            }
            merged |= slide.merged;
            next.write_line_front_to_back(direction, line_idx, slide.line);
        }

        SlideResult {
            board: next,
            moved_lines,
            changed,
            merged,
            merged_cells,
        }
    }

    fn line_front_to_back(&self, direction: Direction, line_idx: usize) -> [u16; SIZE] {
        let mut line = [0; SIZE];
        for (offset, cell) in line.iter_mut().enumerate() {
            let (row, col) = coords_for(direction, line_idx, offset);
            *cell = self.get(row, col);
        }
        line
    }

    fn write_line_front_to_back(
        &mut self,
        direction: Direction,
        line_idx: usize,
        line: [u16; SIZE],
    ) {
        for (offset, rank) in line.into_iter().enumerate() {
            let (row, col) = coords_for(direction, line_idx, offset);
            self.set(row, col, rank);
        }
    }
}

pub fn rank_to_face(rank: u16) -> u64 {
    match rank {
        0 => 0,
        1 => 1,
        2 => 2,
        n => 3u64.checked_shl((n - 3).into()).unwrap_or(u64::MAX),
    }
}

pub fn rank_score(rank: u16) -> u64 {
    if rank < 3 {
        0
    } else {
        3u64.saturating_pow((rank - 2).into())
    }
}

pub fn face_to_rank(face: u64) -> Option<u16> {
    match face {
        0 => Some(0),
        1 => Some(1),
        2 => Some(2),
        3 => Some(3),
        n if n >= 6 && n % 3 == 0 => {
            let quotient = n / 3;
            quotient
                .is_power_of_two()
                .then(|| 3 + quotient.trailing_zeros() as u16)
        }
        _ => None,
    }
}

pub fn merge_rank(front: u16, back: u16) -> Option<u16> {
    match (front, back) {
        (1, 2) | (2, 1) => Some(3),
        (a, b) if a >= 3 && a == b => Some(a + 1),
        _ => None,
    }
}

pub fn index(row: usize, col: usize) -> usize {
    row * SIZE + col
}

pub fn row_col(idx: usize) -> (usize, usize) {
    (idx / SIZE, idx % SIZE)
}

fn coords_for(direction: Direction, line_idx: usize, offset: usize) -> (usize, usize) {
    match direction {
        Direction::Left => (line_idx, offset),
        Direction::Right => (line_idx, SIZE - 1 - offset),
        Direction::Up => (offset, line_idx),
        Direction::Down => (SIZE - 1 - offset, line_idx),
    }
}

fn slide_line_toward_front(line: [u16; SIZE]) -> LineSlide {
    let mut moving = [false; SIZE];
    let mut merged_cells = [false; SIZE];

    for idx in 1..SIZE {
        let rank = line[idx];
        if rank == 0 {
            continue;
        }
        let target = idx - 1;
        moving[idx] =
            line[target] == 0 || moving[target] || merge_rank(line[target], rank).is_some();
    }

    let mut output = [0; SIZE];
    let mut merged = false;

    for idx in 0..SIZE {
        let rank = line[idx];
        if rank == 0 {
            continue;
        }

        if moving[idx] {
            let target = idx - 1;
            if line[target] != 0 && !moving[target] {
                if let Some(merged_rank) = merge_rank(line[target], rank) {
                    output[target] = merged_rank;
                    merged_cells[target] = true;
                    merged = true;
                    continue;
                }
            }
            output[target] = rank;
        } else {
            output[idx] = rank;
        }
    }

    LineSlide {
        line: output,
        changed: output != line,
        merged,
        merged_cells,
    }
}

fn index_for_line(direction: Direction, line_idx: usize, offset: usize) -> usize {
    let (row, col) = coords_for(direction, line_idx, offset);
    index(row, col)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_after(line: [u16; SIZE]) -> [u16; SIZE] {
        slide_line_toward_front(line).line
    }

    #[test]
    fn encodes_faces_and_scores() {
        assert_eq!(rank_to_face(0), 0);
        assert_eq!(rank_to_face(1), 1);
        assert_eq!(rank_to_face(2), 2);
        assert_eq!(rank_to_face(3), 3);
        assert_eq!(rank_to_face(4), 6);
        assert_eq!(rank_to_face(9), 192);

        assert_eq!(rank_score(1), 0);
        assert_eq!(rank_score(2), 0);
        assert_eq!(rank_score(3), 3);
        assert_eq!(rank_score(4), 9);
        assert_eq!(rank_score(9), 2187);

        assert_eq!(face_to_rank(0), Some(0));
        assert_eq!(face_to_rank(1), Some(1));
        assert_eq!(face_to_rank(2), Some(2));
        assert_eq!(face_to_rank(3), Some(3));
        assert_eq!(face_to_rank(192), Some(9));
        assert_eq!(face_to_rank(5), None);
    }

    #[test]
    fn slides_one_cell_without_compressing() {
        assert_eq!(line_after([0, 1, 2, 0]), [1, 2, 0, 0]);
        assert_eq!(line_after([0, 3, 3, 0]), [3, 3, 0, 0]);
    }

    #[test]
    fn merges_against_stationary_front_tile() {
        assert_eq!(line_after([1, 2, 0, 0]), [3, 0, 0, 0]);
        assert_eq!(line_after([2, 1, 0, 0]), [3, 0, 0, 0]);
        assert_eq!(line_after([3, 3, 0, 0]), [4, 0, 0, 0]);
    }

    #[test]
    fn does_not_double_merge_in_one_slide() {
        assert_eq!(line_after([1, 2, 3, 0]), [3, 3, 0, 0]);
        assert_eq!(line_after([3, 3, 3, 3]), [4, 3, 3, 0]);
    }

    #[test]
    fn reports_legal_moves() {
        let blocked = Board::from_ranks([1, 3, 1, 3, 3, 1, 3, 1, 1, 3, 1, 3, 3, 1, 3, 1]);
        assert!(!blocked.has_legal_move());

        let mergeable = Board::from_ranks([1, 2, 1, 3, 3, 1, 3, 1, 1, 3, 1, 3, 3, 1, 3, 1]);
        assert!(mergeable.has_legal_move());
    }
}
