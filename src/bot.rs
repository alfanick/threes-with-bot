use crate::board::{rank_to_face, Board, Direction, SIZE};
use crate::game::{Game, DEFAULT_BONUS_FORECAST_HORIZON};
use rand::seq::SliceRandom;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use rayon::prelude::*;
use std::collections::HashMap;
use std::sync::RwLock;

const BOT_SEED_DOMAIN: u64 = 0x9e37_79b9_7f4a_7c15;
const DFS_PLAN_DEPTH: u8 = 4;
const DFS_PLAN_BEAM: usize = 2;
const DFS_PLAN_WEIGHT: f64 = 0.35;
const AB_CACHE_MAX_ENTRIES: usize = 200_000;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BotKind {
    Random,
    Ab(AbConfig),
}

impl BotKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Random => "random",
            Self::Ab(_) => "ab",
        }
    }
}

pub trait Bot {
    fn name(&self) -> &'static str;
    fn choose_move(&mut self, game: &Game) -> Option<Direction>;
}

pub fn create_bot(kind: BotKind, game_seed: u64) -> Box<dyn Bot> {
    match kind {
        BotKind::Random => Box::new(RandomBot::new(game_seed)),
        BotKind::Ab(config) => Box::new(AbBot::new(config)),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AbConfig {
    pub depth: u8,
    pub alpha: f64,
    pub beta: f64,
    pub dfs: bool,
}

impl Default for AbConfig {
    fn default() -> Self {
        Self {
            depth: 3,
            alpha: f64::NEG_INFINITY,
            beta: f64::INFINITY,
            dfs: false,
        }
    }
}

struct RandomBot {
    rng: ChaCha8Rng,
}

impl RandomBot {
    fn new(game_seed: u64) -> Self {
        Self {
            rng: ChaCha8Rng::seed_from_u64(game_seed ^ BOT_SEED_DOMAIN),
        }
    }
}

impl Bot for RandomBot {
    fn name(&self) -> &'static str {
        BotKind::Random.name()
    }

    fn choose_move(&mut self, game: &Game) -> Option<Direction> {
        let legal = game.legal_directions();
        legal.choose(&mut self.rng).copied()
    }
}

struct AbBot {
    config: AbConfig,
    cache: RwLock<HashMap<AbCacheKey, f64>>,
}

impl AbBot {
    fn new(config: AbConfig) -> Self {
        Self {
            config,
            cache: RwLock::new(HashMap::new()),
        }
    }

    fn choose_with_score(&self, game: &Game) -> Option<(Direction, f64)> {
        game.legal_directions()
            .into_par_iter()
            .enumerate()
            .map(|(index, direction)| {
                let value = self.expected_value_for_move(
                    game,
                    direction,
                    self.config.depth,
                    self.config.alpha,
                    self.config.beta,
                );
                (index, direction, value)
            })
            .reduce_with(best_parallel_result)
            .map(|(_, direction, value)| (direction, value))
    }

    fn expected_value_for_move(
        &self,
        game: &Game,
        direction: Direction,
        depth: u8,
        alpha: f64,
        beta: f64,
    ) -> f64 {
        let outcomes = game.preview_outcomes(direction);
        if outcomes.is_empty() {
            return f64::NEG_INFINITY;
        }

        let mut expected = 0.0;
        for outcome in outcomes {
            let value = if depth <= 1 || outcome.game().is_game_over() {
                evaluate(outcome.game(), self.config)
            } else {
                self.search(outcome.game(), depth - 1, alpha, beta)
            };
            expected += outcome.probability * value;
        }
        expected
    }

    fn search(&self, game: &Game, depth: u8, mut alpha: f64, beta: f64) -> f64 {
        let key = AbCacheKey::new(game, depth, self.config.dfs);
        if let Some(value) = self.cached_value(&key) {
            return value;
        }

        if depth == 0 || game.is_game_over() {
            let value = evaluate(game, self.config);
            self.store_cached_value(key, value);
            return value;
        }

        let legal = game.legal_directions();
        if legal.is_empty() {
            let value = evaluate(game, self.config);
            self.store_cached_value(key, value);
            return value;
        }

        let mut best = f64::NEG_INFINITY;
        let mut cut_off = false;
        for direction in legal {
            let value = self.expected_value_for_move(game, direction, depth, alpha, beta);
            best = best.max(value);
            alpha = alpha.max(best);
            if alpha >= beta {
                cut_off = true;
                break;
            }
        }
        if !cut_off {
            self.store_cached_value(key, best);
        }
        best
    }

    fn cached_value(&self, key: &AbCacheKey) -> Option<f64> {
        self.cache
            .read()
            .expect("AB cache lock poisoned")
            .get(key)
            .copied()
    }

    fn store_cached_value(&self, key: AbCacheKey, value: f64) {
        let mut cache = self.cache.write().expect("AB cache lock poisoned");
        if cache.len() >= AB_CACHE_MAX_ENTRIES {
            cache.clear();
        }
        cache.insert(key, value);
    }

    #[cfg(test)]
    fn cache_len(&self) -> usize {
        self.cache.read().expect("AB cache lock poisoned").len()
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct AbCacheKey {
    board: [u16; 16],
    next: Vec<u16>,
    bonus: [u8; 3],
    depth: u8,
    dfs: bool,
}

impl AbCacheKey {
    fn new(game: &Game, depth: u8, dfs: bool) -> Self {
        Self {
            board: game.board().ranks(),
            next: game.next_tile().rank_signature(),
            bonus: game.bonus_forecast_signature(),
            depth,
            dfs,
        }
    }
}

fn best_parallel_result(
    left: (usize, Direction, f64),
    right: (usize, Direction, f64),
) -> (usize, Direction, f64) {
    let left_is_better = left.2 > right.2 || (left.2 == right.2 && left.0 < right.0);
    if left_is_better {
        left
    } else {
        right
    }
}

impl Bot for AbBot {
    fn name(&self) -> &'static str {
        "ab"
    }

    fn choose_move(&mut self, game: &Game) -> Option<Direction> {
        self.choose_with_score(game).map(|(direction, _)| direction)
    }
}

fn evaluate(game: &Game, config: AbConfig) -> f64 {
    let base = base_evaluate(game);
    if config.dfs {
        base + dfs_plan_score(game) * DFS_PLAN_WEIGHT
    } else {
        base
    }
}

fn base_evaluate(game: &Game) -> f64 {
    let board = game.board();
    base_evaluate_board(&board) + bonus_forecast_score(game)
}

fn base_evaluate_board(board: &Board) -> f64 {
    board.score() as f64
        + empty_cell_score(board)
        + legal_move_score(board)
        + merge_opportunity_score(board)
        + one_two_merge_score(board)
        + high_tile_score(board)
        + orderedness_score(board)
        + max_corner_score(board)
        - one_two_imbalance_penalty(board)
}

fn dfs_plan_score(game: &Game) -> f64 {
    let root = game.board();
    dfs_plan_visit(game, root.score(), root.high_rank(), DFS_PLAN_DEPTH, 0)
}

fn dfs_plan_visit(game: &Game, root_score: u64, root_high_rank: u16, depth: u8, ply: u8) -> f64 {
    let immediate = plan_progress_score(game, root_score, root_high_rank) / f64::from(ply + 1);
    if depth == 0 || game.is_game_over() {
        return immediate;
    }

    let mut best = immediate;
    for direction in game.legal_directions() {
        let mut outcomes = game.preview_outcomes(direction);
        outcomes.sort_by(|a, b| {
            plan_progress_score(b.game(), root_score, root_high_rank)
                .total_cmp(&plan_progress_score(a.game(), root_score, root_high_rank))
        });
        outcomes.truncate(DFS_PLAN_BEAM);

        for outcome in outcomes {
            let path_score = outcome.probability.sqrt()
                * dfs_plan_visit(
                    outcome.game(),
                    root_score,
                    root_high_rank,
                    depth - 1,
                    ply + 1,
                );
            best = best.max(path_score);
        }
    }
    best
}

fn plan_progress_score(game: &Game, root_score: u64, root_high_rank: u16) -> f64 {
    let board = game.board();
    let score_gain = board.score().saturating_sub(root_score) as f64;
    let high_gain = board.high_rank().saturating_sub(root_high_rank) as f64 * 650.0;

    score_gain * 0.75
        + high_gain
        + max_corner_score(&board) * 0.7
        + orderedness_score(&board) * 0.35
        + merge_opportunity_score(&board) * 0.45
        + one_two_merge_score(&board) * 0.6
        + bonus_forecast_score(game) * 0.5
        + corner_path_score(&board)
        + merge_chain_path_score(&board)
}

fn empty_cell_score(board: &Board) -> f64 {
    board.empty_indices().len() as f64 * 120.0
}

fn legal_move_score(board: &Board) -> f64 {
    board.legal_directions().len() as f64 * 45.0
}

fn merge_opportunity_score(board: &Board) -> f64 {
    let mut opportunities = 0;
    for row in 0..SIZE {
        for col in 0..SIZE {
            let rank = board.get(row, col);
            if rank == 0 {
                continue;
            }
            if col + 1 < SIZE && crate::board::merge_rank(rank, board.get(row, col + 1)).is_some() {
                opportunities += 1;
            }
            if row + 1 < SIZE && crate::board::merge_rank(rank, board.get(row + 1, col)).is_some() {
                opportunities += 1;
            }
        }
    }
    opportunities as f64 * 90.0
}

fn one_two_merge_score(board: &Board) -> f64 {
    let mut pairs = 0;
    for row in 0..SIZE {
        for col in 0..SIZE {
            let rank = board.get(row, col);
            if !matches!(rank, 1 | 2) {
                continue;
            }
            if col + 1 < SIZE && is_one_two_pair(rank, board.get(row, col + 1)) {
                pairs += 1;
            }
            if row + 1 < SIZE && is_one_two_pair(rank, board.get(row + 1, col)) {
                pairs += 1;
            }
        }
    }
    pairs as f64 * 110.0
}

fn is_one_two_pair(left: u16, right: u16) -> bool {
    matches!((left, right), (1, 2) | (2, 1))
}

fn high_tile_score(board: &Board) -> f64 {
    let high = board.high_face();
    if high == 0 {
        0.0
    } else {
        (high as f64).log2() * 80.0
    }
}

fn orderedness_score(board: &Board) -> f64 {
    let mut score = 0.0;
    for row in 0..SIZE {
        let line = [
            board.get(row, 0),
            board.get(row, 1),
            board.get(row, 2),
            board.get(row, 3),
        ];
        score += line_orderedness(line);
    }
    for col in 0..SIZE {
        let line = [
            board.get(0, col),
            board.get(1, col),
            board.get(2, col),
            board.get(3, col),
        ];
        score += line_orderedness(line);
    }
    score * 35.0
}

fn line_orderedness(line: [u16; SIZE]) -> f64 {
    let increasing = adjacent_order_score(line);
    let decreasing = adjacent_order_score([line[3], line[2], line[1], line[0]]);
    increasing.max(decreasing)
}

fn adjacent_order_score(line: [u16; SIZE]) -> f64 {
    line.windows(2)
        .map(|pair| {
            if pair[0] == 0 || pair[1] == 0 {
                0.25
            } else if pair[0] >= pair[1] {
                1.0
            } else {
                -0.75 * f64::from(pair[1] - pair[0])
            }
        })
        .sum()
}

fn max_corner_score(board: &Board) -> f64 {
    let high = board.high_rank();
    if high == 0 {
        return 0.0;
    }
    let corners = [
        board.get(0, 0),
        board.get(0, SIZE - 1),
        board.get(SIZE - 1, 0),
        board.get(SIZE - 1, SIZE - 1),
    ];
    if corners.contains(&high) {
        500.0
    } else {
        0.0
    }
}

fn corner_path_score(board: &Board) -> f64 {
    let high = board.high_rank();
    if high == 0 {
        return 0.0;
    }

    let mut best = 0.0;
    for idx in board
        .ranks()
        .into_iter()
        .enumerate()
        .filter_map(|(idx, rank)| (rank == high).then_some(idx))
    {
        let row = idx / SIZE;
        let col = idx % SIZE;
        for (corner_row, corner_col) in [(0, 0), (0, SIZE - 1), (SIZE - 1, 0), (SIZE - 1, SIZE - 1)]
        {
            let distance = row.abs_diff(corner_row) + col.abs_diff(corner_col);
            let alignment_bonus = if row == corner_row || col == corner_col {
                120.0
            } else {
                0.0
            };
            let clear_bonus = if path_to_corner_is_clearish(board, row, col, corner_row, corner_col)
            {
                90.0
            } else {
                0.0
            };
            let score = 260.0 / (distance + 1) as f64 + alignment_bonus + clear_bonus;
            best = f64::max(best, score);
        }
    }
    best
}

fn path_to_corner_is_clearish(
    board: &Board,
    row: usize,
    col: usize,
    corner_row: usize,
    corner_col: usize,
) -> bool {
    let row_step = step_toward(row, corner_row);
    let col_step = step_toward(col, corner_col);

    let mut current_row = row;
    while current_row != corner_row {
        current_row = (current_row as isize + row_step) as usize;
        let rank = board.get(current_row, col);
        if rank > 0 && rank != board.high_rank() {
            return false;
        }
    }

    let mut current_col = col;
    while current_col != corner_col {
        current_col = (current_col as isize + col_step) as usize;
        let rank = board.get(corner_row, current_col);
        if rank > 0 && rank != board.high_rank() {
            return false;
        }
    }

    true
}

fn step_toward(from: usize, to: usize) -> isize {
    match from.cmp(&to) {
        std::cmp::Ordering::Less => 1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => -1,
    }
}

fn merge_chain_path_score(board: &Board) -> f64 {
    let ranks = board.ranks();
    let mut score = 0.0;

    for left_idx in 0..ranks.len() {
        let rank = ranks[left_idx];
        if rank < 3 {
            continue;
        }
        for (right_idx, other_rank) in ranks.iter().copied().enumerate().skip(left_idx + 1) {
            if other_rank != rank {
                continue;
            }

            let left_row = left_idx / SIZE;
            let left_col = left_idx % SIZE;
            let right_row = right_idx / SIZE;
            let right_col = right_idx % SIZE;
            let distance = left_row.abs_diff(right_row) + left_col.abs_diff(right_col);
            let aligned = left_row == right_row || left_col == right_col;
            let face_weight = (rank_to_face(rank) as f64).log2().max(1.0);
            let alignment = if aligned { 85.0 } else { 35.0 };

            score += alignment * face_weight / (distance + 1) as f64;
        }
    }

    score
}

fn one_two_imbalance_penalty(board: &Board) -> f64 {
    let mut ones = 0i32;
    let mut twos = 0i32;
    for rank in board.ranks() {
        match rank {
            1 => ones += 1,
            2 => twos += 1,
            _ => {}
        }
    }
    f64::from((ones - twos).abs()) * 40.0
}

fn bonus_forecast_score(game: &Game) -> f64 {
    let forecast = game.bonus_forecast(DEFAULT_BONUS_FORECAST_HORIZON);
    if !forecast.unlocked {
        return 0.0;
    }

    let high_face_scale = (game.high_tile() as f64).log2().max(1.0);
    forecast
        .slots
        .iter()
        .map(|slot| {
            let urgency = 1.0 / f64::from(slot.accepted_moves_from_now + 1);
            slot.probability * urgency * (220.0 + high_face_scale * 18.0)
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_bot_is_deterministic_for_seed() {
        let game = Game::new(123);
        let mut a = create_bot(BotKind::Random, 123);
        let mut b = create_bot(BotKind::Random, 123);

        assert_eq!(a.choose_move(&game), b.choose_move(&game));
    }

    #[test]
    fn random_bot_picks_legal_moves() {
        let game = Game::new(123);
        let legal = game.legal_directions();
        let mut bot = create_bot(BotKind::Random, 123);

        for _ in 0..20 {
            let direction = bot.choose_move(&game).unwrap();
            assert!(legal.contains(&direction));
        }
    }

    #[test]
    fn ab_bot_picks_legal_move() {
        let game = Game::new(123);
        let mut bot = create_bot(BotKind::Ab(AbConfig::default()), 123);
        let direction = bot.choose_move(&game).unwrap();
        assert!(game.legal_directions().contains(&direction));
    }

    #[test]
    fn ab_bot_is_deterministic_for_board() {
        let game = Game::new(123);
        let mut a = create_bot(BotKind::Ab(AbConfig::default()), 1);
        let mut b = create_bot(BotKind::Ab(AbConfig::default()), 2);
        assert_eq!(a.choose_move(&game), b.choose_move(&game));
    }

    #[test]
    fn ab_cache_reuses_visible_states() {
        let game = Game::new(123);
        let bot = AbBot::new(AbConfig::default());
        let first = bot.search(&game, 2, f64::NEG_INFINITY, f64::INFINITY);
        let cache_len = bot.cache_len();
        let second = bot.search(&game, 2, f64::NEG_INFINITY, f64::INFINITY);
        assert_eq!(first, second);
        assert_eq!(cache_len, bot.cache_len());
        assert!(cache_len > 0);
    }

    #[test]
    fn ab_dfs_keeps_move_legal() {
        let game = Game::new(123);
        let config = AbConfig {
            depth: 1,
            dfs: true,
            ..AbConfig::default()
        };
        let mut bot = create_bot(BotKind::Ab(config), 123);
        let direction = bot.choose_move(&game).unwrap();
        assert!(game.legal_directions().contains(&direction));
    }

    #[test]
    fn dfs_plan_score_is_non_negative() {
        let game = Game::new(123);
        assert!(dfs_plan_score(&game) >= 0.0);
    }

    #[test]
    fn corner_high_tile_scores_better() {
        let corner = Board::from_ranks([7, 1, 2, 3, 1, 2, 3, 1, 2, 3, 1, 2, 3, 1, 2, 3]);
        let middle = Board::from_ranks([1, 1, 2, 3, 1, 7, 3, 1, 2, 3, 1, 2, 3, 1, 2, 3]);
        assert!(max_corner_score(&corner) > max_corner_score(&middle));
    }

    #[test]
    fn ordered_line_scores_better_than_disordered_line() {
        assert!(line_orderedness([7, 6, 5, 4]) > line_orderedness([4, 7, 5, 6]));
    }

    #[test]
    fn parallel_result_tie_breaks_by_move_order() {
        let left = (2, Direction::Left, 10.0);
        let right = (1, Direction::Right, 10.0);
        assert_eq!(best_parallel_result(left, right), right);
    }

    #[test]
    fn merge_chain_path_rewards_aligned_pairs() {
        let aligned = Board::from_ranks([6, 6, 0, 0, 1, 2, 3, 1, 2, 3, 1, 2, 3, 1, 2, 3]);
        let distant = Board::from_ranks([6, 1, 2, 3, 1, 2, 3, 1, 2, 3, 1, 2, 3, 1, 2, 6]);
        assert!(merge_chain_path_score(&aligned) > merge_chain_path_score(&distant));
    }

    #[test]
    fn one_two_merge_score_rewards_adjacent_pairs() {
        let adjacent = Board::from_ranks([1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let reversed = Board::from_ranks([2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let separated = Board::from_ranks([1, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

        assert!(one_two_merge_score(&adjacent) > one_two_merge_score(&separated));
        assert_eq!(
            one_two_merge_score(&adjacent),
            one_two_merge_score(&reversed)
        );
    }

    #[test]
    fn base_eval_rewards_one_two_merge_setup() {
        let adjacent = Board::from_ranks([1, 2, 0, 0, 4, 5, 6, 7, 4, 5, 6, 7, 4, 5, 6, 7]);
        let separated = Board::from_ranks([1, 0, 2, 0, 4, 5, 6, 7, 4, 5, 6, 7, 4, 5, 6, 7]);

        assert!(one_two_merge_score(&adjacent) > 0.0);
        assert!(base_evaluate_board(&adjacent) > base_evaluate_board(&separated));
    }
}
