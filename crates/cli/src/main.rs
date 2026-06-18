//! CLI парсера. Загружает config/seeds.toml, строит pipeline, гоняет стадии.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use detox_parser_core::config::AppConfig;
use detox_parser_core::types::Platform;
use detox_parser_pipeline::Pipeline;

#[derive(Parser)]
#[command(name = "detox-parser", about = "Сбор YouTube/TikTok видео для detox/ml (Bronze stage)")]
struct Cli {
    /// Путь к TOML-конфигу источников.
    #[arg(short, long, default_value = "config/seeds.toml", global = true)]
    config: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Stage 0: найти кандидатов по seeds.
    Discover,
    /// Stage 1: собрать метаданные (Bronze).
    Harvest,
    /// Stage 1b: скачать медиа.
    Media,
    /// Расширение: тянуть полные аплоады каналов собранных видео (анти-bias).
    Expand,
    /// Полный прогон: discover → harvest → expand → media.
    Run,
    /// Сводка по манифесту.
    Status,
    /// Экспорт нормализованных записей в parquet (для аналитики/DuckDB).
    Export {
        /// Путь к parquet (по умолчанию <out_root>/export/normalized.parquet).
        #[arg(long)]
        out: Option<String>,
    },
    /// Разовый парс одного URL.
    Single {
        #[arg(value_enum)]
        platform: CliPlatform,
        url: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum CliPlatform {
    Youtube,
    Tiktok,
}

impl From<CliPlatform> for Platform {
    fn from(p: CliPlatform) -> Self {
        match p {
            CliPlatform::Youtube => Platform::Youtube,
            CliPlatform::Tiktok => Platform::Tiktok,
        }
    }
}

fn load_config(path: &str) -> Result<AppConfig> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("не удалось прочитать конфиг {path}"))?;
    toml::from_str(&text).with_context(|| format!("невалидный TOML в {path}"))
}

/// id прогона на основе времени (без рандома — детерминированно от часов).
fn run_id() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("run-{secs}")
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,sqlx=warn".into()),
        )
        .init();

    let cli = Cli::parse();
    let cfg = load_config(&cli.config)?;
    let out_root = cfg.out_root.clone();
    let pipeline = Pipeline::new(cfg).await?;

    match cli.command {
        Command::Discover => {
            let n = pipeline.discover(&run_id()).await?;
            println!("новых кандидатов: {n}");
        }
        Command::Harvest => pipeline.harvest().await?,
        Command::Media => pipeline.fetch_media().await?,
        Command::Expand => pipeline.expand_loop().await?,
        Command::Run => pipeline.run(&run_id()).await?,
        Command::Status => {
            println!("{:<8} {:<10} count", "stage", "status");
            for (stage, status, count) in pipeline.manifest().summary().await? {
                println!("{stage:<8} {status:<10} {count}");
            }
        }
        Command::Export { out } => {
            let out_path = out.unwrap_or_else(|| {
                format!("{}/export/normalized.parquet", out_root.trim_end_matches('/'))
            });
            let n = detox_parser_export::export_parquet(
                std::path::Path::new(&out_root),
                std::path::Path::new(&out_path),
            )?;
            println!("экспортировано строк: {n} → {out_path}");
        }
        Command::Single { platform, url } => {
            let v = pipeline.single(platform.into(), &url).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
    }
    Ok(())
}
