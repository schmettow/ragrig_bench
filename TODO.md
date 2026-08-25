# TODO

- **Migrate to `RagAgent`** — replace the manual embed → search → prompt →
  generate wiring with `RagAgent::builder()` and
  `generate_with_context_detailed()`.  Each (agent, folder) pair gets its own
  `RagAgent` built from cloned `Box<dyn Generator>` / `Box<dyn Embedder>` /
  `Box<dyn VectorStore>` (all three trait objects are `Clone` since v0.9.3).
  The agent handles prompt construction, context truncation, and error
  recovery internally, and `RagResponse` provides structured output with
  chunk count, sources, and timing.
