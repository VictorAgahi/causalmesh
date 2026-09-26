#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod decapitate;
pub mod graph;
pub mod guard;
pub mod languages;
pub mod markdown;
pub mod topology;

pub use decapitate::{AstDecapitator, DecapitatedSource, LanguageKind};
pub use graph::{GraphRenderer, WebEdge, WebGraphPayload, WebNode};
pub use guard::{AstGuard, BoundedMatch, ParserError};
pub use languages::{CompiledPattern, ExtractConfig, FileIndex, PolyglotIndexer};
pub use markdown::{MarkdownFormatter, SearchPage, SearchResult, MAX_OUTPUT_BYTES};
pub use topology::{Topology, TopologyOptions};
