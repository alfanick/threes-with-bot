use crate::board::{rank_to_face, Board, Direction, SIZE};
use crate::bot::{create_bot, AbConfig, Bot, BotKind};
use crate::game::{time_seed, BonusForecast, Game, NextTile, DEFAULT_BONUS_FORECAST_HORIZON};
use crate::logging::{AbConfigLog, GameLogger, RunConfigLog};
use anyhow::{Context, Result};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{Color, Print, ResetColor, SetBackgroundColor, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{execute, queue};
use std::io::{stdout, IsTerminal, Stdout, Write};
use std::path::PathBuf;
use std::thread::sleep;
use std::time::Duration;

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
}

impl ObserverConfig {
    pub fn move_delay(self) -> Duration {
        Duration::from_secs_f64(1.0 / self.speed_hz)
    }
}

#[derive(Clone, Copy)]
struct BotOpponentState {
    last_move: Option<LastMoveView>,
    active: bool,
}

fn step_bot_opponent_turn(
    bot: &mut dyn Bot,
    opponent_game: &mut Game,
    opponent_state: &mut BotOpponentState,
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
    match bot.choose_move(opponent_game) {
        Some(bot_direction) => {
            let opponent_result = match forced_rank {
                Some(rank) => opponent_game.step_with_forced_next(bot_direction, rank),
                None => opponent_game.step(bot_direction),
            };
            opponent_state.last_move = opponent_result.spawn.as_ref().map(|spawn| LastMoveView {
                direction: bot_direction,
                tile_face: spawn.face,
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
    opponent: Option<(&Game, Option<&LastMoveView>, Option<&NextTile>)>,
) -> Result<()> {
    match opponent {
        Some((bot_game, bot_last_move, bot_next)) => draw_dual_game(
            stdout,
            game,
            bot_game,
            last_move,
            bot_last_move,
            bot_next,
            use_color,
            message,
        ),
        None => draw_game(stdout, game, last_move, use_color, message),
    }
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
    let mut bot_opponent = config.bot_opponent.map(|kind| {
        (
            create_bot(kind, seed),
            Game::new(seed),
            BotOpponentState {
                last_move: None,
                active: true,
            },
        )
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
    redraw_human_state(
        &mut stdout,
        &game,
        last_move.as_ref(),
        use_color,
        "arrows, wasd/hjkl to move; q quit; r restart",
        bot_opponent
            .as_ref()
            .map(|(_, opponent_game, opponent_state)| {
                (
                    opponent_game,
                    opponent_state.last_move.as_ref(),
                    Some(opponent_game.next_tile()),
                )
            }),
    )?;

    loop {
        match read_action()? {
            Action::Move(direction) => {
                let result = game.step(direction);
                logger.log_turn(&result)?;
                if let Some(spawn) = result.spawn.as_ref() {
                    last_move = Some(LastMoveView {
                        direction,
                        tile_face: spawn.face,
                    });
                }

                if result.accepted {
                    let forced_rank = result.spawn.as_ref().map_or_else(
                        || game.next_tile().single_rank().unwrap_or(3),
                        |spawn| spawn.rank,
                    );
                    if let Some((bot, opponent_game, opponent_state)) = bot_opponent.as_mut() {
                        step_bot_opponent_turn(
                            bot.as_mut(),
                            opponent_game,
                            opponent_state,
                            Some(game.next_tile()),
                            Some(forced_rank),
                            true,
                        );
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
                    bot_opponent
                        .as_ref()
                        .map(|(_, opponent_game, opponent_state)| {
                            (
                                opponent_game,
                                opponent_state.last_move.as_ref(),
                                Some(opponent_game.next_tile()),
                            )
                        }),
                )?;

                if result.game_over {
                    if let Some((bot, opponent_game, opponent_state)) = bot_opponent.as_mut() {
                        while step_bot_opponent_turn(
                            bot.as_mut(),
                            opponent_game,
                            opponent_state,
                            None,
                            None,
                            false,
                        ) {
                            redraw_human_state(
                                &mut stdout,
                                &game,
                                last_move.as_ref(),
                                use_color,
                                "human finished, bot continues",
                                Some((
                                    opponent_game,
                                    opponent_state.last_move.as_ref(),
                                    Some(opponent_game.next_tile()),
                                )),
                            )?;
                            sleep(Duration::from_secs_f64(1.0 / config.speed_hz));
                        }

                        redraw_human_state(
                            &mut stdout,
                            &game,
                            last_move.as_ref(),
                            use_color,
                            &redraw_message,
                            Some((
                                opponent_game,
                                opponent_state.last_move.as_ref(),
                                Some(opponent_game.next_tile()),
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
                    bot_opponent
                        .as_ref()
                        .map(|(_, opponent_game, opponent_state)| {
                            (
                                opponent_game,
                                opponent_state.last_move.as_ref(),
                                Some(opponent_game.next_tile()),
                            )
                        }),
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
                    bot_opponent
                        .as_ref()
                        .map(|(_, opponent_game, opponent_state)| {
                            (
                                opponent_game,
                                opponent_state.last_move.as_ref(),
                                Some(opponent_game.next_tile()),
                            )
                        }),
                )?;
            }
            Action::Restart => {
                if confirm(
                    &mut stdout,
                    &game,
                    last_move.as_ref(),
                    use_color,
                    "Restart? y/n",
                    bot_opponent
                        .as_ref()
                        .map(|(_, opponent_game, opponent_state)| {
                            (
                                opponent_game,
                                opponent_state.last_move.as_ref(),
                                Some(opponent_game.next_tile()),
                            )
                        }),
                )? {
                    logger.log_end("restart", &game.snapshot())?;
                    seed = next_seed(config.seed);
                    game = Game::new(seed);
                    bot_opponent = config.bot_opponent.map(|kind| {
                        (
                            create_bot(kind, seed),
                            Game::new(seed),
                            BotOpponentState {
                                last_move: None,
                                active: true,
                            },
                        )
                    });
                    last_move = None;
                    logger.log_start(&game.snapshot(), &log_config)?;
                }
                redraw_human_state(
                    &mut stdout,
                    &game,
                    last_move.as_ref(),
                    use_color,
                    "arrows, wasd/hjkl to move; q quit; r restart",
                    bot_opponent
                        .as_ref()
                        .map(|(_, opponent_game, opponent_state)| {
                            (
                                opponent_game,
                                opponent_state.last_move.as_ref(),
                                Some(opponent_game.next_tile()),
                            )
                        }),
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
        logger.log_turn(&result)?;
        if let Some(spawn) = result.spawn.as_ref() {
            last_move = Some(LastMoveView {
                direction,
                tile_face: spawn.face,
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
    }

    if !parts.is_empty() {
        status.push_str(" [");
        status.push_str(&parts.join(", "));
        status.push(']');
    }

    status.push_str("; q quit; r restart");
    status
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
    opponent: Option<(&Game, Option<&LastMoveView>, Option<&NextTile>)>,
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
            draw_tile_row(stdout, &board, row, tile_row, use_color)?;
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
            draw_tile_row_dual(stdout, &human_board, &bot_board, row, tile_row, use_color)?;
        }
    }

    queue!(stdout, Print(NL))?;
    draw_next_pair(
        stdout,
        human_game.next_tile(),
        bot_next.unwrap_or_else(|| bot_game.next_tile()),
        use_color,
    )?;
    queue!(stdout, Print(format!("{}{}", message, NL)))?;
    stdout.flush()?;
    Ok(())
}

fn draw_tile_row(
    stdout: &mut Stdout,
    board: &Board,
    row: usize,
    tile_row: usize,
    use_color: bool,
) -> Result<()> {
    for col in 0..SIZE {
        let rank = board.get(row, col);
        let label = if tile_row == TILE_LABEL_ROW {
            tile_label(rank)
        } else {
            " ".repeat(TILE_WIDTH)
        };
        draw_tile(stdout, rank, &label, use_color)?;
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
    use_color: bool,
) -> Result<()> {
    for col in 0..SIZE {
        let rank = human.get(row, col);
        let label = if tile_row == TILE_LABEL_ROW {
            tile_label(rank)
        } else {
            " ".repeat(TILE_WIDTH)
        };
        draw_tile(stdout, rank, &label, use_color)?;
    }

    queue!(stdout, Print(" ".repeat(BOARD_GUTTER)))?;
    for col in 0..SIZE {
        let rank = bot.get(row, col);
        let label = if tile_row == TILE_LABEL_ROW {
            tile_label(rank)
        } else {
            " ".repeat(TILE_WIDTH)
        };
        draw_tile(stdout, rank, &label, use_color)?;
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
            use_color,
        )?;
        return Ok(());
    }

    match next {
        NextTile::Bonus { .. } => {
            draw_tile(stdout, 3, &center_label("+"), use_color)?;
        }
        _ => unreachable!("unexpected next tile variant"),
    }
    Ok(())
}

fn draw_tile(stdout: &mut Stdout, rank: u16, label: &str, use_color: bool) -> Result<()> {
    let label = fit_label(label);
    if use_color {
        let (fg, bg) = tile_colors(rank);
        queue!(
            stdout,
            SetForegroundColor(fg),
            SetBackgroundColor(bg),
            Print(label),
            ResetColor
        )?;
    } else {
        queue!(stdout, Print(label))?;
    }
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
        };
        let mut bot = StaticBot(direction);
        step_bot_opponent_turn(&mut bot, &mut opponent_game, &mut state, None, None, false);

        assert_eq!(
            opponent_game.next_tile().display_label(),
            expected.next_tile().display_label()
        );
        assert_eq!(opponent_game.score(), expected.score());
        assert_eq!(opponent_game.board(), expected.board());
    }
}
