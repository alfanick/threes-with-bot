use crate::game::{BonusForecast, GameSnapshot, MoveResult};
use anyhow::{Context, Result};
use serde::Serialize;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

const SCHEMA_VERSION: u8 = 1;

#[derive(Clone, Debug, Serialize)]
pub struct RunConfigLog {
    pub mode: String,
    pub seed_source: String,
    pub speed_hz: f64,
    pub color: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ab: Option<AbConfigLog>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AbConfigLog {
    pub depth: u8,
    pub alpha: String,
    pub beta: String,
    pub dfs: bool,
    pub time_limit_ms: Option<u64>,
    pub node_limit: Option<u64>,
}

pub struct GameLogger {
    json: Option<BufWriter<File>>,
    text: Option<BufWriter<File>>,
}

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum JsonEvent<'a> {
    Start {
        schema_version: u8,
        snapshot: &'a GameSnapshot,
        config: &'a RunConfigLog,
    },
    Turn {
        schema_version: u8,
        result: &'a MoveResult,
    },
    End {
        schema_version: u8,
        reason: &'a str,
        snapshot: &'a GameSnapshot,
    },
}

impl GameLogger {
    pub fn new(json_path: Option<PathBuf>, text_path: Option<PathBuf>) -> Result<Self> {
        let json = json_path
            .map(|path| {
                File::create(&path)
                    .with_context(|| format!("failed to create JSON log {}", path.display()))
                    .map(BufWriter::new)
            })
            .transpose()?;

        let text = text_path
            .map(|path| {
                File::create(&path)
                    .with_context(|| format!("failed to create text log {}", path.display()))
                    .map(BufWriter::new)
            })
            .transpose()?;

        Ok(Self { json, text })
    }

    pub fn log_start(&mut self, snapshot: &GameSnapshot, config: &RunConfigLog) -> Result<()> {
        self.write_json(&JsonEvent::Start {
            schema_version: SCHEMA_VERSION,
            snapshot,
            config,
        })?;

        if let Some(text) = &mut self.text {
            writeln!(
                text,
                "START seed={} score={}",
                snapshot.seed, snapshot.score
            )?;
            writeln!(
                text,
                "mode={} seed_source={} speed_hz={} color={}",
                config.mode, config.seed_source, config.speed_hz, config.color
            )?;
            if let Some(ab) = &config.ab {
                writeln!(
                    text,
                    "ab depth={} alpha={} beta={} time_limit_ms={:?} node_limit={:?}",
                    ab.depth, ab.alpha, ab.beta, ab.time_limit_ms, ab.node_limit
                )?;
                writeln!(text, "ab dfs={}", ab.dfs)?;
            }
            writeln!(text, "next={}", snapshot.next_tile.display_label())?;
            writeln!(
                text,
                "bonus_forecast={}",
                display_bonus_forecast(&snapshot.bonus_forecast)
            )?;
            write_board(&mut *text, &snapshot.board)?;
            writeln!(text)?;
        }
        Ok(())
    }

    pub fn log_turn(&mut self, result: &MoveResult) -> Result<()> {
        self.write_json(&JsonEvent::Turn {
            schema_version: SCHEMA_VERSION,
            result,
        })?;

        if let Some(text) = &mut self.text {
            writeln!(
                text,
                "TURN move={} accepted={} score={} high={}",
                result.direction.as_str(),
                result.accepted,
                result.score,
                result.high_tile
            )?;
            if let Some(spawn) = &result.spawn {
                writeln!(
                    text,
                    "spawn row={} col={} value={} source={:?} preview={}",
                    spawn.row,
                    spawn.col,
                    spawn.face,
                    spawn.source,
                    spawn.preview.display_label()
                )?;
            }
            writeln!(text, "next={}", result.next_tile.display_label())?;
            writeln!(
                text,
                "bonus_forecast={}",
                display_bonus_forecast(&result.bonus_forecast)
            )?;
            write_board(&mut *text, &result.board_after_spawn)?;
            writeln!(text)?;
        }
        Ok(())
    }

    pub fn log_end(&mut self, reason: &str, snapshot: &GameSnapshot) -> Result<()> {
        self.write_json(&JsonEvent::End {
            schema_version: SCHEMA_VERSION,
            reason,
            snapshot,
        })?;

        if let Some(text) = &mut self.text {
            writeln!(
                text,
                "END reason={} seed={} score={} high={} accepted_moves={} attempted_moves={}",
                reason,
                snapshot.seed,
                snapshot.score,
                snapshot.high_tile,
                snapshot.accepted_moves,
                snapshot.attempted_moves
            )?;
            write_board(&mut *text, &snapshot.board)?;
            writeln!(text)?;
        }

        self.flush()
    }

    pub fn flush(&mut self) -> Result<()> {
        if let Some(json) = &mut self.json {
            json.flush()?;
        }
        if let Some(text) = &mut self.text {
            text.flush()?;
        }
        Ok(())
    }

    fn write_json<T: Serialize>(&mut self, event: &T) -> Result<()> {
        if let Some(json) = &mut self.json {
            serde_json::to_writer(&mut *json, event)?;
            writeln!(json)?;
        }
        Ok(())
    }
}

fn write_board(mut writer: impl Write, board: &[u64; 16]) -> Result<()> {
    for row in board.chunks(4) {
        writeln!(
            writer,
            "{:>6} {:>6} {:>6} {:>6}",
            display_cell(row[0]),
            display_cell(row[1]),
            display_cell(row[2]),
            display_cell(row[3])
        )?;
    }
    Ok(())
}

fn display_cell(value: u64) -> String {
    if value == 0 {
        ".".to_string()
    } else {
        value.to_string()
    }
}

fn display_bonus_forecast(forecast: &BonusForecast) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::Game;

    #[test]
    fn serializes_start_event() {
        let game = Game::new(1);
        let config = RunConfigLog {
            mode: "human".to_string(),
            seed_source: "explicit".to_string(),
            speed_hz: 4.0,
            color: "auto".to_string(),
            ab: None,
        };
        let event = JsonEvent::Start {
            schema_version: SCHEMA_VERSION,
            snapshot: &game.snapshot(),
            config: &config,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"start\""));
        assert!(json.contains("\"schema_version\":1"));
        assert!(json.contains("\"seed\":1"));
        assert!(!json.contains("\"rank\""));
    }
}
