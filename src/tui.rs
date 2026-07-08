use crate::board::{rank_to_face, Board, Direction, SIZE};
use crate::bot::{create_bot, AbConfig, Bot, BotKind};
use crate::game::{time_seed, BonusForecast, Game, NextTile, DEFAULT_BONUS_FORECAST_HORIZON};
use crate::logging::{AbConfigLog, AbTurnStats, GameLogger, RunConfigLog};
use anyhow::{Context, Result};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{
    Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{execute, queue};
use std::io::{stdout, IsTerminal, Stdout, Write};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::thread;
use std::thread::sleep;
use std::time::{Duration, Instant};

const TILE_WIDTH: usize = 8;
const TILE_HEIGHT: usize = 4;
const TILE_LABEL_ROW: usize = 1;
const BOARD_GUTTER: usize = 4;
const NL: &str = "\r\n";

#[derive(Clone, Copy, Debug)]
pub enum ColorMode {
    Auto,
    Always,
    Never,
}

impl ColorMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Always => "always",
            Self::Never => "never",
        }
    }
}

#[derive(Clone, Debug)]
pub struct HumanConfig {
    pub seed: Option<u64>,
    pub log_json: Option<PathBuf>,
    pub log_text: Option<PathBuf>,
    pub color: ColorMode,
    pub speed_hz: f64,
    pub bot_opponent: Option<BotKind>,
}

#[derive(Clone, Debug)]
pub struct BotConfig {
    pub seed: Option<u64>,
    pub log_json: Option<PathBuf>,
    pub log_text: Option<PathBuf>,
    pub color: ColorMode,
    pub speed_hz: f64,
    pub bot: BotKind,
}

#[derive(Clone, Copy, Debug)]
pub struct ObserverConfig {
    pub speed_hz: f64,
}

#[derive(Clone, Copy)]
struct LastMoveView {
    direction: Direction,
    tile_face: u64,
    spawn_idx: Option<usize>,
    merged_cells: [bool; SIZE * SIZE],
}

impl ObserverConfig {
    pub fn move_delay(self) -> Duration {
        Duration::from_secs_f64(1.0 / self.speed_hz)
    }
}

#[derive(Clone)]
struct BotOpponentState {
    last_move: Option<LastMoveView>,
    active: bool,
    last_status: Option<String>,
}

struct BotOpponent {
    game: Game,
    state: BotOpponentState,
    thinker: OpponentThinker,
    bot_name: &'static str,
}

struct OpponentThinker {
    request_tx: Option<mpsc::Sender<OpponentThinkRequest>>,
    pending: Option<PendingThought>,
    handle: Option<thread::JoinHandle<()>>,
}

struct OpponentThinkRequest {
    game: Game,
    time_limit_ms: Option<u64>,
    cancel: Arc<AtomicBool>,
    response: mpsc::Sender<OpponentThinkResponse>,
}

struct OpponentThinkResponse {
    direction: Option<Direction>,
    search_stats: Option<crate::bot::AbSearchStats>,
}

struct PendingThought {
    cancel: Arc<AtomicBool>,
    response: mpsc::Receiver<OpponentThinkResponse>,
}

impl OpponentThinker {
    fn new(kind: BotKind, seed: u64) -> Self {
        let (request_tx, request_rx) = mpsc::channel();
        let handle = thread::spawn(move || opponent_think_worker(kind, seed, request_rx));
        Self {
            request_tx: Some(request_tx),
            pending: None,
            handle: Some(handle),
        }
    }

    fn request_move(&mut self, game: &Game, time_limit_ms: Option<u64>) {
        if let Some(pending) = self.pending.take() {
            pending.cancel.store(true, Ordering::SeqCst);
        }

        let (response_tx, response_rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let Some(request_tx) = self.request_tx.as_ref() else {
            return;
        };
        if request_tx
            .send(OpponentThinkRequest {
                game: game.clone(),
                time_limit_ms,
                cancel: cancel.clone(),
                response: response_tx,
            })
            .is_err()
        {
            self.pending = None;
            return;
        }

        self.pending = Some(PendingThought {
            cancel,
            response: response_rx,
        });
    }

    fn request_if_needed(&mut self, enabled: bool, game: &Game, time_limit_ms: Option<u64>) {
        if enabled {
            self.request_move(game, time_limit_ms);
        } else {
            self.cancel_pending();
        }
    }

    fn cancel_pending(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.cancel.store(true, Ordering::SeqCst);
        }
    }

    fn cancel_and_take_response(&mut self) -> Option<OpponentThinkResponse> {
        let pending = self.pending.take()?;
        pending.cancel.store(true, Ordering::SeqCst);
        pending.response.recv().ok()
    }

    fn take_move(&mut self) -> Option<OpponentThinkResponse> {
        let pending = self.pending.take()?;
        pending.response.recv().ok()
    }
}

