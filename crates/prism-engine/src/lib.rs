//! The engine: everything between an event arriving and an answer leaving.

pub mod bench;
pub mod corpus;
pub mod engine;
pub mod ingest;
pub mod merge;
pub mod model;
pub mod oracle;
pub mod search;
pub mod tsv;

pub use engine::Engine;
pub use ingest::IngestReport;
pub use merge::{MergeReport, ReembedReport};
pub use model::{HashModelPlane, ModelPlane};
