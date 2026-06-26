# Архитектура парсера (detox-parser)

Сервис сбора видео с **YouTube** и **TikTok** для ML-проекта `detox/ml`
(«Dopamine Index»). Парсер — это **Stage 0 (Discovery) + Stage 1 (Bronze) +
Stage 1b (Media)** medallion-пайплайна `ml`, переписанные на Rust и расширенные
на TikTok и tag-based многостадийный discovery.

Граница ответственности жёсткая: **парсер доводит данные до Bronze-слоя**
(сырой JSON + медиа + манифест). Silver/Features/Gold остаются в Python
(`ml/`). Дублировать их в Rust не нужно — это размыло бы контракт.

## Решения (зафиксированы)

> Ключевые трудно-обратимые решения ведутся отдельным журналом ADR:
> [`docs/decisions/`](decisions/README.md). Ниже — базовые решения проекта.

- **Язык:** Rust (tokio async).
- **Движок экстракции:** гибрид.
  - **Discovery** (поиск по тегам/хэштегам, обходы каналов) — нативный Rust
    через `reqwest` (YouTube InnerTube, TikTok web API). Это чистый HTTP+JSON.
  - **Метаданные «вся инфа» + скачивание медиа** — `yt-dlp` как subprocess.
    Он постоянно обновляется под изменения YT/TikTok (signatureCipher,
    n-throttling, протухающие playAddr) — переписывать это на Rust = вечная
    борьба с поломками.
- **Медиа по умолчанию:** capped low-res (`worst[ext=mp4]`, кап ~100 МБ) —
  ровно как в `ml/dopamine/config.py::CollectConfig`. Фичи `ml` считаются на
  1 fps + аудио, full-res не нужен. Настраивается.
- **Ingestion:** discovery по тегам/хэштегам/каналам, многостадийный
  (seed → fan-out → опц. расширение по каналам/related против survivorship bias).

## Контракт с `ml` (почему интеграция бесплатна)

`ml` на Silver-стадии (`dopamine/labels/engagement.py::extract_raw_signals`)
читает из каждого `data/raw/meta/<id>.json`:

| поле                     | назначение в `ml`                         |
|--------------------------|-------------------------------------------|
| `id`                     | ключ джойнов                              |
| `view_count`             | `views_per_day`, `log_views`              |
| `like_count`             | `like_rate`                               |
| `comment_count`          | `comment_rate`                            |
| `duration` (сек)         | `domain` (≤90с → short), длина окна       |
| `upload_date` (YYYYMMDD) | `age_days`, `age_bucket`                  |
| `channel_follower_count` | `channel_size_bucket`                     |
| `categories`/`category`  | группировка для перцентильной нормализации|
| формат: `fps`, `width`, `height`, aspect, `vcodec`, `tbr` | дешёвые format-фичи (etl.md, A0) |

Так как метаданные берём через `yt-dlp -J`, **сырой JSON уже в формате, который
`ml` ждёт** в `raw/meta/<id>.json`. Хранение отделено от `ml`: пишем в S3/MinIO
(или нейтральный `out_root` в FS-режиме), `ml` читает оттуда. Нормализованную
common-schema пишем рядом (это «наше»), статусы — в Postgres.

### TikTok → общий контракт (маппинг)

| TikTok                | normalized / `ml`           |
|-----------------------|-----------------------------|
| `stats.playCount`     | `view_count`                |
| `stats.diggCount`     | `like_count`                |
| `stats.commentCount`  | `comment_count`             |
| `stats.shareCount`    | `share_count` (extra)       |
| `createTime` (unix)   | `upload_date` (YYYYMMDD)    |
| `video.duration`      | `duration`                  |
| `author.followerCount`| `channel_follower_count`    |
| `diversificationLabels`| `categories`               |

## Принцип хранения: normalize + сохранять raw

«Напарсить всю возможную информацию»:

1. **Raw verbatim** — полный ответ экстрактора как есть, immutable. Ничего не
   теряем (heatmap «most replayed», chapters, subtitles, все форматы, music и т.д.).
2. **Normalized common-schema** — кросс-платформенный срез (то, что нужно `ml`
   + аналитика), одинаковый для YT и TikTok, с провенансом.

## Раскладка крейтов (workspace)

