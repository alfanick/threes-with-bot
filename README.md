# Threes

`threes` is a terminal implementation of the 2048-style tile game with rules
matching [Threes!](https://threesgame.com): merging `1` and `2` into `3`, then
equal powers of two combine onward.

## Features

- Human and bot gameplay modes in one CLI.
- Bot opponent mode for human games (side-by-side boards).
- Alpha-beta bot (`--bot ab`) with optional depth/time/node limits.
- Time-seeded randomness by default, `--seed` for reproducible runs.
- Machine- and human-readable logs for full replayability.
- TUI rendering with colored, fixed-size tiles.
- Bonus-tile forecasting and score/evaluation statistics for bot decisions.

## Install and Run

```bash
cargo run --release -- play
```

## Examples

### Human mode

```bash
cargo run --release -- --speed 6
```

### Observed bot mode

```bash
cargo run --release -- --bot ab --ab-depth 4
```

### Human mode with bot opponent

```bash
cargo run --release -- --bot-opponent ab --ab-depth 3
```

## Controls

- Move: arrow keys or `wasd`/`hjkl`
- Quit: `q`
- Restart: `r`

## CLI Options

See `--help` for the full option list.

## Development

```bash
cargo test --lib
cargo fmt --all
```

## License

This project is licensed under the MIT license. See [LICENSE](./LICENSE).
