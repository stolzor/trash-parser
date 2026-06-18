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
- **Объектное хранилище** (по умолчанию `backend="s3"`) — S3 / R2 или локально MinIO:
  ```bash
  docker run -d --name detox-minio -p 9000:9000 \
    -e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin \
    minio/minio server /data
  # создать бакет (mc) и задать креды в env:
  export AWS_ACCESS_KEY_ID=minioadmin AWS_SECRET_ACCESS_KEY=minioadmin
  ```
  Параметры — в `[storage.s3]`. Для чисто локального запуска без S3: `backend="fs"`.

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

## Статус

Все запланированные возможности реализованы; где это возможно без внешних
кредов/прокси — проверены вживую (Docker Postgres/MinIO + реальная сеть).

| Возможность | Статус |
|---|---|
| YouTube discovery — нативный InnerTube (`reqwest`) для query/channel + fallback на `yt-dlp` (hashtag/trending) | ✅ live |
| YouTube метаданные — контракт `ml` (9 полей) + полный raw `-J` + heatmap | ✅ live |
| Скачивание медиа — capped low-res; для длинных видео оконно (`--download-sections`) | ✅ live |
| TikTok discovery — SSR-парсинг (`__UNIVERSAL_DATA_FOR_REHYDRATION__`) + msToken/cookie + fallback | ⚙️ механизм готов, в тестах; live нужен residential-прокси + `msToken` |
| Многохоп против survivorship bias (`discovery.max_hops`) | ✅ unit + live |
| Квоты по доменам short/long | ✅ unit |
| Прокси-пул из файла (round-robin + cooldown) — для yt-dlp и reqwest | ✅ end-to-end |
| Distributed claim (`FOR UPDATE SKIP LOCKED`, батчи) | ✅ live, на реальных данных |
| PostgreSQL-манифест (async sqlx) | ✅ live |
| S3/MinIO/R2 sink | ✅ live (MinIO) |
| Экспорт parquet, в т.ч. Hive-партиционированный | ✅ live (DuckDB) |

13 тестов: unit + gated интеграционные (Postgres/MinIO в Docker + реальная сеть),
последние тихо пропускаются без `PG_TEST_URL`/`S3_TEST_BUCKET`.

### Единственное ограничение
**Живой TikTok** требует residential-прокси + валидный `msToken`-cookie — без них
и нативный SSR, и yt-dlp упираются в бот-стены TikTok. Это внешний фактор, не код:
задай `[tiktok] cookie`/`cookie_file` + `[proxy] file` — нативный путь заработает
без правок.

### Опционально на будущее
- Подписанный TikTok XHR для пагинации глубже первой страницы.
- Lakehouse (Apache Iceberg / Delta Lake) поверх S3 при росте и нескольких писателях.
- Observability (Prometheus-метрики сбора).