```
parser/
  Cargo.toml                      # [workspace]
  config/seeds.toml               # источники, квоты, лимиты
  docs/architecture.md
  crates/
    core/      detox-parser-core      # типы, трейты, схема, config, ошибки — КОНТРАКТ
    manifest/  detox-parser-manifest  # postgres (async sqlx): состояние по стадиям
    store/     detox-parser-store     # medallion-sink на ФС (bronze layout)
    ytdlp/     detox-parser-ytdlp     # async-обёртка над yt-dlp subprocess
    youtube/   detox-parser-youtube   # InnerTube discovery + extractor-адаптер
    tiktok/    detox-parser-tiktok    # TikTok discovery + extractor-адаптер
    pipeline/  detox-parser-pipeline  # оркестратор / state machine / concurrency
    cli/       detox-parser-cli       # clap-бинарь
```

Зависимость только «вниз»: `cli → pipeline → {youtube,tiktok,store,manifest} → core`,
`{youtube,tiktok} → ytdlp → core`. `core` ни от кого не зависит.

## Трейты (в `core`)

```rust
#[async_trait]
pub trait Discoverer {                       // Stage 0
    fn platform(&self) -> Platform;
    async fn discover(&self, seed: &Seed) -> Result<Vec<Candidate>>; // + provenance
}

#[async_trait]
pub trait Extractor {                        // Stage 1
    fn platform(&self) -> Platform;
    async fn metadata(&self, id: &VideoId) -> Result<RawExtract>;    // raw + normalized
}

#[async_trait]
pub trait MediaFetcher {                     // Stage 1b
    async fn fetch(&self, id: &VideoId, opts: &MediaOpts) -> Result<MediaArtifact>;
}

#[async_trait]
pub trait Sink {                             // Bronze storage
    async fn put_raw(&self, rec: &RawExtract) -> Result<RawRef>;
    async fn put_normalized(&self, rec: &NormalizedVideo) -> Result<()>;
    async fn put_media(&self, art: &MediaArtifact) -> Result<()>;
}
```

`yt-dlp`-бэкенд реализует `Extractor` и `MediaFetcher`; нативные клиенты —
`Discoverer`. Поэтому позже можно добавить InnerTube-`Extractor` или
YouTube Data API без изменения pipeline.

## Multi-stage discovery (Stage 0)

```
seeds.toml ──► [Hop 0] discover         ──► candidates (+provenance)
   │            (query / hashtag /            dedup vs manifest
   │             channel uploads / trending)        │
   │                                                ▼
   └─[Hop 1..N] expand ◄── из собранных видео: uploads канала / related
        (опц.)            ► цель: НИЗКО-вовлечённые примеры
                            (борьба с survivorship bias, см. ml/etl.md Stage 0)
```

- **Seed** = `{platform, kind: query|hashtag|channel|trending, value, domain_hint, target}`.
- **Балансировка выборки:** квоты по домену (short/long) и по бакетам
  вовлечённости — чтобы датасет не состоял из одних виралов.
- **Дедуп** через манифест: один и тот же `id` не обрабатываем дважды.
- Провенанс каждого кандидата (`source`, `query`, `rank`, `fetched_at`) пишется
  в `discovery/<run_id>.jsonl` и в нормализованную запись (etl.md: провенанс-колонки).

## State machine (на видео), Stage 1/1b

```
Discovered ─► MetaFetched ─► Gated ─┬─► MediaFetched ─► Done
                                    └─► Filtered (skip media)
```

- **Идемпотентность + резюмируемость** (ключевой принцип `ml/etl.md`): каждый
  переход сверяется с манифестом; harvest падает на середине — норма, перезапуск
  безопасен.
- **Развязка метаданных и медиа:** можно перегенерить метаданные без
  перекачки медиа и наоборот.
- **Лёгкий gate перед медиа** (дёшево, из уже собранной метадаты): отсев
  private/removed/age-restricted / `duration < 3s` / нет ключевых полей /
  слишком свежие. Авторитетный фильтр — Silver в `ml`; наш gate лишь экономит
  трафик на скачивании. Отключаемый.

## Манифест (PostgreSQL, async sqlx)

