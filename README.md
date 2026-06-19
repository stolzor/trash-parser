# detox-parser

Сбор видео с **YouTube** и **TikTok** для ML-проекта `detox/ml` («Dopamine Index»).
Реализует Bronze-слой его medallion-пайплайна (Stage 0 Discovery, Stage 1
метаданные, Stage 1b медиа) на Rust. Silver/Features/Gold остаются в Python.

Архитектура подробно: [`docs/architecture.md`](docs/architecture.md).

## Требования

- Rust 1.75+
- **`yt-dlp`** в `PATH` — движок экстракции метаданных и медиа (`brew install yt-dlp`).
  Переопределяется env `YTDLP_BIN`.
- **`ffmpeg`** в `PATH` — нужен **только медиа-стадии** (yt-dlp вызывает ffmpeg для
  оконного скачивания длинных видео `--download-sections` и муксинга).
  **Метаданные и discovery работают без ffmpeg.** Переопределяется env `FFMPEG_BIN`.
- **PostgreSQL** (манифест) + **объектное хранилище** (Bronze, по умолчанию
  `backend="s3"`) — поднимаются одним `docker compose` (см. ниже). Оба на
  named volumes. Для прод-S3/R2 просто укажи реальные `[storage.s3]` + креды.

### Проверки окружения

- **yt-dlp** обязателен: если бинарь не запускается, операция падает с понятной
  ошибкой `external tool error: не удалось запустить '<bin>' … (установлен ли yt-dlp?)`.
- **ffmpeg** — мягкая проверка: при старте, если `[media] download = true`, а ffmpeg
  не найден, пайплайн пишет **WARN** (не падает) и продолжает — метаданные/discovery
  соберутся, а оконное скачивание/муксинг — нет. Варианты: поставить ffmpeg, задать
  `FFMPEG_BIN`, либо `[media] download = false` / `[media] long_windows = 0`.
- **Postgres/S3** — обязательны для соответствующих стадий: при недоступности
  операция вернёт `storage error …` (например, пустой `[storage.s3] bucket`).

## Локальная инфра (Docker Compose)

`docker-compose.yml` поднимает PostgreSQL и MinIO (S3 + web-консоль) на
персистентных named volumes и создаёт бакет `detox-bronze`:

```bash
docker compose up -d            # PG :5433, MinIO API :9000, консоль :9001
export AWS_ACCESS_KEY_ID=minioadmin AWS_SECRET_ACCESS_KEY=minioadmin
# ... запускаешь парсер (см. ниже) ...
docker compose down             # стоп; данные остаются в томах
docker compose down -v          # снести вместе с данными
```

- Web-консоль MinIO: <http://127.0.0.1:9001> (minioadmin / minioadmin).
- Тома: `detox-pg-data`, `detox-minio-data` (переживают пересоздание).
- Чисто локально без S3: `backend="fs"` — тогда MinIO не нужен.

## Сборка

```bash
cargo build --release
```

## Использование

Источники описываются в `config/seeds.toml`. По умолчанию хранение **отделено от
ml**: данные (raw/normalized/видео) идут в **S3/MinIO** (`[storage] backend="s3"`),
статусы — в PostgreSQL. `ml` затем читает данные из бакета (или из FS, если
`backend="fs"`). `out_root` — только нейтральный локальный путь под staging.

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

**Распределённый сбор:** запусти несколько процессов `harvest`/`media`/`run` с
одним `[manifest] url` — они делят очередь через атомарный захват
(`FOR UPDATE SKIP LOCKED`), не пересекаясь. Размер батча — `concurrency.claim_batch`.

## Раскладка данных

Одинаковая раскладка ключей в S3 (по умолчанию) и на ФС (`backend="fs"`):

```
raw/meta/<id>.json               # сырой yt-dlp -J (всё, формат, который читает ml)
raw/videos/<id>.mp4              # медиа (capped); для длинных — <id>.win_<start>.mp4
normalized/<platform>/<id>.json  # кросс-платформенная common-schema
```
- **S3-режим:** ключи под `s3://<bucket>/<prefix>/…`; видео сначала во временный
  `staging_dir`, затем грузится в бакет и локальная копия удаляется. На диск рядом
  с `ml` ничего не пишется.
- **FS-режим:** те же пути под `out_root`.
- **Статусы стадий** (резюмируемость) — в **PostgreSQL** (`[manifest]`), не на диске.
- **discovery-лог** провенанса — локально в `staging_dir/discovery/<run_id>.jsonl`.

Метаданные берутся через `yt-dlp -J`, поэтому `raw/meta/<id>.json` уже в формате,
который `ml` ждёт на Silver-стадии — `ml` просто указываешь на бакет/путь.

## Крейты

