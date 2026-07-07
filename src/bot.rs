use crate::board::{rank_to_face, Board, Direction, SIZE};
use crate::game::{Game, Outcome, DEFAULT_BONUS_FORECAST_HORIZON};
use rand::seq::SliceRandom;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use rayon::prelude::*;
use std::collections::HashMap;
use std::sync::RwLock;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

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
    fn search_stats(&self) -> Option<AbSearchStats> {
        None
    }
    fn set_time_limit_ms(&mut self, _time_limit_ms: Option<u64>) {}
    fn set_cancel_token(&mut self, _cancel: Option<Arc<AtomicBool>>) {}

    fn board_eval(&self, _game: &Game) -> Option<f64> {
        None
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AbSearchStats {
    pub predicted_states: u64,
    pub pruned_states: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub searched_depth: u8,
    pub selected_score: f64,
    pub board_eval: f64,
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
    pub time_limit_ms: Option<u64>,
    pub node_limit: Option<u64>,
}

impl Default for AbConfig {
    fn default() -> Self {
        Self {
            depth: 3,
            alpha: f64::NEG_INFINITY,
            beta: f64::INFINITY,
            dfs: false,
            time_limit_ms: None,
            node_limit: None,
        }
    }
}

impl AbConfig {
    fn search_time_limit(&self) -> Option<Duration> {
        self.time_limit_ms
            .filter(|ms| *ms > 0)
            .map(Duration::from_millis)
    }

    fn has_limits(&self) -> bool {
        self.time_limit_ms.is_some() || self.node_limit.is_some()
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
    cache: RwLock<HashMap<AbCacheKey, AbCacheEntry>>,
    last_stats: Option<AbSearchStats>,
    cancel: Option<Arc<AtomicBool>>,
}

impl AbBot {
    fn new(config: AbConfig) -> Self {
        Self {
            config,
            cache: RwLock::new(HashMap::new()),
            last_stats: None,
            cancel: None,
        }
    }

    fn choose_with_score(&self, game: &Game) -> Option<(Direction, f64, AbSearchStats)> {
        let legal = game.legal_directions();
        if legal.is_empty() {
            return None;
        }

        let ordered_directions = self.order_directions(game, legal);
        let board_eval = evaluate(game, self.config);

        let mut result = if self.config.has_limits() {
            self.choose_with_limited_budget(game, &ordered_directions, board_eval)
        } else {
            self.choose_with_fixed_depth_par(game, &ordered_directions, board_eval)
        };

        if let Some((_, _, stats)) = &mut result {
            stats.board_eval = board_eval;
        }

        result
    }

    fn choose_with_fixed_depth_par(
        &self,
        game: &Game,
        ordered_directions: &[Direction],
        board_eval: f64,
    ) -> Option<(Direction, f64, AbSearchStats)> {
        let results: Vec<(usize, Direction, f64, SearchStats)> = ordered_directions
            .iter()
            .copied()
            .enumerate()
            .collect::<Vec<_>>()
            .into_par_iter()
            .map(|(index, direction)| {
                let mut budget = SearchBudget::unlimited_with_cancel(self.cancel.clone());
                let value = self.expected_value_for_move(
                    game,
                    direction,
                    self.config.depth,
                    self.config.alpha,
                    self.config.beta,
                    &mut budget,
                );
                (index, direction, value, budget.stats())
            })
            .collect();

        let mut best: Option<(usize, Direction, f64)> = None;
        let mut aggregate = SearchStats::default();

        for (index, direction, value, stats) in results {
            aggregate.add(stats);
            if best.is_none() {
                best = Some((index, direction, value));
                continue;
            }
            let best_score = best.map(|(_, _, score)| score).unwrap_or(f64::NEG_INFINITY);
            if value > best_score
                || (value == best_score
                    && index
                        < best
                            .map(|(best_index, _, _)| best_index)
                            .unwrap_or(usize::MAX))
            {
                best = Some((index, direction, value));
            }
        }

        best.map(|(_, direction, score)| {
            (
                direction,
                score,
                AbSearchStats {
                    predicted_states: aggregate.predicted_states,
                    pruned_states: aggregate.pruned_states,
                    cache_hits: aggregate.cache_hits,
                    cache_misses: aggregate.cache_misses,
                    searched_depth: self.config.depth,
                    selected_score: score,
                    board_eval,
                },
            )
        })
    }

    fn choose_with_limited_budget(
        &self,
        game: &Game,
        ordered_directions: &[Direction],
        board_eval: f64,
    ) -> Option<(Direction, f64, AbSearchStats)> {
        let mut budget = SearchBudget::with_cancel(
            self.config.search_time_limit(),
            self.config.node_limit,
            self.cancel.clone(),
        );

        let mut best: Option<(Direction, f64)> = None;
        let mut alpha = self.config.alpha;
        let mut searched_depth = 0;

        for depth in 1..=self.config.depth {
            if !budget.can_continue() {
                break;
            }

            if let Some((direction, score)) = self.choose_with_depth_and_budget(
                game,
                ordered_directions,
                depth,
                alpha,
                self.config.beta,
                &mut budget,
            ) {
                best = Some((direction, score));
                alpha = alpha.max(score);
                searched_depth = depth;
                if alpha >= self.config.beta {
                    break;
                }
            }
        }

        best.map(|(direction, score)| {
            let stats = budget.stats();
            (
                direction,
                score,
                AbSearchStats {
                    predicted_states: stats.predicted_states,
                    pruned_states: stats.pruned_states,
                    cache_hits: stats.cache_hits,
                    cache_misses: stats.cache_misses,
                    searched_depth,
                    selected_score: score,
                    board_eval,
                },
            )
        })
        .or_else(|| {
            let fallback = ordered_directions.first().copied()?;
            Some((
                fallback,
                board_eval,
                AbSearchStats {
                    selected_score: board_eval,
                    board_eval,
                    searched_depth: 0,
                    ..AbSearchStats::default()
                },
            ))
        })
    }

    fn choose_with_depth_and_budget(
        &self,
        game: &Game,
        ordered_directions: &[Direction],
        depth: u8,
        mut alpha: f64,
        beta: f64,
        budget: &mut SearchBudget,
    ) -> Option<(Direction, f64)> {
        let mut best_direction = None;
        let mut best_score = f64::NEG_INFINITY;

        for direction in ordered_directions.iter().copied() {
            if !budget.can_continue() {
                break;
            }

            let value = self.expected_value_for_move(game, direction, depth, alpha, beta, budget);

            if value > best_score || (value == best_score && best_direction.is_none()) {
                best_score = value;
                best_direction = Some(direction);
            }

            alpha = alpha.max(best_score);
            if alpha >= beta {
                break;
            }
        }

        best_direction.map(|direction| (direction, best_score))
    }

    fn expected_value_for_move(
        &self,
        game: &Game,
        direction: Direction,
        depth: u8,
        alpha: f64,
        beta: f64,
        budget: &mut SearchBudget,
    ) -> f64 {
        let outcomes = game.preview_outcomes(direction);
        if outcomes.is_empty() {
            return f64::NEG_INFINITY;
        }

        self.expected_value_for_move_with_outcomes(&outcomes, depth, alpha, beta, budget)
    }

    fn expected_value_for_move_with_outcomes(
        &self,
        outcomes: &[Outcome],
        depth: u8,
        alpha: f64,
        beta: f64,
        budget: &mut SearchBudget,
    ) -> f64 {
        let mut expected = 0.0;
        for outcome in outcomes {
            if !budget.can_continue() {
                return evaluate(outcome.game(), self.config);
            }

            let value = if depth <= 1 || outcome.game().is_game_over() {
                evaluate(outcome.game(), self.config)
            } else {
                self.search_with_budget(outcome.game(), depth - 1, alpha, beta, budget)
            };
            expected += outcome.probability * value;
        }
        expected
    }

    #[cfg(test)]
    fn search(&self, game: &Game, depth: u8, alpha: f64, beta: f64) -> f64 {
        let mut budget = SearchBudget::unlimited_with_cancel(self.cancel.clone());
        self.search_with_budget(game, depth, alpha, beta, &mut budget)
    }

    fn search_with_budget(
        &self,
        game: &Game,
        depth: u8,
        mut alpha: f64,
        mut beta: f64,
        budget: &mut SearchBudget,
    ) -> f64 {
        if !budget.can_continue() {
            return evaluate(game, self.config);
        }

        budget.record_node();
        let key = AbCacheKey::new(game, depth, self.config.dfs);
        let original_alpha = alpha;

        if let Some(entry) = self.cached_entry(&key, budget) {
            match entry.bound {
                AbCacheBound::Exact => return entry.value,
                AbCacheBound::Lower => {
                    alpha = alpha.max(entry.value);
                }
                AbCacheBound::Upper => {
                    beta = beta.min(entry.value);
                }
            }

            if alpha >= beta {
                budget.record_prune();
                return entry.value;
            }
        }

        if depth == 0 || game.is_game_over() {
            let value = evaluate(game, self.config);
            self.store_cached_value(key, value, AbCacheBound::Exact);
            return value;
        }

        let legal = game.legal_directions();
        if legal.is_empty() {
            let value = evaluate(game, self.config);
            self.store_cached_value(key, value, AbCacheBound::Exact);
            return value;
        }

        let candidates = self.ordered_move_candidates(game, legal);

        let mut best = f64::NEG_INFINITY;
        let mut cut_off = false;
        for candidate in candidates {
            if !budget.can_continue() {
                return evaluate(game, self.config);
            }

            let value = self.expected_value_for_move_with_outcomes(
                &candidate.outcomes,
                depth,
                alpha,
                beta,
                budget,
            );
            best = best.max(value);
            alpha = alpha.max(best);
            if alpha >= beta {
                budget.record_prune();
                cut_off = true;
                break;
            }
        }

        if !cut_off {
            self.store_cached_value(key, best, AbCacheBound::Exact);
            return best;
        }

        let bound = if best <= original_alpha {
            AbCacheBound::Upper
        } else {
            AbCacheBound::Lower
        };
        self.store_cached_value(key, best, bound);
        best
    }

    fn ordered_move_candidates(&self, game: &Game, legal: Vec<Direction>) -> Vec<MoveCandidate> {
        let mut candidates: Vec<MoveCandidate> = legal
            .into_iter()
            .enumerate()
            .filter_map(|(index, direction)| {
                let outcomes = game.preview_outcomes(direction);
                if outcomes.is_empty() {
                    return None;
                }
                let score = self.quick_outcome_value(&outcomes);

                Some(MoveCandidate {
                    direction,
                    outcomes,
                    score,
                    index,
                })
            })
            .collect();

        candidates.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.index.cmp(&right.index))
        });

        candidates
    }

    fn order_directions(&self, game: &Game, legal: Vec<Direction>) -> Vec<Direction> {
        self.ordered_move_candidates(game, legal)
            .into_iter()
            .map(|candidate| candidate.direction)
            .collect()
    }

    fn quick_outcome_value(&self, outcomes: &[Outcome]) -> f64 {
        outcomes
            .iter()
            .map(|outcome| outcome.probability * evaluate(outcome.game(), self.config))
            .sum()
    }

    fn cached_entry(&self, key: &AbCacheKey, budget: &mut SearchBudget) -> Option<AbCacheEntry> {
        let found = self
            .cache
            .read()
            .expect("AB cache lock poisoned")
            .get(key)
            .cloned();
        if found.is_some() {
            budget.record_cache_hit();
        } else {
            budget.record_cache_miss();
        }
        found
    }

    fn store_cached_value(&self, key: AbCacheKey, value: f64, bound: AbCacheBound) {
        let mut cache = self.cache.write().expect("AB cache lock poisoned");
        if cache.len() >= AB_CACHE_MAX_ENTRIES {
            cache.clear();
        }
        cache.insert(key, AbCacheEntry { value, bound });
    }

    #[cfg(test)]
    fn cache_len(&self) -> usize {
        self.cache.read().expect("AB cache lock poisoned").len()
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct AbCacheKey {
    board: [u16; 16],
    next: [u16; 3],
    next_len: u8,
    bonus: [u8; 3],
    depth: u8,
    dfs: bool,
}

impl AbCacheKey {
    fn new(game: &Game, depth: u8, dfs: bool) -> Self {
        let next_signature = game.next_tile().rank_signature();
        let mut next = [0; 3];
        let next_len = next_signature.len().min(3);
        next[..next_len].copy_from_slice(&next_signature[..next_len]);

        Self {
            board: game.board().ranks(),
            next,
            next_len: u8::try_from(next_len).unwrap_or(3),
            bonus: game.bonus_forecast_signature(),
            depth,
            dfs,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum AbCacheBound {
    Exact,
    Lower,
    Upper,
}

#[derive(Clone, Copy, Debug)]
struct AbCacheEntry {
    value: f64,
    bound: AbCacheBound,
}

#[derive(Clone)]
struct MoveCandidate {
    direction: Direction,
    outcomes: Vec<Outcome>,
    score: f64,
    index: usize,
}

#[derive(Debug)]
struct SearchBudget {
    deadline: Option<Instant>,
    cancel: Option<Arc<AtomicBool>>,
    node_limit: Option<u64>,
    nodes: u64,
    pruned_states: u64,
    cache_hits: u64,
    cache_misses: u64,
}

impl SearchBudget {
    #[allow(dead_code)]
    fn new(deadline: Option<Duration>, node_limit: Option<u64>) -> Self {
        Self::with_cancel(deadline, node_limit, None)
    }

    fn with_cancel(
        deadline: Option<Duration>,
        node_limit: Option<u64>,
        cancel: Option<Arc<AtomicBool>>,
    ) -> Self {
        let deadline = deadline.map(|duration| Instant::now() + duration);
        Self {
            deadline,
            cancel,
            node_limit,
            nodes: 0,
            pruned_states: 0,
            cache_hits: 0,
            cache_misses: 0,
        }
    }

    #[allow(dead_code)]
    fn unlimited() -> Self {
        Self::new(None, None)
    }

    fn unlimited_with_cancel(cancel: Option<Arc<AtomicBool>>) -> Self {
        Self::with_cancel(None, None, cancel)
    }

    fn can_continue(&self) -> bool {
        if let Some(cancel) = &self.cancel {
            if cancel.load(Ordering::Relaxed) {
                return false;
            }
        }

        if let Some(limit) = self.node_limit {
            if self.nodes >= limit {
                return false;
            }
        }

        if let Some(deadline) = self.deadline {
            if Instant::now() >= deadline {
                return false;
            }
        }

        true
    }

    fn record_node(&mut self) {
        self.nodes += 1;
    }

    fn record_prune(&mut self) {
        self.pruned_states += 1;
    }

    fn record_cache_hit(&mut self) {
        self.cache_hits += 1;
    }

    fn record_cache_miss(&mut self) {
        self.cache_misses += 1;
    }

    fn stats(&self) -> SearchStats {
        SearchStats {
            predicted_states: self.nodes,
            pruned_states: self.pruned_states,
            cache_hits: self.cache_hits,
            cache_misses: self.cache_misses,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct SearchStats {
    predicted_states: u64,
    pruned_states: u64,
    cache_hits: u64,
    cache_misses: u64,
}

impl SearchStats {
    fn add(&mut self, other: Self) {
        self.predicted_states += other.predicted_states;
        self.pruned_states += other.pruned_states;
        self.cache_hits += other.cache_hits;
        self.cache_misses += other.cache_misses;
    }
}

impl Bot for AbBot {
    fn name(&self) -> &'static str {
        "ab"
    }

    fn choose_move(&mut self, game: &Game) -> Option<Direction> {
        let (direction, _, stats) = self.choose_with_score(game)?;
        self.last_stats = Some(stats);
        Some(direction)
    }

    fn search_stats(&self) -> Option<AbSearchStats> {
        self.last_stats
    }

    fn board_eval(&self, game: &Game) -> Option<f64> {
        Some(evaluate(game, self.config))
    }

    fn set_time_limit_ms(&mut self, time_limit_ms: Option<u64>) {
        self.config.time_limit_ms = time_limit_ms;
    }

    fn set_cancel_token(&mut self, cancel: Option<Arc<AtomicBool>>) {
        self.cancel = cancel;
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
    fn ab_bot_reports_search_stats_after_move() {
        let game = Game::new(123);
        let mut bot = create_bot(BotKind::Ab(AbConfig::default()), 123);
        bot.choose_move(&game).unwrap();

        let Some(stats) = bot.search_stats() else {
            panic!("ab bot should expose search stats");
        };

        assert!(stats.searched_depth >= 1);
        assert!(stats.predicted_states > 0);
        assert!(stats.board_eval >= 0.0);
        assert!(stats.selected_score.is_finite());
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
