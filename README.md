# detox-parser

Сбор видео с **YouTube** и **TikTok** для ML-проекта `detox/ml` («Dopamine Index»).
Реализует Bronze-слой его medallion-пайплайна (Stage 0 Discovery, Stage 1
метаданные, Stage 1b медиа) на Rust. Silver/Features/Gold остаются в Python.

Архитектура подробно: [`docs/architecture.md`](docs/architecture.md).

## Требования

- Rust 1.75+
- **`yt-dlp`** в `PATH` (движок экстракции и discovery) — `brew install yt-dlp`
- **`ffmpeg`** в `PATH` (нужен yt-dlp для муксинга)
- **PostgreSQL** (манифест состояния). Локально проще через Docker:
  ```bash
  docker run -d --name detox-pg -p 5432:5432 \
    -e POSTGRES_USER=postgres -e POSTGRES_PASSWORD=postgres -e POSTGRES_DB=detox \
    postgres:16-alpine
  ```
  DSN задаётся в `[manifest] url` или через env `DATABASE_URL`.

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
detox-parser expand       # тянуть полные аплоады каналов (анти-survivorship-bias)
detox-parser status       # сводка по стадиям из манифеста
detox-parser export       # нормализованное → один parquet (DuckDB/Polars)
detox-parser export --partitioned   # → Hive: platform=…/domain=…/dt=…/part.parquet
detox-parser single youtube "https://youtube.com/watch?v=..."   # разовый парс
```

Логи: `RUST_LOG=debug detox-parser ...`. Бинарь yt-dlp: `YTDLP_BIN=/path/to/yt-dlp`.

## Раскладка вывода (`out_root`)

```
raw/meta/<id>.json            # сырой yt-dlp -J — формат, который читает ml
raw/videos/<id>.mp4           # медиа (capped)
normalized/<platform>/<id>.json  # кросс-платформенная common-schema
discovery/<run_id>.jsonl      # провенанс discovery
```

Статусы стадий (резюмируемость) живут не на диске, а в **PostgreSQL** (`[manifest]`).

Раз метаданные берутся через `yt-dlp -J`, `raw/meta/<id>.json` уже в формате,
который `ml` ждёт на Silver-стадии — интеграция без изменений в Python.

## Крейты

| крейт                    | роль                                            |
|--------------------------|-------------------------------------------------|
| `detox-parser-core`      | типы, трейты-границы, конфиг, ошибки (контракт) |
| `detox-parser-manifest`  | резюмируемое состояние в PostgreSQL (async sqlx) |
| `detox-parser-store`     | Bronze-sink на ФС                               |
| `detox-parser-store-s3`  | Bronze-sink в S3/MinIO/R2 (data lake)           |
| `detox-parser-ytdlp`     | async-обёртка над yt-dlp + нормализация          |
| `detox-parser-youtube`   | YouTube discovery                               |
| `detox-parser-tiktok`    | TikTok discovery                                |
| `detox-parser-pipeline`  | оркестратор стадий (concurrency, retry, gate, квоты) |
| `detox-parser-export`    | экспорт нормализованного в parquet (arrow/parquet) |
| `detox-parser-cli`       | бинарь `detox-parser`                           |

## Хранение и масштабирование

- **Сейчас (MVP):** parquet + JSON + медиа на локальном диске; запросы — **DuckDB**
  (`SELECT * FROM 'export/normalized.parquet'`), как и в `ml` (Polars/DuckDB).
- **Шеринг/масштаб:** `backend = "s3"` в `[storage]` → Bronze (raw/normalized/
  медиа) пишется в **S3 / MinIO / R2** с той же раскладкой ключей, что на ФС;
  DuckDB/Polars читают из S3 напрямую (data lake). Креды — из env. Медиа сначала
  во временный `staging_dir`, затем загружается и локальная копия удаляется.
- **Партиционированный parquet:** `export --partitioned` пишет Hive-layout
  `platform=…/domain=…/dt=…/part.parquet` (партиционные колонки — в пути, не в
  файлах). Залив в бакет (`mc cp`/`aws s3 sync`), читаешь с pruning:
  ```sql
  SELECT * FROM read_parquet('s3://bucket/parquet/**/*.parquet', hive_partitioning=true)
  WHERE platform='youtube' AND domain='short' AND dt >= '2024-06-01';
  ```
- **Версионирование/upsert:** при росте — table format **Apache Iceberg** или
  **Delta Lake** поверх parquet (ACID, schema evolution, time-travel).
- **Состояние/манифест:** PostgreSQL (async sqlx) — поддерживает распределённый
  сбор несколькими воркерами по одной очереди.

## Статус / дальнейшее

Готово: нативный YouTube InnerTube discovery, многохоп (`discovery.max_hops`),
квоты по доменам, экспорт в parquet.

- **YouTube discovery** — нативный InnerTube (`reqwest`) для query/channel,
  с автоматическим fallback на `yt-dlp` (hashtag/trending — сразу yt-dlp).
- **TikTok discovery** — best-effort: GET страницы тега/юзера → встроенный
  `__UNIVERSAL_DATA_FOR_REHYDRATION__` JSON (без подписи запросов), с fallback
  на `yt-dlp`. TikTok жёстко бот-гейтит: нативный путь возвращает items только
  когда страница их SSR-ит; для надёжности в проде нужен residential-IP /
  `msToken`-cookie, либо рабочий yt-dlp app-info. Логика парсинга покрыта тестами.
- TODO: скачивание только нужных окон для длинных видео; партиционированный
  parquet + S3 `Sink`; подписанный TikTok XHR для пагинации.