| крейт                    | роль                                            |
|--------------------------|-------------------------------------------------|
| `detox-parser-core`      | типы, трейты-границы, конфиг, ошибки (контракт) |
| `detox-parser-manifest`  | резюмируемое состояние в PostgreSQL (async sqlx) |
| `detox-parser-store`     | Bronze-sink на ФС                               |
| `detox-parser-store-s3`  | Bronze-sink в S3/MinIO/R2 (data lake)           |
| `detox-parser-http`      | reqwest-клиент с ротацией прокси (пул + cooldown) |
| `detox-parser-ytdlp`     | async-обёртка над yt-dlp + нормализация          |
| `detox-parser-youtube`   | YouTube discovery                               |
| `detox-parser-tiktok`    | TikTok discovery                                |
| `detox-parser-pipeline`  | оркестратор стадий (concurrency, retry, gate, квоты) |
| `detox-parser-export`    | экспорт нормализованного в parquet (arrow/parquet) |
| `detox-parser-cli`       | бинарь `detox-parser`                           |

## Хранение и масштабирование

- **По умолчанию:** данные (raw/normalized/медиа) в **S3/MinIO** — отдельно от `ml`;
  запросы — **DuckDB** прямо из бакета. Для локальной разработки `backend="fs"` →
  всё под `out_root` (тоже вне `ml`).
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
  сбор несколькими воркерами по одной очереди (атомарный захват задач через
  `FOR UPDATE SKIP LOCKED`, переподхват «застрявших» claimed по TTL).
- **Прокси:** `[proxy] file = "proxies.txt"` — пул из файла, round-robin +
  cooldown на сбойных, общий для yt-dlp и нативных reqwest-клиентов. Нужен для
  стабильного TikTok (residential) и обхода IP-лимитов при масштабе.

## Статус (финальный)

Все запланированные возможности реализованы; где возможно без внешних
кредов/прокси — проверены вживую (Docker Postgres/MinIO + реальная сеть).
**Замкнутый цикл подтверждён e2e:** 10 видео `parser → S3 → адаптер ml →
EngagementLabeler → dopamine_label`.

| Возможность | Статус |
|---|---|
| YouTube discovery — нативный InnerTube (`reqwest`) query/channel + fallback `yt-dlp` (hashtag/trending) | ✅ live |
| YouTube метаданные — контракт `ml` (9 полей) + полный raw `-J` + heatmap | ✅ live |
| Скачивание медиа — capped low-res; для длинных видео оконно (`--download-sections`) | ✅ live |
| TikTok discovery — SSR (`__UNIVERSAL_DATA_FOR_REHYDRATION__`) + msToken/cookie + fallback | ⚙️ механизм готов, в тестах; live нужен residential-прокси + `msToken` |
| Многохоп против survivorship bias (`discovery.max_hops`) | ✅ unit + live |
| Квоты по доменам short/long | ✅ unit |
| Прокси-пул из файла (round-robin + cooldown) — для yt-dlp и reqwest | ✅ end-to-end |
| Distributed claim (`FOR UPDATE SKIP LOCKED`, батчи) | ✅ live, на реальных данных |
| PostgreSQL-манифест (async sqlx) | ✅ live |
| S3/MinIO/R2 sink + хранение отделено от `ml` | ✅ live (MinIO) |
| Экспорт parquet, в т.ч. Hive-партиционированный | ✅ live (DuckDB) |
| Docker Compose (PG + MinIO + бакет) на named volumes | ✅ live |
| Адаптер чтения Bronze из S3 в `ml` (`dopamine/io/bronze.py`) | ✅ live (10 видео → labeler) |
| Префлайт-проверка окружения (yt-dlp hard, ffmpeg soft-WARN) | ✅ |

14 тестов: unit + gated интеграционные (Postgres/MinIO в Docker + реальная сеть);
gated тихо пропускаются без `PG_TEST_URL`/`S3_TEST_BUCKET`. Сборка зелёная, clippy чист.

### Замкнутый поток данных
```
parser (Rust, N воркеров)
   discover → harvest → expand → media        статусы → PostgreSQL (манифест)
        └──────────────┬───────────────┘       данные  → S3/MinIO (bronze/raw+normalized+видео)
                       ▼
        ml: dopamine.io.bronze (S3/ФС) → EngagementLabeler → dopamine_label
```

### Единственное ограничение
**Живой TikTok** требует residential-прокси + валидный `msToken`-cookie — без них
и нативный SSR, и yt-dlp упираются в бот-стены TikTok. Это внешний фактор, не код:
задай куки (`[tiktok] cookie` / `cookie_file` / `cookies_from_browser = "chrome"`
— авто-экспорт куки браузера через yt-dlp, покрывает оба пути) + `[proxy] file`
(residential) — заработает без правок.

### Опционально на будущее
- Подписанный TikTok XHR для пагинации глубже первой страницы.
- `media_path` в манифесте → S3-ключ вместо локального staging-пути (косметика).
- Lakehouse (Apache Iceberg / Delta Lake) поверх S3 при росте и нескольких писателях.
- Observability (Prometheus-метрики сбора).
