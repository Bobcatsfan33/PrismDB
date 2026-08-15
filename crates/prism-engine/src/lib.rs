//! The engine: everything between an event arriving and an answer leaving.

pub mod admission;
pub mod aws_kms;
pub mod bench;
pub mod clock;
pub mod cluster;
pub mod cluster_corpus;
pub mod corpus;
pub mod delete;
pub mod drift;
pub mod engine;
pub mod evidence;
pub mod flight;
pub mod generations;
pub mod gpu;
pub mod hedge;
pub mod idempotency;
pub mod ingest;
pub mod ingestor;
pub mod keys;
pub mod merge;
pub mod model;
pub mod model_policy;
pub mod novelty;
pub mod oracle;
pub mod otlp;
pub mod plan;
pub mod realcorpus;
pub mod rowsource;
pub mod sample;
pub mod scheduler;
pub mod search;
pub mod shard_rpc;
pub mod sharded;
pub mod source;
pub mod sql;
pub mod storage;
pub mod tenant_key;
pub mod topk;
pub mod tsv;
pub mod tuning;
pub mod wal;

pub use engine::Engine;
pub use ingest::IngestReport;
pub use ingestor::{IngestReport2, Ingestor};
pub use merge::{MergeReport, ReembedReport};
#[cfg(unix)]
pub use model::UnixInferenceTransport;
pub use model::{
    HashModelPlane, InferenceItem, InferenceRequest, InferenceResponse, InferenceTransport,
    ModelPlane, ModelRegistry, ProductionModelPlane, RegisteredModel,
};
pub use model_policy::{GovernedModelPlane, ModelPolicy, ModelPolicyEnforcer};
