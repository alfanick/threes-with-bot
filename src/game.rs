use crate::board::{index, rank_to_face, Board, Direction, CELLS, SIZE};
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};

const BASIC_DECK_SIZE: usize = 12;
const BONUS_CYCLE_SIZE: usize = 21;
const BONUS_LOWEST_RANK: u16 = 4;
const BONUS_UNLOCK_HIGH_RANK: u16 = 7; // 48
pub const DEFAULT_BONUS_FORECAST_HORIZON: u8 = 8;

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NextTile {
    Basic {
        #[serde(skip_serializing)]
        rank: u16,
        face: u64,
    },
    Bonus {
        #[serde(skip_serializing)]
        ranks: Vec<u16>,
        faces: Vec<u64>,
    },
}

impl NextTile {
    pub fn display_label(&self) -> String {
        match self {
            Self::Basic { face, .. } => face.to_string(),
            Self::Bonus { faces, .. } => {
                let labels = faces.iter().map(u64::to_string).collect::<Vec<_>>();
                format!("+ {}", labels.join("/"))
            }
        }
    }

    pub fn rank_signature(&self) -> Vec<u16> {
        match self {
            Self::Basic { rank, .. } => vec![*rank],
            Self::Bonus { ranks, .. } => ranks.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct SpawnEvent {
    pub row: usize,
    pub col: usize,
    #[serde(skip_serializing)]
    pub rank: u16,
    pub face: u64,
    pub source: SpawnSource,
    pub preview: NextTile,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SpawnSource {
    Basic,
    Bonus,
}

#[derive(Clone, Debug, Serialize)]
pub struct GameSnapshot {
    pub seed: u64,
    pub board: [u64; CELLS],
    pub score: u64,
    pub high_tile: u64,
    pub accepted_moves: u64,
    pub attempted_moves: u64,
    pub legal_moves_mask: u8,
    pub next_tile: NextTile,
    pub bonus_forecast: BonusForecast,
}

#[derive(Clone, Debug, Serialize)]
pub struct MoveResult {
    pub direction: Direction,
    pub accepted: bool,
    pub board_before: [u64; CELLS],
    pub board_after_slide: [u64; CELLS],
    pub board_after_spawn: [u64; CELLS],
    pub moved_lines: [bool; SIZE],
    pub spawn: Option<SpawnEvent>,
    pub score: u64,
    pub high_tile: u64,
    pub next_tile: NextTile,
    pub bonus_forecast: BonusForecast,
    pub game_over: bool,
}

#[derive(Clone)]
pub struct Game {
    seed: u64,
    rng: ChaCha8Rng,
    board: Board,
    draw: DrawState,
    bonus_tracker: BonusTracker,
    next_tile: NextTile,
    accepted_moves: u64,
    attempted_moves: u64,
}

#[derive(Clone)]
pub struct Outcome {
    pub probability: f64,
    pub direction: Direction,
    pub row: usize,
    pub col: usize,
    pub face: u64,
    pub board: [u64; CELLS],
    pub next_tile: NextTile,
    pub bonus_forecast: BonusForecast,
    pub score: u64,
    pub high_tile: u64,
    state: Game,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct BonusForecast {
    pub unlocked: bool,
    pub cycle_position: Option<u8>,
    pub bonus_seen_this_cycle: bool,
    pub slots: Vec<BonusForecastSlot>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq)]
pub struct BonusForecastSlot {
    pub accepted_moves_from_now: u8,
    pub probability: f64,
}

impl Outcome {
    pub fn game(&self) -> &Game {
        &self.state
    }
}

impl Game {
    pub fn new(seed: u64) -> Self {
        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        let mut draw = DrawState::new(&mut rng);
        let mut board = Board::empty();

        for _ in 0..9 {
            let rank = draw.draw_basic(&mut rng);
            let empty = board.empty_indices();
            let idx = empty[rng.gen_range(0..empty.len())];
            board.set_index(idx, rank);
        }

        let next_tile = draw.next_tile(&board, &mut rng);
        let mut bonus_tracker = BonusTracker::default();
        bonus_tracker.observe_revealed_next(board.high_rank(), &next_tile);

        Self {
            seed,
            rng,
            board,
            draw,
            bonus_tracker,
            next_tile,
            accepted_moves: 0,
            attempted_moves: 0,
        }
    }

    pub fn seed(&self) -> u64 {
        self.seed
    }

    pub fn board(&self) -> Board {
        self.board
    }

    pub fn score(&self) -> u64 {
        self.board.score()
    }

    pub fn high_tile(&self) -> u64 {
        self.board.high_face()
    }

    pub fn next_tile(&self) -> &NextTile {
        &self.next_tile
    }

    pub fn bonus_forecast(&self, horizon: u8) -> BonusForecast {
        self.bonus_tracker.forecast(horizon)
    }

    pub fn bonus_forecast_signature(&self) -> [u8; 3] {
        self.bonus_tracker.signature()
    }

    pub fn accepted_moves(&self) -> u64 {
        self.accepted_moves
    }

    pub fn attempted_moves(&self) -> u64 {
        self.attempted_moves
    }

    pub fn is_game_over(&self) -> bool {
        !self.board.has_legal_move()
    }

    pub fn legal_directions(&self) -> Vec<Direction> {
        self.board.legal_directions()
    }

    pub fn preview_outcomes(&self, direction: Direction) -> Vec<Outcome> {
        let slide = self.board.slide(direction);
        if !slide.changed {
            return Vec::new();
        }

        let candidates = spawn_candidates(&slide.board, direction, slide.moved_lines);
        if candidates.is_empty() {
            return Vec::new();
        }

        let rank_options = self.next_tile.rank_options();
        let spawn_probability = 1.0 / candidates.len() as f64;
        let mut outcomes = Vec::with_capacity(candidates.len() * rank_options.len());

        for idx in candidates.iter().copied() {
            for (rank, rank_probability) in rank_options.iter().copied() {
                let mut state = self.clone();
                state.attempted_moves += 1;
                state.board = slide.board;
                state
                    .draw
                    .consume_known_rank(&state.next_tile, rank, &mut state.rng);
                state.bonus_tracker.advance_after_consuming_current();
                consume_simulated_spawn_choice(&mut state.rng, candidates.len());
                state.board.set_index(idx, rank);
                state.accepted_moves += 1;
                state.next_tile = state.draw.next_tile(&state.board, &mut state.rng);
                state
                    .bonus_tracker
                    .observe_revealed_next(state.board.high_rank(), &state.next_tile);

                outcomes.push(Outcome {
                    probability: spawn_probability * rank_probability,
                    direction,
                    row: idx / SIZE,
                    col: idx % SIZE,
                    face: rank_to_face(rank),
                    board: state.board.face_values(),
                    next_tile: state.next_tile.clone(),
                    bonus_forecast: state.bonus_forecast(DEFAULT_BONUS_FORECAST_HORIZON),
                    score: state.score(),
                    high_tile: state.high_tile(),
                    state,
                });
            }
        }

        outcomes
    }

    pub fn snapshot(&self) -> GameSnapshot {
        GameSnapshot {
            seed: self.seed,
            board: self.board.face_values(),
            score: self.score(),
            high_tile: self.high_tile(),
            accepted_moves: self.accepted_moves,
            attempted_moves: self.attempted_moves,
            legal_moves_mask: self.board.legal_moves_mask(),
            next_tile: self.next_tile.clone(),
            bonus_forecast: self.bonus_forecast(DEFAULT_BONUS_FORECAST_HORIZON),
        }
    }

    pub fn step(&mut self, direction: Direction) -> MoveResult {
        self.attempted_moves += 1;

        let before = self.board;
        let slide = self.board.slide(direction);
        if !slide.changed {
            return MoveResult {
                direction,
                accepted: false,
                board_before: before.face_values(),
                board_after_slide: before.face_values(),
                board_after_spawn: before.face_values(),
                moved_lines: slide.moved_lines,
                spawn: None,
                score: self.score(),
                high_tile: self.high_tile(),
                next_tile: self.next_tile.clone(),
                bonus_forecast: self.bonus_forecast(DEFAULT_BONUS_FORECAST_HORIZON),
                game_over: self.is_game_over(),
            };
        }

        self.board = slide.board;
        let preview = self.next_tile.clone();
        let rank = self.draw.consume_next(&preview, &mut self.rng);
        self.bonus_tracker.advance_after_consuming_current();
        let spawn_idx = self.choose_spawn_index(&slide.board, direction, slide.moved_lines);
        let (row, col) = (spawn_idx / SIZE, spawn_idx % SIZE);
        self.board.set_index(spawn_idx, rank);
        self.accepted_moves += 1;

        let source = match preview {
            NextTile::Basic { .. } => SpawnSource::Basic,
            NextTile::Bonus { .. } => SpawnSource::Bonus,
        };

        let spawn = SpawnEvent {
            row,
            col,
            rank,
            face: rank_to_face(rank),
            source,
            preview,
        };

        self.next_tile = self.draw.next_tile(&self.board, &mut self.rng);
        self.bonus_tracker
            .observe_revealed_next(self.board.high_rank(), &self.next_tile);

        MoveResult {
            direction,
            accepted: true,
            board_before: before.face_values(),
            board_after_slide: slide.board.face_values(),
            board_after_spawn: self.board.face_values(),
            moved_lines: slide.moved_lines,
            spawn: Some(spawn),
            score: self.score(),
            high_tile: self.high_tile(),
            next_tile: self.next_tile.clone(),
            bonus_forecast: self.bonus_forecast(DEFAULT_BONUS_FORECAST_HORIZON),
            game_over: self.is_game_over(),
        }
    }

    fn choose_spawn_index(
        &mut self,
        board: &Board,
        direction: Direction,
        moved_lines: [bool; SIZE],
    ) -> usize {
        let candidates = spawn_candidates(board, direction, moved_lines);
        let choice = self.rng.gen_range(0..candidates.len());
        candidates[choice]
    }
}

pub fn time_seed() -> u64 {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    duration.as_secs() ^ ((duration.subsec_nanos() as u64) << 32)
}

#[derive(Clone)]
struct DrawState {
    basic_deck: [u16; BASIC_DECK_SIZE],
    basic_pos: usize,
    bonus_cycle: [bool; BONUS_CYCLE_SIZE],
    bonus_pos: usize,
    pending_bonus_cycle_active: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct BonusTracker {
    active: bool,
    current_offset: u8,
    seen_bonus_offset: Option<u8>,
}

impl BonusTracker {
    fn observe_revealed_next(&mut self, high_rank: u16, next_tile: &NextTile) {
        if high_rank < BONUS_UNLOCK_HIGH_RANK {
            *self = Self::default();
            return;
        }

        if !self.active {
            self.active = true;
            self.current_offset = 0;
            self.seen_bonus_offset = None;
        }

        if matches!(next_tile, NextTile::Bonus { .. }) {
            self.seen_bonus_offset = Some(self.current_offset);
        }
    }

    fn advance_after_consuming_current(&mut self) {
        if !self.active {
            return;
        }

        self.current_offset += 1;
        if usize::from(self.current_offset) >= BONUS_CYCLE_SIZE {
            self.current_offset = 0;
            self.seen_bonus_offset = None;
        }
    }

    fn forecast(&self, horizon: u8) -> BonusForecast {
        if !self.active {
            return BonusForecast {
                unlocked: false,
                cycle_position: None,
                bonus_seen_this_cycle: false,
                slots: (0..=horizon)
                    .map(|accepted_moves_from_now| BonusForecastSlot {
                        accepted_moves_from_now,
                        probability: 0.0,
                    })
                    .collect(),
            };
        }

        BonusForecast {
            unlocked: true,
            cycle_position: Some(self.current_offset),
            bonus_seen_this_cycle: self.seen_bonus_offset.is_some(),
            slots: (0..=horizon)
                .map(|accepted_moves_from_now| BonusForecastSlot {
                    accepted_moves_from_now,
                    probability: self.probability_at(accepted_moves_from_now),
                })
                .collect(),
        }
    }

    fn probability_at(&self, accepted_moves_from_now: u8) -> f64 {
        if !self.active {
            return 0.0;
        }

        let absolute_offset =
            usize::from(self.current_offset) + usize::from(accepted_moves_from_now);
        let cycle_delta = absolute_offset / BONUS_CYCLE_SIZE;
        let offset = (absolute_offset % BONUS_CYCLE_SIZE) as u8;

        if cycle_delta > 0 {
            return 1.0 / BONUS_CYCLE_SIZE as f64;
        }

        if let Some(seen) = self.seen_bonus_offset {
            if seen == offset {
                return 1.0;
            }
            return 0.0;
        }

        if accepted_moves_from_now == 0 {
            return 0.0;
        }

        let remaining_unseen = BONUS_CYCLE_SIZE - usize::from(self.current_offset) - 1;
        if remaining_unseen == 0 {
            0.0
        } else {
            1.0 / remaining_unseen as f64
        }
    }

    fn signature(&self) -> [u8; 3] {
        [
            u8::from(self.active),
            self.current_offset,
            self.seen_bonus_offset.unwrap_or(u8::MAX),
        ]
    }
}

impl DrawState {
    fn new<R: Rng + ?Sized>(rng: &mut R) -> Self {
        let mut state = Self {
            basic_deck: [1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3],
            basic_pos: 0,
            bonus_cycle: [false; BONUS_CYCLE_SIZE],
            bonus_pos: BONUS_CYCLE_SIZE,
            pending_bonus_cycle_active: false,
        };
        state.shuffle_basic(rng);
        state.shuffle_bonus_cycle(rng);
        state
    }

    fn draw_basic<R: Rng + ?Sized>(&mut self, rng: &mut R) -> u16 {
        self.ensure_basic_available(rng);
        let rank = self.basic_deck[self.basic_pos];
        self.basic_pos += 1;
        rank
    }

    fn peek_basic<R: Rng + ?Sized>(&mut self, rng: &mut R) -> u16 {
        self.ensure_basic_available(rng);
        self.basic_deck[self.basic_pos]
    }

    fn next_tile<R: Rng + ?Sized>(&mut self, board: &Board, rng: &mut R) -> NextTile {
        if let Some(ranks) = self.next_bonus_window(board, rng) {
            NextTile::Bonus {
                faces: ranks.iter().copied().map(rank_to_face).collect(),
                ranks,
            }
        } else {
            let rank = self.peek_basic(rng);
            NextTile::Basic {
                rank,
                face: rank_to_face(rank),
            }
        }
    }

    fn consume_next<R: Rng + ?Sized>(&mut self, next: &NextTile, rng: &mut R) -> u16 {
        match next {
            NextTile::Basic { rank, .. } => {
                let drawn = self.draw_basic(rng);
                debug_assert_eq!(*rank, drawn);
                if self.pending_bonus_cycle_active {
                    self.advance_bonus_cycle(rng);
                }
                drawn
            }
            NextTile::Bonus { ranks, .. } => {
                self.advance_bonus_cycle(rng);
                ranks[rng.gen_range(0..ranks.len())]
            }
        }
    }

    fn consume_known_rank<R: Rng + ?Sized>(&mut self, next: &NextTile, rank: u16, rng: &mut R) {
        match next {
            NextTile::Basic { rank: expected, .. } => {
                let drawn = self.draw_basic(rng);
                debug_assert_eq!(*expected, drawn);
                debug_assert_eq!(rank, drawn);
                if self.pending_bonus_cycle_active {
                    self.advance_bonus_cycle(rng);
                }
            }
            NextTile::Bonus { ranks, .. } => {
                debug_assert!(ranks.contains(&rank));
                self.advance_bonus_cycle(rng);
                let _ = rng.gen_range(0..ranks.len());
            }
        }
    }

    fn next_bonus_window<R: Rng + ?Sized>(
        &mut self,
        board: &Board,
        rng: &mut R,
    ) -> Option<Vec<u16>> {
        let high_rank = board.high_rank();
        if high_rank < BONUS_UNLOCK_HIGH_RANK {
            self.pending_bonus_cycle_active = false;
            return None;
        }

        self.ensure_bonus_cycle_available(rng);
        self.pending_bonus_cycle_active = true;
        if !self.bonus_cycle[self.bonus_pos] {
            return None;
        }

        let max_rank = high_rank - 3;
        let eligible_count = (max_rank - BONUS_LOWEST_RANK + 1) as usize;
        let window_len = eligible_count.min(3);
        let max_start = eligible_count - window_len;
        let start = rng.gen_range(0..=max_start);
        Some(
            (0..window_len)
                .map(|offset| BONUS_LOWEST_RANK + (start + offset) as u16)
                .collect(),
        )
    }

    fn ensure_basic_available<R: Rng + ?Sized>(&mut self, rng: &mut R) {
        if self.basic_pos >= BASIC_DECK_SIZE {
            self.shuffle_basic(rng);
        }
    }

    fn shuffle_basic<R: Rng + ?Sized>(&mut self, rng: &mut R) {
        self.basic_deck = [1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3];
        self.basic_deck.shuffle(rng);
        self.basic_pos = 0;
    }

    fn ensure_bonus_cycle_available<R: Rng + ?Sized>(&mut self, rng: &mut R) {
        if self.bonus_pos >= BONUS_CYCLE_SIZE {
            self.shuffle_bonus_cycle(rng);
        }
    }

    fn advance_bonus_cycle<R: Rng + ?Sized>(&mut self, rng: &mut R) {
        self.bonus_pos += 1;
        if self.bonus_pos >= BONUS_CYCLE_SIZE {
            self.shuffle_bonus_cycle(rng);
        }
    }

    fn shuffle_bonus_cycle<R: Rng + ?Sized>(&mut self, rng: &mut R) {
        self.bonus_cycle = [false; BONUS_CYCLE_SIZE];
        self.bonus_cycle[0] = true;
        self.bonus_cycle.shuffle(rng);
        self.bonus_pos = 0;
    }
}

impl NextTile {
    fn rank_options(&self) -> Vec<(u16, f64)> {
        match self {
            Self::Basic { rank, .. } => vec![(*rank, 1.0)],
            Self::Bonus { ranks, .. } => {
                let probability = 1.0 / ranks.len() as f64;
                ranks
                    .iter()
                    .copied()
                    .map(|rank| (rank, probability))
                    .collect()
            }
        }
    }
}

fn spawn_candidates(board: &Board, direction: Direction, moved_lines: [bool; SIZE]) -> Vec<usize> {
    let mut candidates = Vec::with_capacity(SIZE);
    for (line_idx, moved) in moved_lines.into_iter().enumerate() {
        if !moved {
            continue;
        }
        let idx = match direction {
            Direction::Left => index(line_idx, SIZE - 1),
            Direction::Right => index(line_idx, 0),
            Direction::Up => index(SIZE - 1, line_idx),
            Direction::Down => index(0, line_idx),
        };
        if board.get_index(idx) == 0 {
            candidates.push(idx);
        }
    }
    candidates
}

fn consume_simulated_spawn_choice<R: Rng + ?Sized>(rng: &mut R, candidate_count: usize) {
    let _ = rng.gen_range(0..candidate_count);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_board_has_nine_tiles_and_basic_distribution_remaining() {
        let game = Game::new(7);
        let occupied = game
            .board()
            .ranks()
            .into_iter()
            .filter(|rank| *rank != 0)
            .count();
        assert_eq!(occupied, 9);
        assert!(matches!(game.next_tile(), NextTile::Basic { .. }));
    }

    #[test]
    fn deterministic_seed_replays_initial_board() {
        let a = Game::new(123);
        let b = Game::new(123);
        assert_eq!(a.board(), b.board());
        assert_eq!(
            serde_json::to_value(a.next_tile()).unwrap(),
            serde_json::to_value(b.next_tile()).unwrap()
        );
    }

    #[test]
    fn fixed_seed_replays_moves() {
        let mut a = Game::new(123);
        let mut b = Game::new(123);
        for direction in [
            Direction::Left,
            Direction::Up,
            Direction::Right,
            Direction::Down,
            Direction::Left,
        ] {
            let ar = a.step(direction);
            let br = b.step(direction);
            assert_eq!(
                serde_json::to_value(&ar).unwrap(),
                serde_json::to_value(&br).unwrap()
            );
        }
    }

    #[test]
    fn bonus_windows_are_consecutive_and_limited_to_three() {
        let mut rng = ChaCha8Rng::seed_from_u64(3);
        let mut draw = DrawState::new(&mut rng);
        draw.bonus_cycle = [true; BONUS_CYCLE_SIZE];
        draw.bonus_pos = 0;
        let board = Board::from_ranks([10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

        for _ in 0..100 {
            match draw.next_tile(&board, &mut rng) {
                NextTile::Bonus { ranks, .. } => {
                    assert!((1..=3).contains(&ranks.len()));
                    assert!(ranks.windows(2).all(|pair| pair[1] == pair[0] + 1));
                    assert!(ranks[0] >= 4);
                    assert!(*ranks.last().unwrap() <= 7);
                }
                NextTile::Basic { .. } => panic!("forced cycle should produce bonus"),
            }
        }
    }

    #[test]
    fn one_bonus_slot_per_cycle() {
        let mut rng = ChaCha8Rng::seed_from_u64(5);
        let draw = DrawState::new(&mut rng);
        assert_eq!(
            draw.bonus_cycle
                .into_iter()
                .filter(|is_bonus| *is_bonus)
                .count(),
            1
        );
    }

    #[test]
    fn bonus_forecast_is_locked_before_high_tile_unlocks_bonus() {
        let mut tracker = BonusTracker::default();
        tracker.observe_revealed_next(6, &NextTile::Basic { rank: 1, face: 1 });

        let forecast = tracker.forecast(3);
        assert!(!forecast.unlocked);
        assert!(forecast.slots.iter().all(|slot| slot.probability == 0.0));
    }

    #[test]
    fn bonus_forecast_spreads_probability_over_unseen_current_cycle_slots() {
        let mut tracker = BonusTracker::default();
        tracker.observe_revealed_next(7, &NextTile::Basic { rank: 1, face: 1 });

        let forecast = tracker.forecast(3);
        assert!(forecast.unlocked);
        assert_eq!(forecast.cycle_position, Some(0));
        assert_eq!(forecast.slots[0].probability, 0.0);
        assert!((forecast.slots[1].probability - 0.05).abs() < f64::EPSILON);
        assert!((forecast.slots[2].probability - 0.05).abs() < f64::EPSILON);
    }

    #[test]
    fn bonus_forecast_knows_current_bonus_and_suppresses_rest_of_cycle() {
        let mut tracker = BonusTracker::default();
        tracker.observe_revealed_next(
            7,
            &NextTile::Bonus {
                ranks: vec![4],
                faces: vec![6],
            },
        );

        let forecast = tracker.forecast(3);
        assert_eq!(forecast.slots[0].probability, 1.0);
        assert_eq!(forecast.slots[1].probability, 0.0);
        assert_eq!(forecast.slots[2].probability, 0.0);
    }

    #[test]
    fn bonus_forecast_resets_after_cycle_wrap_and_observed_basic() {
        let mut tracker = BonusTracker {
            active: true,
            current_offset: 20,
            seen_bonus_offset: Some(5),
        };
        tracker.advance_after_consuming_current();
        tracker.observe_revealed_next(7, &NextTile::Basic { rank: 1, face: 1 });

        let forecast = tracker.forecast(2);
        assert_eq!(forecast.cycle_position, Some(0));
        assert!(!forecast.bonus_seen_this_cycle);
        assert_eq!(forecast.slots[0].probability, 0.0);
        assert!((forecast.slots[1].probability - 0.05).abs() < f64::EPSILON);
    }

    #[test]
    fn preview_outcome_probabilities_sum_to_one_for_legal_move() {
        let game = Game::new(123);
        let direction = game.legal_directions()[0];
        let outcomes = game.preview_outcomes(direction);
        let total = outcomes
            .iter()
            .map(|outcome| outcome.probability)
            .sum::<f64>();
        assert!((total - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn preview_outcomes_do_not_mutate_game() {
        let game = Game::new(123);
        let before = game.snapshot();
        let direction = game.legal_directions()[0];
        let _ = game.preview_outcomes(direction);
        let after = game.snapshot();
        assert_eq!(
            serde_json::to_value(before).unwrap(),
            serde_json::to_value(after).unwrap()
        );
    }
}
