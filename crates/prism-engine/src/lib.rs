//! The engine: everything between an event arriving and an answer leaving.

pub mod admission;
pub mod bench;
pub mod cluster;
pub mod cluster_corpus;
pub mod corpus;
pub mod drift;
pub mod engine;
pub mod evidence;
pub mod flight;
pub mod generations;
pub mod gpu;
pub mod idempotency;
pub mod ingest;
pub mod ingestor;
pub mod merge;
pub mod model;
pub mod novelty;
pub mod oracle;
pub mod otlp;
pub mod plan;
pub mod rowsource;
pub mod sample;
pub mod scheduler;
pub mod search;
pub mod source;
pub mod sql;
pub mod topk;
pub mod tsv;
pub mod tuning;
pub mod wal;

pub use engine::Engine;
pub use ingest::IngestReport;
pub use ingestor::{IngestReport2, Ingestor};
pub use merge::{MergeReport, ReembedReport};
pub use model::{HashModelPlane, ModelPlane};
