use anyhow::{bail, Result};
use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use threes::bot::{AbConfig, BotKind};
use threes::tui::{run_human, run_observed_bot, BotConfig, ColorMode as TuiColorMode, HumanConfig};

#[derive(Parser, Debug)]
#[command(name = "threes", version, about = "A terminal Threes implementation")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[arg(
        long,
        global = true,
        help = "Override the default time-based random seed"
    )]
    seed: Option<u64>,

    #[arg(
        long,
        global = true,
        value_name = "PATH",
        help = "Write machine-readable JSONL game log"
    )]
    log_json: Option<PathBuf>,

    #[arg(
        long,
        global = true,
        value_name = "PATH",
        help = "Write human-readable game log"
    )]
    log_text: Option<PathBuf>,

    #[arg(long, global = true, value_enum, default_value_t = ColorMode::Auto)]
    color: ColorMode,

    #[arg(long, global = true, value_parser = parse_speed, default_value_t = 4.0, help = "Observer speed in moves per second for future bot play")]
    speed: f64,

    #[arg(
        long,
        global = true,
        value_enum,
        value_name = "BOT",
        help = "Run an observed bot game"
    )]
    bot: Option<BotName>,

    #[arg(
        long,
        global = true,
        default_value_t = 3,
        help = "Search depth for --bot ab"
    )]
    ab_depth: u8,

    #[arg(
        long,
        global = true,
        value_name = "MS",
        help = "Time limit per --bot ab move in milliseconds"
    )]
    ab_time_limit_ms: Option<u64>,

    #[arg(
        long,
        global = true,
        value_name = "COUNT",
        help = "Node budget per --bot ab move"
    )]
    ab_node_limit: Option<u64>,

    #[arg(long, global = true, default_value = "-inf", value_parser = parse_finite_or_infinite, help = "Initial alpha bound for --bot ab")]
    ab_alpha: f64,

    #[arg(long, global = true, default_value = "inf", value_parser = parse_finite_or_infinite, help = "Initial beta bound for --bot ab")]
    ab_beta: f64,

    #[arg(
        long,
        global = true,
        help = "Enable bounded DFS tactical planning in --bot ab leaf evaluation"
    )]
    ab_dfs: bool,
}

#[derive(Clone, Copy, Subcommand, Debug)]
enum Command {
    Play,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ColorMode {
    Auto,
    Always,
    Never,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum BotName {
    Random,
    Ab,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Command::Play);

    match command {
        Command::Play => {
            if let Some(bot) = cli.bot {
                let ab_config = ab_config(&cli)?;
                run_observed_bot(BotConfig {
                    seed: cli.seed,
                    log_json: cli.log_json,
                    log_text: cli.log_text,
                    color: cli.color.into(),
                    speed_hz: cli.speed,
                    bot: bot.to_kind(ab_config),
                })
            } else {
                run_human(HumanConfig {
                    seed: cli.seed,
                    log_json: cli.log_json,
                    log_text: cli.log_text,
                    color: cli.color.into(),
                    speed_hz: cli.speed,
                })
            }
        }
    }
}

fn parse_speed(value: &str) -> std::result::Result<f64, String> {
    let speed: f64 = value
        .parse()
        .map_err(|_| "speed must be a number of moves per second".to_string())?;
    if !speed.is_finite() || speed <= 0.0 {
        return Err("speed must be a positive finite Hz value".to_string());
    }
    Ok(speed)
}

fn parse_finite_or_infinite(value: &str) -> std::result::Result<f64, String> {
    let bound: f64 = value
        .parse()
        .map_err(|_| "bound must be a number, inf, or -inf".to_string())?;
    if bound.is_nan() {
        return Err("bound must not be NaN".to_string());
    }
    Ok(bound)
}

fn ab_config(cli: &Cli) -> Result<AbConfig> {
    if cli.ab_depth == 0 {
        bail!("--ab-depth must be at least 1");
    }
    if cli.ab_alpha >= cli.ab_beta {
        bail!("--ab-alpha must be less than --ab-beta");
    }

    if let Some(ms) = cli.ab_time_limit_ms {
        if ms == 0 {
            bail!("--ab-time-limit-ms must be greater than 0");
        }
    }

    if let Some(nodes) = cli.ab_node_limit {
        if nodes == 0 {
            bail!("--ab-node-limit must be greater than 0");
        }
    }

    Ok(AbConfig {
        depth: cli.ab_depth,
        alpha: cli.ab_alpha,
        beta: cli.ab_beta,
        dfs: cli.ab_dfs,
        time_limit_ms: cli.ab_time_limit_ms,
        node_limit: cli.ab_node_limit,
    })
}

impl From<ColorMode> for TuiColorMode {
    fn from(value: ColorMode) -> Self {
        match value {
            ColorMode::Auto => Self::Auto,
            ColorMode::Always => Self::Always,
            ColorMode::Never => Self::Never,
        }
    }
}

impl BotName {
    fn to_kind(self, ab_config: AbConfig) -> BotKind {
        match self {
            Self::Random => BotKind::Random,
            Self::Ab => BotKind::Ab(ab_config),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speed_accepts_values_above_sixty_hz() {
        assert_eq!(parse_speed("120").unwrap(), 120.0);
    }

    #[test]
    fn speed_must_be_positive_and_finite() {
        assert!(parse_speed("0").is_err());
        assert!(parse_speed("-1").is_err());
        assert!(parse_speed("NaN").is_err());
        assert!(parse_speed("inf").is_err());
    }
}
