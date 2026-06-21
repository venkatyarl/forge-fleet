use super::spi::{ExtractCtx, Extractor, Fact};
use anyhow::Result;

/// Reference/template extractor that defines the SPI shape new dimension
/// extractors (db, events, config, security, …) copy from.
///
/// Code symbols themselves are still extracted by the in-tree per-language pass in
/// `index_one_generation` (tree-sitter parse + two-pass call resolution), which
/// already stamps `generation`/`confidence`. So this extractor is intentionally a
/// no-op for now — it keeps the registry path exercised end to end without
/// double-extracting. A future PR can migrate the in-tree pass to emit `Fact`s
/// here; until then, new extractors are the real registry members.
pub struct CodeSymbolsExtractor;

#[async_trait::async_trait]
impl Extractor for CodeSymbolsExtractor {
    fn name(&self) -> &'static str {
        "code_symbols"
    }

    async fn extract(&self, _ctx: &ExtractCtx) -> Result<Vec<Fact>> {
        // No-op: code symbols are written by the in-tree pass (see module docs).
        // A real extractor returns its Node/Edge Facts here; write_facts persists
        // them stamped with the in-progress generation.
        Ok(Vec::new())
    }
}
