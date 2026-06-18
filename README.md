# detox-parser

Сбор видео с **YouTube** и **TikTok** для ML-проекта `detox/ml` («Dopamine Index»).
Реализует Bronze-слой его medallion-пайплайна (Stage 0 Discovery, Stage 1
метаданные, Stage 1b медиа) на Rust. Silver/Features/Gold остаются в Python.

Архитектура подробно: [`docs/architecture.md`](docs/architecture.md).

## Требования

- Rust 1.75+
- **`yt-dlp`** в `PATH` (движок экстракции и discovery) — `brew install yt-dlp`
- **`ffmpeg`** в `PATH` (нужен yt-dlp для муксинга)

## Сборка

```bash
cargo build --release
```

## Использование

Источники описываются в `config/seeds.toml` (по умолчанию `out_root = ../ml/data`,
то есть Bronze падает прямо во вход ml).

```bash
detox-parser discover     # Stage 0: найти кандидатов по seeds → манифест
detox-parser harvest      # Stage 1: собрать метаданные (raw/meta/<id>.json)
detox-parser media        # Stage 1b: скачать медиа (capped low-res)
detox-parser run          # всё подряд: discover → harvest → media
detox-parser status       # сводка по стадиям из манифеста
detox-parser single youtube "https://youtube.com/watch?v=..."   # разовый парс
```

Логи: `RUST_LOG=debug detox-parser ...`. Бинарь yt-dlp: `YTDLP_BIN=/path/to/yt-dlp`.

## Раскладка вывода (`out_root`)

```
raw/meta/<id>.json            # сырой yt-dlp -J — формат, который читает ml
raw/videos/<id>.mp4           # медиа (capped)
normalized/<platform>/<id>.json  # кросс-платформенная common-schema
manifest.sqlite               # статусы стадий (резюмируемость)
discovery/<run_id>.jsonl      # провенанс discovery
```

Раз метаданные берутся через `yt-dlp -J`, `raw/meta/<id>.json` уже в формате,
который `ml` ждёт на Silver-стадии — интеграция без изменений в Python.

## Крейты

| крейт                    | роль                                            |
|--------------------------|-------------------------------------------------|
| `detox-parser-core`      | типы, трейты-границы, конфиг, ошибки (контракт) |
| `detox-parser-manifest`  | резюмируемое состояние в SQLite                 |
| `detox-parser-store`     | Bronze-sink на ФС                               |
| `detox-parser-ytdlp`     | async-обёртка над yt-dlp + нормализация          |
| `detox-parser-youtube`   | YouTube discovery                               |
| `detox-parser-tiktok`    | TikTok discovery                                |
| `detox-parser-pipeline`  | оркестратор стадий (concurrency, retry, gate)   |
| `detox-parser-cli`       | бинарь `detox-parser`                           |

## Статус / дальнейшее

- v0 discovery идёт через `yt-dlp --flat-playlist` (надёжно, работает сразу).
  Нативный InnerTube-клиент на `reqwest` можно добавить как альтернативную
  реализацию трейта `Discoverer`, не трогая pipeline.
- TODO: многохоповое расширение (`discovery.max_hops`), квоты по доменам,
  скачивание окон для длинных видео, экспорт в parquet.
