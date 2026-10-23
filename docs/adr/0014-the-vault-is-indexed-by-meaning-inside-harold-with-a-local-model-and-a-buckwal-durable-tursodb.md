# 0014. The vault is indexed by meaning inside Harold, with a local model and a buckwal-durable tursodb

Date: 2026-10-24
Status: Accepted (knowledge-index)

## Context

The chief finds a vault note, a Markdown file in the organisation's knowledge folder, only by its file name or by reading `MEMORY.md`. A note that says the same thing in other words stays out of sight, and the user cannot search the vault from the phone or the dash at all. The user chose to build the index into Harold rather than adopt an off-the-shelf tool (qmd runs on Bun or Node, keeps its own SQLite outside tursodb and buckwal, and has no web app or dash).

## Decision

Harold hosts a knowledge tool ([ADR 0001](0001-tools-are-modules-hosted-by-harold.md)) that indexes the vault by meaning.

- **The vault stays the source.** Harold reads it and never writes it. The index is a cache that can always be rebuilt, so it records no events.
- **Exact cosine search in tursodb.** Each vault note's passages are stored with their embeddings (`vector32`) and a search orders them by `vector_distance_cos`. Turso 0.8.3 has no index for dense vectors, and at this size (hundreds of passages, well under a second into the tens of thousands) none is needed. A small word bonus lets exact names such as `buckwal` rank. Turso's Tantivy full-text index, behind a feature Harold leaves off, and a re-ranking model are later steps.
- **A local model, run by an embedding worker.** A Python process Harold starts on the first request that needs it and stops after 10 minutes idle, spoken to by JSON lines as the voice worker is. Stage 0 compared three runtimes on the 77 vault notes and 10 written queries: **llama.cpp through `llama-cpp-python` 0.3.36 (MIT) with EmbeddingGemma 300M as the Q8_0 GGUF** found all 10 targets in the top 3 at about 19 ms a passage and 1.3 GB resident; `model2vec` (MIT, `potion-base-8M`) found 8 of 10; `mlx-embeddings` (GPL-3.0) was not run, because the rule picks llama.cpp first and the GPL one only if neither of the others passes. The model is Google's, under the Gemma Terms of Use, from `ggml-org/embeddinggemma-300M-GGUF` at a pinned commit and SHA-256. Harold downloads nothing: the user fetches the file with a documented command and the worker checks the hash.
- **buckwal keeps the database durable.** buckwal is a git dependency pinned to a commit, because it is not on crates.io. It pins `turso_core =0.8.3`, as Harold's `turso =0.8.3` does, and Cargo resolves both to one `turso_core 0.8.3` with the features joined. Its API blocks, so Harold calls it from blocking threads. The bucket holds ciphertext: buckwal encrypts with a key in a file on the Mac. The S3 keys and the encryption key are files at mode 0600 outside the vault and any git checkout; Harold refuses to start the tool otherwise and never passes them through the environment.
- **One prefix per Mac**, `harold-knowledge/<machine name>`, because buckwal lets one writer hold a database at a time and a second takes it over. Two Macs never share an index. The bucket is the RustFS already running on the Mac; a cloud bucket is a change of endpoint and keys.
- **buckwal's telemetry runs through Harold's subscriber.** Harold never calls `buckwal::telemetry::init`, which would install a second global subscriber and fail beside Harold's. Harold's span export filter and log filter accept the target `buckwal`, and its allow-list of exported fields adds buckwal's attributes, so a `buckwal.commit` span reaches ClickHouse under `service.name = harold`.
- **TypeDB and a graph view are later steps.** The vault's links and note kinds give most of the typed relations today.

## Consequences

- The chief, the web app's Notes page and the dash's `s` search find a vault note by meaning through one gRPC service.
- Harold has a second Python worker, and a second way to Turso (recorded in the architecture page's departures).
- A tool gains an off switch: the knowledge tool starts only when `[knowledge] vault` is set.
- The first search after the worker has stopped waits for the model to load (about 8 to 13 s cold, a quarter of a second warm).
- Losing the Mac costs a rebuild, not the notes, since the vault is in git. If the bucket is unreachable, passes fail and retry, and searches still read the local copy.
- `Cargo.lock` gains 22 entries and upgrades six crates (tokio 1.53.1 among them), which also moves `tmx-agent-dash`'s tokio pin; the whole delta is in the [dependency audits](../references/dependency-audits.md).
- llama-cpp-python's dependency diskcache has an unfixed pickle advisory, so the worker never uses `LlamaDiskCache`.
- The word bonus is crude. If results disappoint, the next steps are Turso's full-text index (a new audit) or a re-ranking model.