impl Drop for OpponentThinker {
    fn drop(&mut self) {
        self.cancel_pending();
        self.request_tx = None;
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn opponent_think_worker(
    kind: BotKind,
    seed: u64,
    request_rx: mpsc::Receiver<OpponentThinkRequest>,
) {
    let mut bot = create_bot(kind, seed);
    while let Ok(request) = request_rx.recv() {
        bot.set_cancel_token(Some(request.cancel));
        bot.set_time_limit_ms(request.time_limit_ms);
        let direction = bot.choose_move(&request.game);
        let search_stats = bot.search_stats();
        let _ = request.response.send(OpponentThinkResponse {
            direction,
            search_stats,
        });
    }
}

fn step_bot_opponent_turn(
    opponent_game: &mut Game,
    opponent_state: &mut BotOpponentState,
    direction: Option<Direction>,
    visible_next: Option<&NextTile>,
    forced_rank: Option<u16>,
    mirror_human_next: bool,
) -> bool {
    if !opponent_state.active || opponent_game.is_game_over() {
        opponent_state.active = false;
        return false;
    }

    if let Some(visible_next) = visible_next {
        opponent_game.force_next_tile(visible_next.clone());
    }

    match direction {
        Some(bot_direction) => {
            let opponent_result = match forced_rank {
                Some(rank) => opponent_game.step_with_forced_next(bot_direction, rank),
                None => opponent_game.step(bot_direction),
            };
            opponent_state.last_move = opponent_result.spawn.as_ref().map(|spawn| LastMoveView {
                direction: bot_direction,
                tile_face: spawn.face,
                spawn_idx: Some(spawn.row * SIZE + spawn.col),
                merged_cells: opponent_result.merged_cells,
            });
            opponent_state.active = !opponent_result.game_over;
            if mirror_human_next {
                if let Some(visible_next) = visible_next {
                    opponent_game.force_next_tile(visible_next.clone());
                }
            }
        }
        None => {
            opponent_state.active = false;
            opponent_state.last_move = None;
        }
    }

    opponent_state.active && !opponent_game.is_game_over()
}

fn redraw_human_state(
    stdout: &mut Stdout,
    game: &Game,
    last_move: Option<&LastMoveView>,
    use_color: bool,
    message: &str,
    opponent: Option<(
        &Game,
        Option<&LastMoveView>,
        Option<&NextTile>,
        Option<&str>,
    )>,
) -> Result<()> {
    match opponent {
        Some((bot_game, bot_last_move, bot_next, bot_status)) => draw_dual_game(
            stdout,
            game,
            bot_game,
            last_move,
            bot_last_move,
            bot_next,
            bot_status,
            use_color,
            message,
        ),
        None => draw_game(stdout, game, last_move, use_color, message),
    }
}

fn human_move_budget_ms(elapsed: Duration) -> u64 {
    let millis = elapsed.as_millis();
    if millis == 0 {
        return 1;
    }
    u64::try_from(millis).unwrap_or(u64::MAX)
}

fn opponent_view(
    opponent: &Option<BotOpponent>,
) -> Option<(
    &Game,
    Option<&LastMoveView>,
    Option<&NextTile>,
    Option<&str>,
)> {
    opponent.as_ref().map(|opponent| {
        (
            &opponent.game,
            opponent.state.last_move.as_ref(),
            Some(opponent.game.next_tile()),
            opponent.state.last_status.as_deref(),
        )
    })
}

pub fn run_human(config: HumanConfig) -> Result<()> {
    let mut stdout = stdout();
    let use_color = match config.color {
        ColorMode::Auto => stdout.is_terminal(),
        ColorMode::Always => true,
        ColorMode::Never => false,
    };

    let mut logger = GameLogger::new(config.log_json.clone(), config.log_text.clone())?;
    let mut seed = next_seed(config.seed);
    let mut game = Game::new(seed);
    let mut bot_opponent = config.bot_opponent.map(|kind| BotOpponent {
        game: Game::new(seed),
        state: BotOpponentState {
            last_move: None,
            active: true,
            last_status: None,
        },
        thinker: OpponentThinker::new(kind, seed),
        bot_name: kind.name(),
    });
    let log_config = RunConfigLog {
        mode: "human".to_string(),
        seed_source: if config.seed.is_some() {
            "explicit".to_string()
        } else {
            "time".to_string()
        },
        speed_hz: config.speed_hz,
        color: config.color.as_str().to_string(),
        ab: None,
    };

    let _guard = TerminalGuard::activate(&mut stdout)?;
    logger.log_start(&game.snapshot(), &log_config)?;
    let mut last_move = None;
    let mut opponent_turn_time_limit_ms = None;

    redraw_human_state(
        &mut stdout,
        &game,
        last_move.as_ref(),
        use_color,
        "arrows, wasd/hjkl to move; q quit; r restart",
        opponent_view(&bot_opponent),
    )?;

    if let Some(opponent) = bot_opponent.as_mut() {
        opponent
            .thinker
            .request_move(&opponent.game, opponent_turn_time_limit_ms);
    }

    loop {
        let turn_started = Instant::now();
        match read_action()? {
            Action::Move(direction) => {
                let elapsed = turn_started.elapsed();
                let human_budget_ms = human_move_budget_ms(elapsed);
                let result = game.step(direction);
                let mut bot_search = None;
                if let Some(opponent) = bot_opponent.as_mut() {
                    if result.accepted {
                        let response = opponent.thinker.cancel_and_take_response();
                        opponent.state.last_status = response.as_ref().and_then(|resp| {
                            format_opponent_status(opponent.bot_name, resp.search_stats.clone())
                        });
                        bot_search = response.as_ref().and_then(|resp| {
                            resp.search_stats.as_ref().map(AbTurnStats::from_bot_stats)
                        });
                        let direction = response.and_then(|resp| resp.direction);
                        let forced_rank = result.spawn.as_ref().map_or_else(
                            || game.next_tile().single_rank().unwrap_or(3),
                            |spawn| spawn.rank,
                        );
                        step_bot_opponent_turn(
                            &mut opponent.game,
                            &mut opponent.state,
                            direction,
                            Some(game.next_tile()),
                            Some(forced_rank),
                            true,
                        );
                    }
                }
                logger.log_turn("human", &result, bot_search)?;
                if let Some(spawn) = result.spawn.as_ref() {
                    last_move = Some(LastMoveView {
                        direction,
                        tile_face: spawn.face,
                        spawn_idx: Some(spawn.row * SIZE + spawn.col),
                        merged_cells: result.merged_cells,
                    });
                }

                if result.accepted {
                    opponent_turn_time_limit_ms = Some(human_budget_ms);
                    if let Some(opponent) = bot_opponent.as_mut() {
                        if opponent.state.active {
                            opponent.thinker.request_if_needed(
                                opponent.state.active,
                                &opponent.game,
                                opponent_turn_time_limit_ms,
                            );
                        }
                    }
                }

                let redraw_message = if result.game_over {
                    game_over_message(&game)
                } else {
                    "arrows, wasd/hjkl to move; q quit; r restart".to_string()
                };
                redraw_human_state(
                    &mut stdout,
                    &game,
                    last_move.as_ref(),
                    use_color,
                    &redraw_message,
                    opponent_view(&bot_opponent),
                )?;

                if result.game_over {
                    if let Some(opponent) = bot_opponent.as_mut() {
                        while opponent.state.active && !opponent.game.is_game_over() {
                            let bot_next = opponent.game.next_tile().clone();
                            opponent.thinker.request_if_needed(
                                true,
                                &opponent.game,
                                Some(human_budget_ms),
                            );
                            let response = opponent.thinker.take_move();
                            opponent.state.last_status = response.as_ref().and_then(|resp| {
                                format_opponent_status(opponent.bot_name, resp.search_stats.clone())
                            });
                            let direction = response.and_then(|resp| resp.direction);
                            step_bot_opponent_turn(
                                &mut opponent.game,
                                &mut opponent.state,
                                direction,
                                Some(&bot_next),
                                None,
                                false,
                            );
                            if opponent.state.active && !opponent.game.is_game_over() {
                                let bot_next = opponent.game.next_tile();
                                redraw_human_state(
                                    &mut stdout,
                                    &game,
                                    last_move.as_ref(),
                                    use_color,
                                    "human finished, bot continues",
                                    Some((
                                        &opponent.game,
                                        opponent.state.last_move.as_ref(),
                                        Some(bot_next),
                                        opponent.state.last_status.as_deref(),
                                    )),
                                )?;
                                sleep(Duration::from_secs_f64(1.0 / config.speed_hz));
                            }
                        }

                        let bot_next = opponent.game.next_tile();
                        redraw_human_state(
                            &mut stdout,
                            &game,
                            last_move.as_ref(),
                            use_color,
                            &redraw_message,
                            Some((
                                &opponent.game,
                                opponent.state.last_move.as_ref(),
                                Some(bot_next),
                                opponent.state.last_status.as_deref(),
                            )),
                        )?;
                    }
                    logger.log_end("game_over", &game.snapshot())?;
                    wait_for_key()?;
                    break;
                }
            }
            Action::Quit => {
                if confirm(
                    &mut stdout,
                    &game,
                    last_move.as_ref(),
                    use_color,
                    "Quit? y/n",
                    opponent_view(&bot_opponent),
                )? {
                    logger.log_end("quit", &game.snapshot())?;
                    break;
                }
                redraw_human_state(
                    &mut stdout,
                    &game,
                    last_move.as_ref(),
                    use_color,
                    "arrows, wasd/hjkl to move; q quit; r restart",
                    opponent_view(&bot_opponent),
                )?;
            }
            Action::Restart => {
                if confirm(
                    &mut stdout,
                    &game,
                    last_move.as_ref(),
                    use_color,
                    "Restart? y/n",
                    opponent_view(&bot_opponent),
                )? {
                    logger.log_end("restart", &game.snapshot())?;
                    seed = next_seed(config.seed);
                    opponent_turn_time_limit_ms = None;
                    game = Game::new(seed);
                    bot_opponent = config.bot_opponent.map(|kind| BotOpponent {
                        game: Game::new(seed),
                        state: BotOpponentState {
                            last_move: None,
                            active: true,
                            last_status: None,
                        },
                        thinker: OpponentThinker::new(kind, seed),
                        bot_name: kind.name(),
                    });
                    if let Some(opponent) = bot_opponent.as_mut() {
                        opponent
                            .thinker
                            .request_move(&opponent.game, opponent_turn_time_limit_ms);
                    }
                    last_move = None;
                    logger.log_start(&game.snapshot(), &log_config)?;
                }
                redraw_human_state(
                    &mut stdout,
                    &game,
                    last_move.as_ref(),
                    use_color,
                    "arrows, wasd/hjkl to move; q quit; r restart",
                    opponent_view(&bot_opponent),
                )?;
            }
            Action::Terminate => {
                logger.log_end("terminated", &game.snapshot())?;
                break;
            }
            Action::None => {}
        }
    }

    logger.flush()?;
    Ok(())
}

pub fn run_observed_bot(config: BotConfig) -> Result<()> {
    let mut stdout = stdout();
    let use_color = match config.color {
        ColorMode::Auto => stdout.is_terminal(),
        ColorMode::Always => true,
        ColorMode::Never => false,
    };

    let mut logger = GameLogger::new(config.log_json.clone(), config.log_text.clone())?;
    let mut seed = next_seed(config.seed);
    let mut game = Game::new(seed);
    let mut bot = create_bot(config.bot, seed);
    let observer = ObserverConfig {
        speed_hz: config.speed_hz,
    };
    let log_config = RunConfigLog {
        mode: format!("bot:{}", bot.name()),
        seed_source: if config.seed.is_some() {
            "explicit".to_string()
        } else {
            "time".to_string()
        },
        speed_hz: config.speed_hz,
        color: config.color.as_str().to_string(),
        ab: ab_log_config(config.bot),
    };

    let _guard = TerminalGuard::activate(&mut stdout)?;
    logger.log_start(&game.snapshot(), &log_config)?;
    let mut last_move = None;
    draw_game(
        &mut stdout,
        &game,
        last_move.as_ref(),
        use_color,
        &bot_status(bot.as_ref(), config.speed_hz, &game),
    )?;

    loop {
        match poll_action(observer.move_delay())? {
            Action::Quit => {
                logger.log_end("quit", &game.snapshot())?;
                break;
            }
            Action::Restart => {
                logger.log_end("restart", &game.snapshot())?;
                seed = next_seed(config.seed);
                game = Game::new(seed);
                bot = create_bot(config.bot, seed);
                last_move = None;
                logger.log_start(&game.snapshot(), &log_config)?;
                draw_game(
                    &mut stdout,
                    &game,
                    last_move.as_ref(),
                    use_color,
                    &bot_status(bot.as_ref(), config.speed_hz, &game),
                )?;
                continue;
            }
            Action::Terminate => {
                logger.log_end("terminated", &game.snapshot())?;
                break;
            }
            Action::Move(_) | Action::None => {}
        }

        let Some(direction) = bot.choose_move(&game) else {
            draw_game(
                &mut stdout,
                &game,
                last_move.as_ref(),
                use_color,
                &game_over_message(&game),
            )?;
            logger.log_end("game_over", &game.snapshot())?;
            wait_for_key()?;
            break;
        };

        let result = game.step(direction);
        let stats = bot
            .search_stats()
            .map(|search| AbTurnStats::from_bot_stats(&search));
        logger.log_turn("bot", &result, stats)?;
        if let Some(spawn) = result.spawn.as_ref() {
            last_move = Some(LastMoveView {
                direction,
                tile_face: spawn.face,
                spawn_idx: Some(spawn.row * SIZE + spawn.col),
                merged_cells: result.merged_cells,
            });
        }
        if result.game_over {
            draw_game(
                &mut stdout,
                &game,
                last_move.as_ref(),
                use_color,
                &game_over_message(&game),
            )?;
            logger.log_end("game_over", &game.snapshot())?;
            wait_for_key()?;
            break;
        }
        draw_game(
            &mut stdout,
            &game,
            last_move.as_ref(),
            use_color,
            &bot_status(bot.as_ref(), config.speed_hz, &game),
        )?;
    }

    logger.flush()?;
    Ok(())
}

fn next_seed(seed: Option<u64>) -> u64 {
    seed.unwrap_or_else(time_seed)
}

enum Action {
    Move(Direction),
    Quit,
    Restart,
    Terminate,
    None,
}

fn read_action() -> Result<Action> {
    loop {
        let event = event::read().context("failed to read terminal input")?;
        let Event::Key(key) = event else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        return Ok(key_to_action(key));
    }
}

fn poll_action(timeout: Duration) -> Result<Action> {
    if !event::poll(timeout).context("failed to poll terminal input")? {
        return Ok(Action::None);
    }
    let Event::Key(key) = event::read().context("failed to read terminal input")? else {
        return Ok(Action::None);
    };
    if key.kind != KeyEventKind::Press {
        return Ok(Action::None);
    }
    Ok(key_to_action(key))
}

fn wait_for_key() -> Result<()> {
    loop {
        let event = event::read().context("failed to read terminal input")?;
        let Event::Key(key) = event else {
            continue;
        };
        if key.kind == KeyEventKind::Press {
            return Ok(());
        }
    }
}

fn key_to_action(key: KeyEvent) -> Action {
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
        return Action::Terminate;
    }

    match key.code {
        KeyCode::Left | KeyCode::Char('a') | KeyCode::Char('h') => Action::Move(Direction::Left),
        KeyCode::Right | KeyCode::Char('d') | KeyCode::Char('l') => Action::Move(Direction::Right),
        KeyCode::Up | KeyCode::Char('w') | KeyCode::Char('k') => Action::Move(Direction::Up),
        KeyCode::Down | KeyCode::Char('s') | KeyCode::Char('j') => Action::Move(Direction::Down),
        KeyCode::Char('q') => Action::Quit,
        KeyCode::Char('r') => Action::Restart,
        _ => Action::None,
    }
}

fn game_over_message(game: &Game) -> String {
    format!(
        "GAME OVER - final score {} - press any key to exit",
        game.score()
    )
}

fn bot_status(bot: &dyn Bot, speed_hz: f64, game: &Game) -> String {
    let mut status = format!("bot {} running at {speed_hz} Hz", bot.name());
    let mut parts: Vec<String> = Vec::new();

    if let Some(eval) = bot.board_eval(game) {
        parts.push(format!("eval {eval:.1}"));
    }

    if let Some(stats) = bot.search_stats() {
        let pruned_after = stats.predicted_states.saturating_sub(stats.pruned_states);
        parts.push(format!("d {}", stats.searched_depth));
        parts.push(format!(
            "pred {} ({} pruned)",
            pruned_after, stats.pruned_states
        ));
        parts.push(format!(
            "cache {}h/{}m",
            stats.cache_hits, stats.cache_misses
        ));
        parts.push(format!("best {:.1}", stats.selected_score));
        if let Some(layer_summary) = format_state_layers(&stats) {
            parts.push(layer_summary);
        }
    }

    if !parts.is_empty() {
        status.push_str(" [");
        status.push_str(&parts.join(", "));
        status.push(']');
    }

    status.push_str("; q quit; r restart");
    status
}

fn format_opponent_status(
    name: &'static str,
    stats: Option<crate::bot::AbSearchStats>,
) -> Option<String> {
    let stats = stats?;
    let mut status = format!("bot {name}");
    let mut parts: Vec<String> = Vec::new();

    let eval = stats.board_eval;
    parts.push(format!("eval {eval:.1}"));

    let pruned_after = stats.predicted_states.saturating_sub(stats.pruned_states);
    parts.push(format!("d {}", stats.searched_depth));
    parts.push(format!(
        "pred {} ({} pruned)",
        pruned_after, stats.pruned_states
    ));
    parts.push(format!(
        "cache {}h/{}m",
        stats.cache_hits, stats.cache_misses
    ));
    parts.push(format!("best {:.1}", stats.selected_score));
    if let Some(layer_summary) = format_state_layers(&stats) {
        parts.push(layer_summary);
    }

    if !parts.is_empty() {
        status.push_str(" [");
        status.push_str(&parts.join(", "));
        status.push(']');
    }

    Some(status)
}

fn format_state_layers(stats: &crate::bot::AbSearchStats) -> Option<String> {
    if stats.states_per_layer.is_empty() {
        return None;
    }

    let mut parts: Vec<String> = Vec::new();
    let max_depth = stats.searched_depth.max(stats.states_per_layer.len() as u8);
    for layer in 0..usize::from(max_depth) {
        if layer >= stats.states_per_layer.len() {
            break;
        }
        if layer == 0 {
            let exact = stats.states_per_layer[0];
            parts.push(format!("{}", exact));
        } else if layer < stats.states_per_layer.len() {
            let parent = stats.states_per_layer[layer - 1];
            let current = stats.states_per_layer[layer];
            let avg = if parent == 0 {
                0
            } else {
                ((current as f64 / parent as f64).ceil()) as u64
            };
            parts.push(format!("{}", avg));
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

fn ab_log_config(bot: BotKind) -> Option<AbConfigLog> {
    match bot {
        BotKind::Random => None,
        BotKind::Ab(AbConfig {
            depth,
            alpha,
            beta,
            dfs,
            time_limit_ms,
            node_limit,
        }) => Some(AbConfigLog {
            depth,
            alpha: log_bound(alpha),
            beta: log_bound(beta),
            dfs,
            time_limit_ms,
            node_limit,
        }),
    }
}

fn log_bound(value: f64) -> String {
    if value == f64::INFINITY {
        "inf".to_string()
    } else if value == f64::NEG_INFINITY {
        "-inf".to_string()
    } else {
        value.to_string()
    }
}

fn confirm(
    stdout: &mut Stdout,
    game: &Game,
    last_move: Option<&LastMoveView>,
    use_color: bool,
    message: &str,
    opponent: Option<(
        &Game,
        Option<&LastMoveView>,
        Option<&NextTile>,
        Option<&str>,
    )>,
) -> Result<bool> {
    redraw_human_state(stdout, game, last_move, use_color, message, opponent)?;
    loop {
        let event = event::read().context("failed to read terminal input")?;
        let Event::Key(key) = event else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => return Ok(true),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => return Ok(false),
            _ => {}
        }
    }
}

fn draw_game(
    stdout: &mut Stdout,
    game: &Game,
    last_move: Option<&LastMoveView>,
    use_color: bool,
    message: &str,
) -> Result<()> {
    let previous = last_move
        .map(last_move_label)
        .unwrap_or_else(|| "previous -".to_string());
    let bonus = bonus_forecast_label(&game.bonus_forecast(DEFAULT_BONUS_FORECAST_HORIZON));

    queue!(stdout, MoveTo(0, 0), Clear(ClearType::All))?;
    queue!(
        stdout,
        Print(format!("Threes {:>18} pts{}", game.score(), NL)),
        Print(format!(
            "seed {}  moves {}  high {}{}",
            game.seed(),
            game.accepted_moves(),
            game.high_tile(),
            NL
        )),
        Print(format!(
            "next {}  {}  bonus {}{}{}",
            game.next_tile().display_label(),
            previous,
            bonus,
            NL,
            NL
        ))
    )?;

    let board = game.board();
    for row in 0..SIZE {
        for tile_row in 0..TILE_HEIGHT {
            draw_tile_row(
                stdout,
                &board,
                row,
                tile_row,
                last_move.and_then(|last| last.spawn_idx),
                last_move.map_or(&[false; SIZE * SIZE], |last| &last.merged_cells),
                use_color,
            )?;
        }
    }

    queue!(stdout, Print(NL))?;
    draw_next_tile(stdout, game.next_tile(), use_color)?;
    queue!(stdout, Print(format!("{}{}{}", NL, message, NL)))?;
    stdout.flush()?;
    Ok(())
}

fn draw_dual_game(
    stdout: &mut Stdout,
    human_game: &Game,
    bot_game: &Game,
    human_last_move: Option<&LastMoveView>,
    bot_last_move: Option<&LastMoveView>,
    bot_next: Option<&NextTile>,
    bot_status: Option<&str>,
    use_color: bool,
    message: &str,
) -> Result<()> {
    const BOARD_WIDTH: usize = TILE_WIDTH * SIZE;
    const GUTTER: usize = BOARD_GUTTER;

    let human_score = format!("you {:>6} pts", human_game.score());
    let bot_score = format!("bot {:>6} pts", bot_game.score());
    let human_meta = format!(
        "seed {}  moves {}  high {}  next {}",
        human_game.seed(),
        human_game.accepted_moves(),
        human_game.high_tile(),
        human_game.next_tile().display_label()
    );
    let bot_meta = format!(
        "seed {}  moves {}  high {}  next {}",
        bot_game.seed(),
        bot_game.accepted_moves(),
        bot_game.high_tile(),
        bot_next
            .map(|next| next.display_label())
            .unwrap_or_else(|| bot_game.next_tile().display_label())
    );
    let human_prev = human_last_move
        .map(last_move_label)
        .unwrap_or_else(|| "previous -".to_string());
    let bot_prev = bot_last_move
        .map(last_move_label)
        .unwrap_or_else(|| "previous -".to_string());

    queue!(stdout, MoveTo(0, 0), Clear(ClearType::All))?;
    queue!(
        stdout,
        Print(format!(
            "{:<width$}{:<gap$}{:<width$}{}",
            human_score,
            "",
            bot_score,
            NL,
            width = BOARD_WIDTH,
            gap = GUTTER
        )),
        Print(format!(
            "{:<width$}{:<gap$}{:<width$}{}",
            human_meta,
            "",
            bot_meta,
            NL,
            width = BOARD_WIDTH,
            gap = GUTTER
        )),
        Print(format!(
            "{:<width$}{:<gap$}{:<width$}{}",
            human_prev,
            "",
            bot_prev,
            NL,
            width = BOARD_WIDTH,
            gap = GUTTER
        ))
    )?;

    let human_board = human_game.board();
    let bot_board = bot_game.board();
    for row in 0..SIZE {
        for tile_row in 0..TILE_HEIGHT {
            draw_tile_row_dual(
                stdout,
                &human_board,
                &bot_board,
                row,
                tile_row,
                human_last_move.and_then(|last| last.spawn_idx),
                human_last_move.map_or(&[false; SIZE * SIZE], |last| &last.merged_cells),
                bot_last_move.and_then(|last| last.spawn_idx),
                bot_last_move.map_or(&[false; SIZE * SIZE], |last| &last.merged_cells),
                use_color,
            )?;
        }
    }

    queue!(stdout, Print(NL))?;
    draw_next_pair(
        stdout,
        human_game.next_tile(),
        bot_next.unwrap_or_else(|| bot_game.next_tile()),
        use_color,
    )?;
    let footer = if let Some(bot_status) = bot_status {
        format!("{}  |  {}{}", message, bot_status, NL)
    } else {
        format!("{}{}", message, NL)
    };
    queue!(stdout, Print(footer))?;
    stdout.flush()?;
    Ok(())
}

fn draw_tile_row(
    stdout: &mut Stdout,
    board: &Board,
    row: usize,
    tile_row: usize,
    spawn_idx: Option<usize>,
    merged_cells: &[bool; SIZE * SIZE],
    use_color: bool,
) -> Result<()> {
    for col in 0..SIZE {
        let rank = board.get(row, col);
        let idx = row * SIZE + col;
        let bold = Some(idx) == spawn_idx;
        let underline = merged_cells[idx];
        let label = if tile_row == TILE_LABEL_ROW {
            tile_label(rank)
        } else {
            " ".repeat(TILE_WIDTH)
        };
        draw_tile(stdout, rank, &label, bold, underline, use_color)?;
    }
    queue!(stdout, Print(NL))?;
    Ok(())
}

fn draw_tile_row_dual(
    stdout: &mut Stdout,
    human: &Board,
    bot: &Board,
    row: usize,
    tile_row: usize,
    human_spawn_idx: Option<usize>,
    human_merged_cells: &[bool; SIZE * SIZE],
    bot_spawn_idx: Option<usize>,
    bot_merged_cells: &[bool; SIZE * SIZE],
    use_color: bool,
) -> Result<()> {
    for col in 0..SIZE {
        let rank = human.get(row, col);
        let idx = row * SIZE + col;
        let bold = Some(idx) == human_spawn_idx;
        let underline = human_merged_cells[idx];
        let label = if tile_row == TILE_LABEL_ROW {
            tile_label(rank)
        } else {
            " ".repeat(TILE_WIDTH)
        };
        draw_tile(stdout, rank, &label, bold, underline, use_color)?;
    }

    queue!(stdout, Print(" ".repeat(BOARD_GUTTER)))?;
    for col in 0..SIZE {
        let rank = bot.get(row, col);
        let idx = row * SIZE + col;
        let bold = Some(idx) == bot_spawn_idx;
        let underline = bot_merged_cells[idx];
        let label = if tile_row == TILE_LABEL_ROW {
            tile_label(rank)
        } else {
            " ".repeat(TILE_WIDTH)
        };
        draw_tile(stdout, rank, &label, bold, underline, use_color)?;
    }
    queue!(stdout, Print(NL))?;
    Ok(())
}

fn draw_next_tile(stdout: &mut Stdout, next: &NextTile, use_color: bool) -> Result<()> {
    queue!(stdout, Print("next "))?;
    draw_next_value(stdout, next, use_color)
}

fn draw_next_pair(
    stdout: &mut Stdout,
    human_next: &NextTile,
    bot_next: &NextTile,
    use_color: bool,
) -> Result<()> {
    let board_width = TILE_WIDTH * SIZE;
    let human_prefix = "you next ";
    let bot_prefix = "bot next ";
    let pad_between = board_width.saturating_sub(human_prefix.len() + TILE_WIDTH) + BOARD_GUTTER;

    queue!(stdout, Print(human_prefix))?;
    draw_next_value(stdout, human_next, use_color)?;
    queue!(stdout, Print(" ".repeat(pad_between)))?;
    queue!(stdout, Print(bot_prefix))?;
    draw_next_value(stdout, bot_next, use_color)?;
    queue!(stdout, Print(NL))?;
    Ok(())
}

fn draw_next_value(stdout: &mut Stdout, next: &NextTile, use_color: bool) -> Result<()> {
    if let Some(rank) = next.single_rank() {
        draw_tile(
            stdout,
            rank,
            &center_label(&rank_to_face(rank).to_string()),
            false,
            false,
            use_color,
        )?;
        return Ok(());
    }

    match next {
        NextTile::Bonus { .. } => {
            draw_tile(stdout, 3, &center_label("+"), false, false, use_color)?;
        }
        _ => unreachable!("unexpected next tile variant"),
    }
    Ok(())
}

fn draw_tile(
    stdout: &mut Stdout,
    rank: u16,
    label: &str,
    bold: bool,
    underline: bool,
    use_color: bool,
) -> Result<()> {
    let label = fit_label(label);
    let chars: Vec<(usize, char)> = label.char_indices().collect();
    let first_non_space = chars.iter().position(|(_, ch)| !ch.is_whitespace());
    let last_non_space = chars.iter().rposition(|(_, ch)| !ch.is_whitespace());
    if use_color {
        let (fg, bg) = tile_colors(rank);
        queue!(stdout, SetForegroundColor(fg), SetBackgroundColor(bg))?;
    }
    if bold {
        queue!(stdout, SetAttribute(Attribute::Bold))?;
    }

    match (first_non_space, last_non_space) {
        (Some(start), Some(end)) if end >= start => {
            let (start_offset, end_offset) =
                (chars[start].0, chars[end].0 + chars[end].1.len_utf8());
            let head = &label[..start_offset];
            let body = &label[start_offset..end_offset];
            let tail = &label[end_offset..];
            queue!(stdout, Print(head))?;
            if underline {
                queue!(stdout, SetAttribute(Attribute::Underlined))?;
            }
            queue!(stdout, Print(body))?;
            if underline {
                queue!(stdout, SetAttribute(Attribute::NoUnderline))?;
            }
            if !tail.is_empty() {
                queue!(stdout, Print(tail))?;
            }
        }
        _ => queue!(stdout, Print(label))?,
    }
    queue!(stdout, SetAttribute(Attribute::Reset), ResetColor)?;
    Ok(())
}

fn last_move_label(last_move: &LastMoveView) -> String {
    format!(
        "previous {} {}",
        direction_arrow(last_move.direction),
        last_move.tile_face
    )
}

fn direction_arrow(direction: Direction) -> &'static str {
    match direction {
        Direction::Up => "↑",
        Direction::Down => "↓",
        Direction::Left => "←",
        Direction::Right => "→",
    }
}

fn bonus_forecast_label(forecast: &BonusForecast) -> String {
    if !forecast.unlocked {
        return "locked".to_string();
    }

    let Some(slot) = forecast.slots.iter().find(|slot| slot.probability > 0.0) else {
        return "none".to_string();
    };

    format!(
        "+{} {:.1}%",
        slot.accepted_moves_from_now,
        slot.probability * 100.0
    )
}

fn tile_label(rank: u16) -> String {
    if rank == 0 {
        center_label(".")
    } else {
        center_label(&rank_to_face(rank).to_string())
    }
}

fn center_label(label: &str) -> String {
    let visible_len = label.chars().count();
    if visible_len >= TILE_WIDTH {
        return label.chars().take(TILE_WIDTH).collect();
    }
    let left = (TILE_WIDTH - visible_len) / 2;
    let right = TILE_WIDTH - visible_len - left;
    format!("{}{}{}", " ".repeat(left), label, " ".repeat(right))
}

fn fit_label(label: &str) -> String {
    let len = label.chars().count();
    match len.cmp(&TILE_WIDTH) {
        std::cmp::Ordering::Less => format!("{}{}", label, " ".repeat(TILE_WIDTH - len)),
        std::cmp::Ordering::Equal => label.to_string(),
        std::cmp::Ordering::Greater => label.chars().take(TILE_WIDTH).collect(),
    }
}

fn tile_colors(rank: u16) -> (Color, Color) {
    match rank {
        0 => (Color::AnsiValue(250), Color::AnsiValue(238)),
        1 => (Color::White, Color::AnsiValue(25)),
        2 => (Color::White, Color::AnsiValue(160)),
        3..=5 => (Color::Black, Color::AnsiValue(253)),
        6..=8 => (Color::Black, Color::AnsiValue(250)),
        9..=11 => (Color::White, Color::AnsiValue(244)),
        _ => (Color::White, Color::AnsiValue(239)),
    }
}

struct TerminalGuard;

impl TerminalGuard {
    fn activate(stdout: &mut Stdout) -> Result<Self> {
        terminal::enable_raw_mode().context("failed to enable raw terminal mode")?;
        execute!(stdout, Hide, Clear(ClearType::All), MoveTo(0, 0))?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let mut stdout = stdout();
        let _ = execute!(stdout, Show, ResetColor, Print(NL));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::Bot;

    struct StaticBot(Direction);

    impl Bot for StaticBot {
        fn name(&self) -> &'static str {
            "static"
        }

        fn choose_move(&mut self, game: &Game) -> Option<Direction> {
            let legal = game.legal_directions();
            if legal.contains(&self.0) {
                Some(self.0)
            } else {
                None
            }
        }
    }

    #[test]
    fn observer_speed_maps_to_delay() {
        let observer = ObserverConfig { speed_hz: 4.0 };
        assert_eq!(observer.move_delay(), Duration::from_millis(250));
    }

    #[test]
    fn tile_labels_fit_fixed_width() {
        assert_eq!(center_label("3").chars().count(), TILE_WIDTH);
        assert_eq!(center_label("12288").chars().count(), TILE_WIDTH);
        assert_eq!(fit_label("1234567890").chars().count(), TILE_WIDTH);
    }

    #[test]
    fn tile_shape_is_terminal_square() {
        assert_eq!(TILE_WIDTH, TILE_HEIGHT * 2);
    }

    #[test]
    fn game_over_message_contains_final_score() {
        let game = Game::new(123);
        assert!(game_over_message(&game).contains("final score"));
        assert!(game_over_message(&game).contains(&game.score().to_string()));
    }

    #[test]
    fn bot_status_mentions_bot_and_speed() {
        let bot = create_bot(BotKind::Random, 123);
        let game = Game::new(123);
        let status = bot_status(bot.as_ref(), 4.0, &game);
        assert!(status.contains("random"));
        assert!(status.contains("4"));
    }

    #[test]
    fn bot_status_includes_ab_stats_after_move() {
        let mut bot = create_bot(BotKind::Ab(AbConfig::default()), 123);
        let game = Game::new(123);
        bot.choose_move(&game);

        let status = bot_status(bot.as_ref(), 4.0, &game);
        assert!(status.contains("pred"));
        assert!(status.contains("cache"));
        assert!(status.contains("best"));
        assert!(status.contains(";"));
        assert!(status.contains("/"));
    }

    #[test]
    fn direction_arrows_are_utf8_symbols() {
        assert_eq!(direction_arrow(Direction::Up), "↑");
        assert_eq!(direction_arrow(Direction::Down), "↓");
        assert_eq!(direction_arrow(Direction::Left), "←");
        assert_eq!(direction_arrow(Direction::Right), "→");
    }

    #[test]
    fn last_move_label_shows_face_value_and_arrow() {
        let label = last_move_label(&LastMoveView {
            direction: Direction::Left,
            tile_face: 48,
            spawn_idx: None,
            merged_cells: [false; SIZE * SIZE],
        });
        assert_eq!(label, "previous ← 48");
    }

    #[test]
    fn bonus_forecast_label_shows_first_nonzero_probability() {
        let forecast = BonusForecast {
            unlocked: true,
            cycle_position: Some(0),
            bonus_seen_this_cycle: false,
            slots: vec![
                crate::game::BonusForecastSlot {
                    accepted_moves_from_now: 0,
                    probability: 0.0,
                },
                crate::game::BonusForecastSlot {
                    accepted_moves_from_now: 1,
                    probability: 0.05,
                },
            ],
        };

        assert_eq!(bonus_forecast_label(&forecast), "+1 5.0%");
    }

    #[test]
    fn step_bot_turn_without_mirroring_updates_opponent_next() {
        let mut opponent_game = Game::new(123);
        let legal = opponent_game.legal_directions();
        let Some(direction) = legal.first().copied() else {
            panic!("game should have a legal move in test fixture");
        };

        let mut expected = opponent_game.clone();
        expected.step(direction);

        let mut state = BotOpponentState {
            last_move: None,
            active: true,
            last_status: None,
        };
        let mut bot = StaticBot(direction);
        let direction = bot.choose_move(&opponent_game);
        step_bot_opponent_turn(&mut opponent_game, &mut state, direction, None, None, false);

        assert_eq!(
            opponent_game.next_tile().display_label(),
            expected.next_tile().display_label()
        );
        assert_eq!(opponent_game.score(), expected.score());
        assert_eq!(opponent_game.board(), expected.board());
    }
}
