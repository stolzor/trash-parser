//! Ядро detox-parser: доменные типы, трейты-границы, конфиг, ошибки.
//! Ни от чего не зависит — единственный общий контракт всех крейтов.

pub mod config;
pub mod error;
pub mod proxy;
pub mod traits;
pub mod types;

pub use error::{Error, Result};
pub use proxy::ProxyPool;
pub use traits::{Discoverer, Extractor, MediaFetcher, Sink};
pub use types::*;
