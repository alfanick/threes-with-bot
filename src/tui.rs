use crate::board::{rank_to_face, Board, Direction, SIZE};
use crate::bot::{create_bot, AbConfig, BotKind};
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
use std::time::Duration;

const TILE_WIDTH: usize = 8;
const TILE_HEIGHT: usize = 4;
const TILE_LABEL_ROW: usize = 1;
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
    draw_game(
        &mut stdout,
        &game,
        last_move.as_ref(),
        use_color,
        "arrows, wasd/hjkl to move; q quit; r restart",
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
                    "arrows, wasd/hjkl to move; q quit; r restart",
                )?;
            }
            Action::Quit => {
                if confirm(
                    &mut stdout,
                    &game,
                    last_move.as_ref(),
                    use_color,
                    "Quit? y/n",
                )? {
                    logger.log_end("quit", &game.snapshot())?;
                    break;
                }
                draw_game(
                    &mut stdout,
                    &game,
                    last_move.as_ref(),
                    use_color,
                    "arrows, wasd/hjkl to move; q quit; r restart",
                )?;
            }
            Action::Restart => {
                if confirm(
                    &mut stdout,
                    &game,
                    last_move.as_ref(),
                    use_color,
                    "Restart? y/n",
                )? {
                    logger.log_end("restart", &game.snapshot())?;
                    seed = next_seed(config.seed);
                    game = Game::new(seed);
                    last_move = None;
                    logger.log_start(&game.snapshot(), &log_config)?;
                }
                draw_game(
                    &mut stdout,
                    &game,
                    last_move.as_ref(),
                    use_color,
                    "arrows, wasd/hjkl to move; q quit; r restart",
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
        &bot_status(bot.name(), config.speed_hz),
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
                    &bot_status(bot.name(), config.speed_hz),
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
            &bot_status(bot.name(), config.speed_hz),
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

fn bot_status(bot_name: &str, speed_hz: f64) -> String {
    format!("bot {bot_name} running at {speed_hz} Hz; q quit; r restart")
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
) -> Result<bool> {
    draw_game(stdout, game, last_move, use_color, message)?;
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

fn draw_next_tile(stdout: &mut Stdout, next: &NextTile, use_color: bool) -> Result<()> {
    queue!(stdout, Print("next "))?;
    match next {
        NextTile::Basic { rank, face } => {
            draw_tile(stdout, *rank, &center_label(&face.to_string()), use_color)?;
        }
        NextTile::Bonus { .. } => {
            let label = "+";
            draw_tile(stdout, 3, &center_label(label), use_color)?;
        }
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
        let status = bot_status("random", 4.0);
        assert!(status.contains("random"));
        assert!(status.contains("4"));
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
}