```sql
-- одна строка на видео, статусы по стадиям
CREATE TABLE videos (
  platform TEXT, id TEXT, url TEXT,
  discovered_at BIGINT, source TEXT, query TEXT,
  channel_id TEXT, domain TEXT, duration_s DOUBLE PRECISION,
  status_meta   TEXT,   -- pending|ok|failed|filtered
  status_media  TEXT,   -- pending|ok|failed|skipped
  retries_meta  INT, retries_media INT,
  meta_path     TEXT, media_path TEXT, last_error TEXT, updated_at BIGINT,
  PRIMARY KEY (platform, id)
);
CREATE TABLE seeds (run_id TEXT, platform TEXT, kind TEXT, value TEXT, ts BIGINT);
CREATE TABLE expanded_channels (platform TEXT, channel_id TEXT, hop INT, ts BIGINT,
  PRIMARY KEY (platform, channel_id));
```

Postgres — только под статусы (данные — JSON/медиа в Sink: ФС/S3). Async sqlx
позволяет распределённый сбор несколькими воркерами по одной очереди.

## Concurrency / надёжность

- `tokio` + bounded `Semaphore` на платформу; пул subprocess'ов `yt-dlp` с капом.
- Rate-limit per host (`governor`), экспоненциальный backoff на 429/5xx (`backoff`).
- Грейсфул-резюм: pipeline читает манифест и берёт только `pending`/`failed`.

## Раскладка данных (хранение отделено от ml)

Одинаковая раскладка ключей в S3 (дефолт) и на ФС:

```
raw/meta/<id>.json               # СЫРОЙ yt-dlp -J — формат ml, immutable
raw/videos/<id>.mp4              # медиа (capped); для длинных — <id>.win_<start>.mp4
normalized/<platform>/<id>.json  # наша common-schema (extra)
```

- **S3-режим (дефолт):** ключи под `s3://<bucket>/<prefix>/…`; медиа yt-dlp пишет
  в локальный `staging_dir`, затем грузится в бакет и локальная копия удаляется.
  Рядом с `ml` ничего не пишется — хранение полностью отделено.
- **FS-режим:** те же пути под `out_root` (нейтральный локальный путь, не `ml`).
- `discovery/<run_id>.jsonl` — провенанс, локально в `staging_dir`.

`ml` читает Bronze из бакета/пути (формат `raw/meta` не меняется).

## Нормализованная схема (`NormalizedVideo`)

`schema_version`, `platform`, `id`, `url`, `title`, `description`,
`channel{id,name,url,follower_count}`, `duration_s`, `domain`(short/long),
`view_count`, `like_count`, `comment_count`, `share_count?`, `upload_date`(YYYYMMDD),
`timestamp`, `categories[]`, `tags[]`, `hashtags[]`, `language?`, `thumbnails[]`,
`format{fps,width,height,aspect_ratio,vcodec,acodec,tbr,filesize}`,
`captions[]`, `heatmap?`(YT most-replayed), `chapters?`, `music?`(TikTok),
`provenance{source,query,rank,fetched_at,extractor,extractor_version}`,
`raw_path`.

## CLI (`detox-parser-cli`)

| команда                 | что делает                                            |
|-------------------------|-------------------------------------------------------|
| `discover`              | Stage 0: seeds → манифест + `discovery/*.jsonl`        |
| `harvest`               | Stage 1: метаданные для `pending` (резюмируемо)        |
| `media`                 | Stage 1b: медиа для прошедших gate                     |
| `run`                   | весь пайплайн с квотами (discover→harvest→gate→media)  |
| `status`                | сводка по манифесту (стадии/платформы/домены)          |
| `export`                | нормализованный jsonl/parquet (опц.)                   |
| `single <url>`          | разовый парс одного видео (debug / on-demand)          |

Конфиг — `config/seeds.toml` (источники, квоты, concurrency, rate-limits,
media-opts, out_root).

## Стек (крейты)

`tokio`, `reqwest`(rustls, gzip, brotli), `serde`/`serde_json`, `clap`(derive),
`sqlx`(postgres), `tracing`(+subscriber), `governor`, `backoff`, `anyhow`,
`thiserror`, `async-trait`, `futures`, `indicatif`, `time`. Внешние рантайм-деп:
`yt-dlp` и `ffmpeg` в `PATH`.

## Принципы (зашить в код, из `ml/etl.md`)

- Immutable raw — сырьё не перекачиваем.
- Идемпотентность + резюмируемость через манифест.
- Развязка метаданных и медиа/фичей.
- Провенанс-колонки (источник, запрос, дата фетча).
- Версионирование схемы (`schema_version` в нормализованной записи).
