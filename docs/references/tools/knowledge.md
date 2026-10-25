# Knowledge Tool

The knowledge tool is Harold's index of the **vault** by meaning. The vault is the organisation's folder of Markdown notes, such as `personal-chief/vault/knowledge`. A **vault note** is one Markdown file in it. The tool reads every vault note, splits a long one into passages, turns each passage into an embedding with a small model on the Mac, and keeps them in a tursodb database that buckwal makes durable in a bucket. A search turns the words into an embedding and returns the vault notes whose passages are closest.

It is a [tool](README.md): a module of the daemon with its own storage and gRPC service, `harold.tools.knowledge.v1.KnowledgeService` (`harold-api/proto/tools/knowledge.proto`). It trusts local callers (ADR 0002). The chief uses it through `harold tool knowledge`, the web app's Notes page ([away-app reference](../away-app/README.md#notes)) and the dash's `s` key ([dash README](../../../tmx-agent-dash/README.md)) call the same service.

The vault stays the source. Harold only reads it, and the index can always be rebuilt from it.

## How the parts fit

<pre class="mermaid">
flowchart LR
  subgraph Callers["Callers"]
    CLI["The chief: harold tool knowledge"]
    WEB["Web app: Notes page"]
    DASH["Dash: the s search"]
  end
  subgraph Tool["Harold: the knowledge tool"]
    PASS["Watcher and passes: start-up, hourly, and 2 s after a save"]
    EMB["Embedding worker: EmbeddingGemma, started on demand, stops after 10 minutes idle"]
    IDX["Knowledge index: tursodb, opened through buckwal"]
    GW["Graph writer: checks, logs, then applies each change"]
    TDB["TypeDB: started by Harold, stops after idle_minutes"]
  end
  subgraph Disk["This Mac's local disk"]
    VAULT[("The vault: .md vault notes")]
    LOCAL[("Index local copy: tools/knowledge/index")]
    LOGS[("Schema log and data log: OpenData, ObjectStoreConfig Local")]
    TFILES[("TypeDB data: tools/knowledge/graph/typedb")]
  end
  subgraph Remote["The bucket"]
    BUCKET[("Encrypted bucket: ciphertext only, one prefix per Mac")]
  end
  VAULT --> PASS
  PASS --> EMB
  PASS --> IDX
  PASS --> GW
  IDX --> LOCAL
  LOCAL --> BUCKET
  GW --> LOGS
  GW --> TDB
  TDB --> TFILES
  CLI --> IDX
  CLI --> GW
  WEB --> IDX
  DASH --> IDX
  classDef existing fill:#eef0f4,stroke:#8e96a3,color:#1d2420
  class CLI,WEB,DASH,PASS,EMB,IDX,GW,TDB,VAULT,LOCAL,LOGS,TFILES,BUCKET existing
  style Callers fill:#1b1d23,stroke:#a4adbb,color:#ffffff
  style Tool fill:#1b1d23,stroke:#a4adbb,color:#ffffff
  style Disk fill:#1b1d23,stroke:#a4adbb,color:#ffffff
  style Remote fill:#1b1d23,stroke:#a4adbb,color:#ffffff
  linkStyle default stroke:#8e96a3,stroke-width:2px
</pre>

Boxes are parts and cylinders are stores. Each dark group is where the parts run or live. An arrow points from a part to what it reads or writes. A search embeds the query with the embedding worker, as a pass embeds passages. The graph exists only when `[knowledge.graph] enabled` is set.

**Which store each call reads:**

| Call | Caller | Reads |
|---|---|---|
| `search`, `related`, `links`, `show`, `recent` | the chief, the Notes page, the dash's `s` (`search`, `recent` and `show`) | the knowledge index, through buckwal's local copy |
| `status`, `rebuild` | the chief | the knowledge index (`rebuild` reads the vault again) |
| `relate`, `unrelate`, `relations`, `graph status` | the chief | the graph writer, then TypeDB |
| `graph rebuild`, `graph void` | the chief | the schema log and the data log, then TypeDB. A rebuild never reads the vault |

**What survives which loss:**

| Lost | What happens |
|---|---|
| The index's local copy | buckwal restores the database from the bucket at the next start |
| The bucket's objects too | the next pass builds the index again from the vault |
| TypeDB's data | Harold rebuilds the graph from the two logs, at start or with `graph rebuild` |
| The two logs | TypeDB still holds the graph until the next `graph rebuild`. That rebuild replays only the records written after the loss, so the older typed relations are lost. The rebuild can also stop at a link or typed relation whose vault notes the new log lacks: see [A record TypeDB refuses](#a-record-typedb-refuses) |
| The Mac | the vault and the bucket survive, if they live elsewhere. The logs and TypeDB's data are on this Mac only, so the typed relations are lost |

## The CLI

```sh
harold tool knowledge search "keeping the index safe if the Mac is lost" [--limit 10]
harold tool knowledge recent [--limit 10]
harold tool knowledge related a-knowledge-index-on-buckwal [--limit 10]
harold tool knowledge links a-knowledge-index-on-buckwal
harold tool knowledge show a-knowledge-index-on-buckwal
harold tool knowledge status
harold tool knowledge rebuild
```

The graph has its own commands, [below](#the-graphs-commands).

| Command | What it prints |
|---|---|
| `search WORDS` | The closest vault notes, best first, at most `--limit` (1 to 10, default 10). One line each: score, name, kind and description |
| `recent` | The vault notes changed most recently, newest first, in the same lines with `-` for the score |
| `related NOTE` | The vault notes closest to the vault note's own passages, the note itself left out. A line ends its kind with `(linked)` when the vault note already links to it |
| `links NOTE` | The suggested links: related vault notes scoring 0.60 or more that the vault note does not link to yet, at most 5 |
| `show NOTE` | The vault note's name, title, kind, description and links, then its text as stored, without the front matter. At the service the `VaultNote` message also carries `file`, the file's absolute path, which the dash opens in `$EDITOR` |
| `status` | The vault notes and passages held, the last good pass, the changed files not yet indexed, the runtime and model, the worker (`idle`, `loading`, `ready` or `unavailable`) and the last failure |
| `rebuild` | Clears the index and builds it again from the vault, then prints the counts |

A vault note is named by its path under the vault without `.md`: `a-knowledge-index-on-buckwal`, `references/hyperdx`. The first search after the worker has stopped waits for the model to load. The CLI waits up to 180 seconds for a knowledge call.

Refusals print one line: `unknown vault note NAME`, `search is resting: the model didn't load`, `the knowledge index is unavailable`, `the Harold daemon does not serve the knowledge tool` (`[knowledge]` is not set). At the service they are `NotFound` `unknown_vault_note`, `Unavailable` `search_resting: <reason>` or `store_failed: <reason>`, `InvalidArgument` `invalid <field>` and `Unimplemented`.

## What is indexed

- **Which files:** every `.md` file under `[knowledge] vault`, in every folder, except the names in `[knowledge] exclude` (paths relative to the vault, with `.md`) and any file or folder whose name starts with `.`. The setting ships empty. The vault's layout belongs to the organisation, so Harold hard-codes none of it.
- **A vault note's name** is its path without `.md`. Its front matter, when present, gives the title (`name`), `description`, `kind` and `created`. Without front matter the title is the file name and the kind is `unknown`; the note is still indexed. Its `[[links]]` are kept, as written, once each (`[[a|label]]` and `[[a#heading]]` link to `a`), to tell a related note that is already linked from one that is not.
- **Passages:** a vault note of at most 400 words is one passage. A longer one is split at its headings, then at paragraphs, into passages of at most 400 words; one paragraph longer than that is cut at a word. A heading line inside a code fence is not a heading.
- **The prompts:** EmbeddingGemma sees a passage as `title: <title> | text: <passage>` and a search as `task: search result | query: <your words>`, the prompts it is trained with. They are recorded as `embeddinggemma` in `index_info`.

## Keeping the index current

- At start-up and every hour, a pass compares each file's SHA-256 with the index. It embeds new and changed vault notes and removes deleted ones. A rename removes the old name and indexes the new one, with no history kept. A pass with nothing to do does not start the embedding worker.
- Between those passes a file watcher on the vault reacts to a saved vault note: after the vault has been quiet for 2 seconds, a pass runs. If the watcher cannot start, the hourly pass still runs.
- A change of the model, its dimensions, its runtime or its prompts rebuilds the whole index: a pass compares the configured model's file name with the one recorded without starting the worker, and the worker's own report of the model and prompts when it starts. A `rebuild` that cannot load the model leaves the index as it was.
- A vault note the model cannot embed is skipped and the pass goes on with the others, then reports `worker_failed`; the next pass tries it again.
- `rebuild` clears the index and builds it again.
- A search during a pass reads the last committed index: a vault note's rows are replaced in one commit.
- The vault note's time in the index (`changed_at`) is the file's modification time, so `recent` follows the vault's edits, not the pass.

## Searching

- A search embeds the query and asks tursodb for the 40 closest passages (`ORDER BY vector_distance_cos(embedding, vector32(?)) LIMIT 40`). tursodb has no index for dense vectors, so this compares the query with every passage; that stays well under a second into the tens of thousands of passages.
- It keeps the best passage per vault note, then adds the word bonus: +0.05 for each query word of 4 characters or more found in the note's title or description, and +0.02 for each found in the passage. It returns the top 10 by score with each note's title, kind, description, the passage cut to about 200 characters, and the score. The score is `1 - cosine distance` plus the bonus.
- A query under 2 characters finds nothing.
- **Related** and **suggested links** use the closest passages (400 are looked at) to any passage of the vault note.

## The embedding worker

The embedding worker is a Python program, `harold/src/embed_worker.py`, that Harold writes into `tools/knowledge/worker/` (mode 0600) and starts as `<command> <args> -I embed_worker.py --model <model> [--sha256 <hash>]`. It speaks one JSON line per request on stdin and stdout:

```text
ready:    {"ready": true, "load_ms": 240, "dimensions": 768, "runtime": "llama.cpp"}
          {"ready": false, "cause": "model_missing" | "model_changed" | "import_failed" | "load_failed"}
request:  {"id": 1, "texts": ["...", "..."]}
answer:   {"id": 1, "embeddings": [[...], [...]]}   or   {"id": 1, "error": "failed"}
```

It starts on the first request that needs it, takes at most 16 texts a request, and stops after 10 minutes with no request. It runs the model with `llama-cpp-python` (llama.cpp, with Metal), keeps no cache on disk and never uses `LlamaDiskCache` (diskcache has an unfixed pickle advisory). It is not given a network: `HF_HUB_OFFLINE=1`, and its stderr is not read. No text, path or error message leaves it.

### The runtime and the model

| | |
|---|---|
| Runtime | [`llama-cpp-python`](https://pypi.org/project/llama-cpp-python/) 0.3.36, MIT (llama.cpp, MIT), built from source with Metal. Its other packages: numpy 2.5.4, typing-extensions 4.16.0, diskcache 5.6.3, jinja2 3.1.6, markupsafe 3.0.4. Audited, see [dependency audits](../dependency-audits.md) |
| Model | EmbeddingGemma 300M, as the GGUF file `embeddinggemma-300M-Q8_0.gguf` (333 590 944 bytes), 768 dimensions |
| Source | Hugging Face repository `ggml-org/embeddinggemma-300M-GGUF` (public, not gated, no login token), commit `0f741b5a6585bd53aeb15cd1372c56f2a0f65e12` |
| SHA-256 | `b5ce9d77a3fc4b3b39ccb5643c36777911cc4eb46a66962eadfa3f5f60490d63` |
| Licence | The Gemma Terms of Use, since it is a derivative of `google/embeddinggemma-300M` |

Harold downloads nothing for this tool. Fetch the model once, into a folder you choose, and check it:

```sh
mkdir -p ~/.local/share/harold-knowledge && cd ~/.local/share/harold-knowledge
curl -L -o embeddinggemma-300M-Q8_0.gguf \
  https://huggingface.co/ggml-org/embeddinggemma-300M-GGUF/resolve/0f741b5a6585bd53aeb15cd1372c56f2a0f65e12/embeddinggemma-300M-Q8_0.gguf
shasum -a 256 embeddinggemma-300M-Q8_0.gguf     # must print the SHA-256 above
```

Set `model_sha256` and Harold checks the file before every worker start; a file that differs is refused with `model_changed`.

Install the runtime in a Python of its own, from the audited pins:

```sh
python3 -m venv ~/.local/share/harold-knowledge/venv
cd ~/.local/share/harold-knowledge
venv/bin/python -m pip install scikit-build-core==1.1.1 packaging==26.3 pathspec==1.1.1 cmake==4.4.4 ninja==1.13.2 \
  numpy==2.5.4 typing-extensions==4.16.0 diskcache==5.6.3 jinja2==3.1.6 markupsafe==3.0.4
SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX26.5.sdk CMAKE_ARGS="-DGGML_METAL=on -DLLAVA_BUILD=OFF" \
  PATH="$PWD/venv/bin:$PATH" venv/bin/python -m pip install llama-cpp-python==0.3.36 --no-binary llama-cpp-python --no-build-isolation
```

**The build needs an older SDK.** On a Mac whose newest Command Line Tools SDK does not link (`ld: tapi error: malformed file ... libSystem.B.tbd`), set `SDKROOT` to an older one in `/Library/Developer/CommandLineTools/SDKs/`, as above. `MacOSX26.5.sdk` worked on 2026-10-24; the newest, `MacOSX27.0.sdk`, did not.

**Stage 0 results** (2026-10-24, this Mac): of the three routes the spec named, llama.cpp found all 10 of 10 test queries' vault notes in its top 3, model2vec (`potion-base-8M`, MIT) 8 of 10, and the third, `mlx-embeddings` 0.1.0 (GPL-3.0), was audited but not installed since llama.cpp passed first. llama.cpp took about 19 ms a passage at normal priority (170 to 234 ms at background priority on a busy Mac), 0.24 s to load warm and 1.3 GB resident.

## Settings

`[knowledge]` is off unless `vault` is set. Put it in `local.toml`:

```toml
[knowledge]
vault = "/Users/you/notes/vault/knowledge"
exclude = ["MEMORY.md", "log.md"]            # ships as []; this organisation's index and change log
command = "/Users/you/.local/share/harold-knowledge/venv/bin/python"
args = []                                    # put before "-I embed_worker.py"
model = "/Users/you/.local/share/harold-knowledge/embeddinggemma-300M-Q8_0.gguf"
model_sha256 = "b5ce9d77a3fc4b3b39ccb5643c36777911cc4eb46a66962eadfa3f5f60490d63"   # optional

[knowledge.bucket]
endpoint = "http://127.0.0.1:9000"
bucket = "harold-knowledge"
# prefix = "harold-knowledge/<machine name>"  # the default; Harold turns characters buckwal does not allow into "-"
credentials_file = "/Users/you/.config/harold/knowledge-bucket"        # access_key=… and secret_key=… lines
encryption_key_file = "/Users/you/.config/harold/knowledge-key"        # 64 hex digits
```

- **Keys are never values in a settings file.** The two files hold the S3 keys and the database's encryption key. They must be mode 0600 (no access for group or others), and lie outside the vault and outside every git checkout. When either is readable by others, missing, inside the vault or inside a git checkout, the knowledge tool does not start and Harold logs `knowledge index failed` with `reason=store_failed`, `refused=<why>` and the file's path. Harold passes their contents to buckwal's `Config` and never through `BUCKWAL_*` environment variables.
- **One prefix per Mac.** buckwal lets one writer hold a database at a time: a second writer on the same prefix takes it over and stops the first. So two Macs never share a prefix.
- **Validation:** with `vault` set, `vault`, `model`, `credentials_file` and `encryption_key_file` are absolute paths, `command` and `[knowledge.bucket]` are set, `endpoint` starts with `http://` or `https://`, and `prefix` holds only `A-Z a-z 0-9 . _ - /` in non-empty parts. Harold refuses to start and names the key.
- The bucket holds only ciphertext (buckwal encrypts the database).

## Storage

`<store folder>/tools/knowledge/index/` is the local copy of the database, opened through buckwal; `worker/` holds the worker script. Tables:

| Table | Columns |
|---|---|
| `vault_notes` | `name` (primary key), `path`, `title`, `kind`, `description`, `created`, `links` (JSON), `hash`, `changed_at` (milliseconds), `body` (the text, for `show` and the web app) |
| `passages` | `note`, `seq`, `text`, `embedding` (a `vector32`), primary key (`note`, `seq`) |
| `index_info` | `runtime`, `model`, `dimensions`, `prompts`, `built_at` |

Every commit is in the bucket before it returns. Delete the local `index/` folder and restart, and buckwal restores the database from the bucket. Delete the prefix's objects as well, and the index is built again from the vault. If the bucket cannot be reached when Harold starts, the tool starts anyway, answers `Unavailable` (`store_failed: bucket_unreachable`) and tries to open the database again every 30 seconds. Once it is open and the bucket cannot be reached, a pass fails with `bucket_unreachable` and tries again at the next one; searches still read the local copy.

## Logs and spans

| Signal | Fields | When |
|---|---|---|
| `knowledge indexed` (info) | `notes`, `count` (passages embedded), `removed`, `embed_ms`, `pending`, `last_ok_age_s` | Each pass and each watched change |
| `knowledge index failed` (warn) | `reason`: `vault_unreadable`, `worker_failed`, `store_failed` or `bucket_unreachable` | A pass or the start failed |
| `knowledge index stale` (warn) | `last_ok_age_s` | The hourly timer finds no good pass for over 2 hours |
| `knowledge worker exited` (warn) | `reason`: `failed_to_load`, `exited` or `timed_out` | The worker could not load, died or did not answer |
| `knowledge worker stopped` (info) | `load_ms`, `embed_ms_per_passage`, `passages`, `rss_mb` | The worker stops (idle or shutdown) |
| `harold.knowledge.search` (span) | `source` (`chief`, `app` or `dash`), `results`, `embed_ms`, `search_ms` | Each search |

No line or span holds a query, a vault note's text or a passage. buckwal's spans and log records (target `buckwal`) go through Harold's own subscriber and are exported under `service.name = harold`; Harold never calls `buckwal::telemetry::init`. The [telemetry reference](../telemetry/README.md#the-knowledge-index) has the cards.

## The knowledge graph

The **knowledge graph** holds the vault notes, their links and the **typed relations** the chief records, such as `ideas-stay-in-the-vault` **inspired_by** `a-knowledge-index-on-buckwal`. TypeDB holds it. It is a separate store from the knowledge index: `search`, `related` and `links` do not change. It is off unless `[knowledge.graph] enabled` is set.

The rule is that **the graph must be recoverable**. The graph writer, one task in Harold, takes changes one at a time from the chief's calls and from the pass over the vault. For each change it:

1. **Checks it.** The relation type exists, both vault notes are in the graph and not missing, and every field is well formed. A change that fails is refused and never reaches the log.
2. **Logs it.** It appends one record to the data log and flushes it. If the log cannot be written, the change is refused `log_failed` and TypeDB is not touched.
3. **Applies it.** In one TypeDB write transaction, it applies the change and moves the **graph position**.
4. **Answers.** The caller hears back once the transaction commits.

A change the graph already holds is not logged, so a pass over an unchanged vault logs nothing. Each change is safe to apply twice. Catch-up and replay apply every record whose sequence is greater than the position, never "the position plus one", since log sequences can jump.

**The relation types:** `relates_to`, `supersedes`, `part_of`, `inspired_by` and `contradicts`. A new type is a schema step, which comes with a Harold release.

### The graph's commands

```sh
harold tool knowledge relate ideas-stay-in-the-vault inspired_by a-knowledge-index-on-buckwal
harold tool knowledge unrelate ideas-stay-in-the-vault inspired_by a-knowledge-index-on-buckwal
harold tool knowledge relations a-knowledge-index-on-buckwal
harold tool knowledge graph status
harold tool knowledge graph rebuild
harold tool knowledge graph void 1291 --reason typo
```

| Command | What it does |
|---|---|
| `relate FROM RELATION TO` | Records a typed relation. Both vault notes must be in the graph and not missing. Prints `already related` when the graph held it |
| `unrelate FROM RELATION TO` | Removes one. It works when a vault note has gone, and a relation that is not there is not an error |
| `relations NOTE` | The vault note's links and typed relations, both ways: `->` or `<-`, the relation, the other vault note, `(missing)` when its file has gone, and its title |
| `graph status` | The graph position, the end of each log, the records waiting and the last refusal |
| `graph rebuild` | Drops the graph database, makes it again, replays the schema log, then the data log, and prints what it replayed |
| `graph void SEQUENCE --reason WORD` | Appends a voided record for the one record the catch-up stopped at |

### When a vault note goes

The pass logs a `remove_link` for each link of the vault note, then:

- **No typed relation names it:** `remove_vault_note`.
- **A typed relation names it:** `mark_vault_note_missing`. The vault note stays, marked missing, and its relations stay. `relations` shows it as `(missing)`, so the chief can record the relation again on the new path and remove the old one. A rename shows as one vault note going and another coming.
- **The file comes back:** `put_vault_note` clears the mark, and the links return.

Harold also refuses `remove_vault_note` for a vault note a typed relation names.

### A record TypeDB refuses

The check in step 1 should stop this, so it means a bug. The catch-up stops at the record and logs `knowledge graph behind` (warn) with its `sequence`. Graph calls answer `Unavailable` `graph_behind`. Read the record in the data log, then run `graph void SEQUENCE --reason WORD`. That appends a `void_record`. The catch-up passes the refused record and goes on, and every later replay honours the void. Only the refused record can be voided: any other sequence is `FailedPrecondition` `not_refused`.

### The logs

Two OpenData logs in `<store>/tools/knowledge/graph/`, mode 0700, with no retention. They are the graph's source.

| Log | Folder | Records |
|---|---|---|
| Schema log | `schema-log/` | JSON: `step`, `at_ms`, `define` (the step's TypeQL) |
| Data log | `data-log/` | JSON: `op` (`put_vault_note`, `mark_vault_note_missing`, `remove_vault_note`, `put_link`, `remove_link`, `put_relation`, `remove_relation`, `void_record`), `at_ms`, `by` (`index` or `call`), and the op's fields. No vault note text |

A vault note is named in the graph by its name (its path without `.md`), kept in the `path` attribute. The logs live on this Mac's disk, as the event stream does. They survive the loss of TypeDB's database, not the loss of the Mac.

**Schema steps** are numbered, in Harold's code. Harold appends a step to the schema log once, flushes it, then applies it. Steps only add: a step may define a type or an attribute and never removes or renames one, since a rebuild replays the whole schema log before any data. A test over the step list refuses a step that does.

### Rebuilding

`graph rebuild` prints, for example:

```text
rebuilt the knowledge graph: 1 schema step, 1284 data records, 0 voided, 77 vault notes, 213 links, 9 typed relations, 4.1 s
```

Searches by meaning still work during a rebuild. Graph calls answer `Unavailable` `graph_rebuilding`. The same rebuild runs on its own when Harold finds the database missing at start, logged `knowledge graph rebuilt` with `reason=missing`. A rebuild never reads the vault. The index's own `rebuild` and `status` stay index only.

### Running TypeDB

TypeDB Community Edition 3.x runs on this Mac and listens on `127.0.0.1` only. Harold refuses to start it with any other address. You install the binary: Harold downloads nothing. Harold starts it on the first graph call, with its data folder in `<store>/tools/knowledge/graph/typedb/`, and stops it after `idle_minutes` without a graph call.

```toml
[knowledge.graph]
enabled = true
server = "/Users/you/typedb-3.14.1/bin/typedb_server_bin"   # the absolute path of the TypeDB server binary
idle_minutes = 10                                # the default
```

`enabled` needs `[knowledge] vault` and `server`. Harold refuses to start and names the key when either is missing.

**Install it.** The audited version is TypeDB server 3.14.1 from Homebrew (see [dependency audits](../dependency-audits.md), row 11). Harold starts only that version: before each start it runs `typedb_server_bin --version` and refuses any other output than `server 3.14.1` (a warn line `knowledge graph server is not the audited version`, and the call answers `typedb_unreachable`). Set `server` to the path of a 3.14.1 binary. Homebrew may give a different version, and `brew upgrade` changes the system install, so do neither blindly: fetch the 3.14.1 bottle with `brew fetch`, check its sha256 against row 11, and extract it into a folder of its own. Do not use the Homebrew service: Harold starts the server itself.

**What Harold does with the server.**
- It writes its own `config.yml` in `<store>/tools/knowledge/graph/typedb/` each time it starts the server. Both listeners (gRPC and HTTP) are on `127.0.0.1` on free ports. TypeDB's reporting and monitoring are off. The config that Homebrew ships listens on `0.0.0.0` and reports, so Harold never uses it.
- It talks to the HTTP API with the `reqwest` that Harold already has. The graph adds no crate.
- A new data folder starts with TypeDB's default `admin` password. Harold replaces it at once with a random one, kept in `typedb/password` (mode 0600).
- The database is `harold_knowledge`. A relation for a link in a vault note's text is `note_link`, because `links` is a reserved TypeQL word. `harold tool knowledge relations` still shows it as `links`.
- A start takes about 2 s. One change takes about 35 ms. A rebuild of 2,000 vault notes and 6,000 relation records takes about 12 s.

**Live tests.** The live tests start a server of their own in a temporary folder. They read the binary from `HAROLD_TYPEDB_BIN` and skip with a note when it is not set or is missing.

### Refusals

| Case | Service answer | CLI line |
|---|---|---|
| Unknown relation type, or the same vault note on both sides | `InvalidArgument` `invalid relation` | `invalid relation` |
| A vault note name that is not well formed | `InvalidArgument` `invalid vault_note` | `invalid vault_note` |
| A vault note not in the graph, or missing (`relate`) | `NotFound` `unknown_vault_note` | `unknown vault note` |
| `[knowledge.graph] enabled` is off | `Unavailable` `graph_off` | `the knowledge graph is off` |
| The catch-up stopped at a refused record | `Unavailable` `graph_behind` | `the knowledge graph is behind its logs` |
| A rebuild runs | `Unavailable` `graph_rebuilding` | `the knowledge graph is rebuilding` |
| The data log could not be written | `Unavailable` `log_failed` | `could not write its log, so nothing changed` |
| TypeDB did not answer | `Unavailable` `typedb_unreachable` | `TypeDB did not answer` |
| `graph void` for another sequence | `FailedPrecondition` `not_refused` | `that record was not refused` |

### Graph logs and spans

| Signal | Fields | When |
|---|---|---|
| `harold.knowledge.graph_waiting` (gauge) | none | The data and schema records with a sequence greater than the graph position. Published after each change, after a refused or unanswered change, and after a catch-up, while the graph is on |
| `harold.knowledge.typed_relations` (gauge) | none | The typed relations in the graph |
| `knowledge graph behind` (warn) | `sequence`, `reason`, `waiting` | TypeDB refuses a record |
| `knowledge graph record voided` (warn) | `sequence`, `reason` | A record is voided |
| `knowledge graph rebuilt` (info) | `reason` (`asked` or `missing`), `schema_steps`, `records`, `voided`, `ms` | A rebuild ends |
| `harold.knowledge.graph_rebuild` (span) | `reason`, `schema_steps`, `records`, `voided`, `ms`, `result` | Each rebuild |
| `knowledge graph sync failed` (warn) | `reason` | The pass could not write to the graph. The index pass does not fail for it |

No field holds a vault note's path or text. The [telemetry reference](../telemetry/README.md#the-knowledge-graph) has the questions and the cards.

## Edge cases

| Case | What happens |
|---|---|
| The vault folder is missing or unreadable | `knowledge index failed` with `vault_unreadable`; searches answer from the last index and `status` says so |
| The worker cannot load | Searches answer `Unavailable`; the web app says "Search is resting: the model didn't load." |
| A link to a vault note that does not exist | Shown as plain text, not a link |
| `[knowledge]` is not set | No knowledge tool, no Notes link in the web app, and the dash's `s` does nothing (it asks Harold once when the stream opens, and `Unimplemented` means off) |
