# LSP — `coddl-lsp` language server + VSCode extension

A VSCode extension shipping a TextMate grammar for instant lexical highlighting, paired with `coddl-lsp` — a Rust language server built on the same frontend crates as the compiler. v1 capabilities currently committed: **syntax highlighting, diagnostics (warnings/errors), inferred-type inlay hints, formatting, and semantic tokens** for mutable-`var` highlighting (see below). Hover, go-to-definition, find-references, completion, and fuller semantic-token coverage (every identifier classified) are designed-for but not v1 work.

## Semantic tokens (mutable-`var` highlighting)

The server advertises `textDocument/semanticTokens` with a minimal legend — token types `[variable]`, token modifiers `[mutable]` — and answers `semantic_tokens_full` by emitting one `variable`+`mutable` token per occurrence (declaration, read, and write) of a mutable `var` binding. This mirrors rust-analyzer's `mutable` modifier so editors underline mutable variables.

The occurrence spans are **precomputed by the typechecker**, not walked in the LSP: `coddl_types::check` resolves every `NameRef` to its binding already, so it records each `var` occurrence's span into `CheckOutput::mutable_spans` as a side product. The analyzer carries that `Vec<Span>` into the `Snapshot` (like `hints`), and `semantic_tokens_full` just sorts, dedups, and delta-encodes it against the `LineIndex`. Because the spans are plain byte offsets (Send/Sync), this needs **no** stored `GreenNode` — sidestepping the `!Sync` rowan-cursor constraint that a general tree-walking semantic-token pass would hit. A fuller highlighter (typing every identifier) is the natural next step and would keep this same span-list plumbing.

## Crate shape

- `coddl-diagnostics` — shared diagnostic data type: `(file_id, byte_range)` span + severity + code + message + optional related-spans. Every frontend crate (`coddl-syntax`, `coddl-types`, `coddl-relir`, `coddl-sqlemit`) produces and consumes this type. The CLI driver renders to terminal (see [driver.md](driver.md)); `coddl-lsp` serializes to `PublishDiagnostics`.
- `coddl-lsp` — language server binary on `tower-lsp` over stdio. Owns document state and request dispatch; no analysis logic of its own — it calls into the frontend crates and forwards their output. Adding hover / go-to-def later is straightforward once `coddl-types` exposes symbol tables.
- `editors/vscode/` — VSCode extension (TypeScript). Ships the TextMate grammar (`syntaxes/coddl.tmLanguage.json`), language configuration (brackets, comments, indent rules), and a client that spawns `coddl-lsp` from `PATH` or a configured location.

Tree-sitter (more accurate, incremental highlighting) is a possible upgrade later; maintaining a second parser in lockstep with `coddl-syntax` is real cost and defers until concrete demand surfaces.

## Discipline this imposes on the frontend (lands in [milestone](milestone.md) step 1, not "when the LSP arrives")

The LSP isn't an add-on bolted on at the end — its requirements shape the rest of the frontend. These constraints land on the compiler from day one, in line with [long-term planning](principles.md):

1. **Spans on every AST/IR node.** Every token, every AST node, every typed-AST node, every diagnostic carries `(file_id, byte_range)`. Retrofitting spans is a project-wide refactor — write them in from the first lexer token.
2. **Error recovery in the parser.** The recursive-descent parser produces a best-effort CST with `PARSE_ERROR` nodes wrapping unrecoverable token ranges rather than failing on the first syntax error. The type checker treats `Error` types as propagating-but-not-cascading — don't pile a hundred type errors on top of one parse error.
3. **Diagnostics-as-values.** No `panic!` or `eprintln!` for user-visible errors anywhere in the frontend. Every pass returns its diagnostics in a `Vec<Diagnostic>` alongside the (possibly partial) result. CLI and LSP differ only in presentation.
4. **Pure analyses.** Every frontend pass is `fn(Input) -> (Output, Vec<Diagnostic>)` — no globals, no I/O, no hidden state. The LSP can call any pass on any buffer at any time.

## Performance posture

v1: full re-parse + re-typecheck per buffer edit. Coddl programs are small and the Rust frontend is fast; latency won't be the bottleneck on realistic files. **Long-term:** route analyses through `salsa` (rust-analyzer's incremental-computation library) once response latency matters. The pure-analysis discipline above makes that migration mechanical rather than architectural — every pass is already shaped like a salsa query.

## Out of scope for v1

Code lenses, refactorings, debug adapter protocol. Sockets for these live in `coddl-lsp` once core diagnostics + hover + go-to-def land. Formatting (`coddl fmt` and `textDocument/formatting`) is in scope — covered separately in [fmt.md](fmt.md) because it has its own design implications for the parser.

---

# Implementation spec

The rest of this doc pins the LSP layer's per-document and per-project models: how documents are tracked, how snapshots are computed and cached, how the analyzer discovers multi-file projects, and how diagnostics flow from the frontend crates back to editor buffers.

**Last sync:** Phase 17. Every commit that changes the analyzer's project model, snapshot caching, or diagnostic routing updates this file in the same commit.


## Per-document model (Phase 13)

The `Analyzer` owns a `RwLock<HashMap<Url, Arc<Document>>>` keyed
on document URI. Each `Document` carries:

- `kind: FileKind` — resolved from the URI's extension on first
  open; fixed for the document's lifetime.
- `inner: Mutex<DocumentInner>` — current `version`,
  `source: Arc<str>`, optionally a cached `Snapshot`, and a
  back-reference to the project this document participates in
  (Phase 17).

A `Snapshot` is the lazy result of one analysis pass:
`source`, `diagnostics`, `hints`, `line_index`, `version`. For
`.cd` / `.cddb` documents `coddl_types::check` runs (parse +
typecheck); for `.cdmap` / `.cdstore` only the parser runs.
CPU work runs on `tokio::task::spawn_blocking` so the LSP IO
loop is never blocked.

Cache invariant: each `did_change` bumps the version and clears
the cached snapshot. The next `snapshot()` call recomputes against
the latest source and writes the cache back only if the document
version hasn't moved on under it (race-safe).


## Project model (Phase 17)

The analyzer also owns
`RwLock<HashMap<PathBuf, Arc<Project>>>`, where the key is the
canonical filesystem path of a `.cd` entry point. A `Project`
groups the `.cd` and its same-directory `.cddb` / `.cdstore`
companions:

```rust
pub struct Project {
    pub cd_path: PathBuf,
    inner: Mutex<ProjectInner>,
}

struct ProjectInner {
    database_name: Option<String>,            // from `database <name>;`
    members: HashMap<FileId, Url>,            // FileId(0..2) → URI
    snapshot: Option<Arc<ProjectSnapshot>>,
}

pub struct ProjectSnapshot {
    pub diagnostics_by_file: HashMap<FileId, Vec<Diagnostic>>,
    pub line_indices: HashMap<FileId, LineIndex>,
}
```

The `FileId` constants match what `coddl_plan` emits:
`FileId(0)` = `.cd`, `FileId(1)` = `.cddb`, `FileId(2)` =
`.cdstore`.

### Discovery flow

On `did_open(uri)`:

- **`.cd`**: register a project keyed on this URI's path. Parse
  the buffer to extract the `database <name>;` binding (cached on
  the project so reverse discovery is fast). Sweep already-open
  `.cddb` / `.cdstore` documents in the same directory whose names
  match the binding and bind them to this project.
- **`.cddb` / `.cdstore`**: derive the database name from the
  filename stem. Find a matching `.cd` by:
  1. Existing project whose `database_name` matches.
  2. Open `.cd` document in the same directory whose buffer-source
     binding matches.
  3. Disk scan for `.cd` files in the same directory whose
     `database <name>;` matches.
  Bind to the discovered project.
- **`.cdmap`**: not a project member. The grammar still parses
  Phase 14's nodes; non-identity adapters are deferred.

Every project member's `DocumentInner.project` field is populated
with the project's `cd_path` so `publish_diagnostics_for` can find
sibling members without scanning the projects map.

### Cache invalidation

Every `put_document(uri)` clears `Project.snapshot` if the URI
belongs to a project. The next `project_snapshot()` recomputes
via `coddl_plan::discover_and_validate_with_overrides` with every
currently-open member's buffer source fed in by path. Closed
members fall back to disk reads inside the plan layer.

### Tear-down

`close_document(uri)` removes the URI from its project's
`members` map. When the last member closes, the project entry is
dropped. A subsequent `did_open` re-discovers it from scratch.


## Diagnostic routing

`publish_diagnostics_for(uri)` merges two streams:

1. **Per-document**: the URI's own `Snapshot.diagnostics`,
   converted through `lsp_convert::diagnostic` using the
   document's `LineIndex`.
2. **Per-project**: if the URI belongs to a project, take its
   `FileId` (looked up via `Project.members`), grab
   `ProjectSnapshot.diagnostics_by_file[fid]`, and convert each
   one through `lsp_convert::diagnostic` using
   `ProjectSnapshot.line_indices[fid]`.

The merged list is published in one batch with the document's
current version. Standalone documents (no project membership)
publish only the per-document stream — preserving the Phase 13
behavior for single-file programs.

After every `did_change` / `did_open`, the analyzer also
republishes diagnostics for every other open member of the
affected project. That's how editing `greetings.cddb` refreshes
the squiggles on `hello-world-db.cd`'s public-relvar name.


## Out of scope

- **Live workspace scans / file watchers**: discovery only fires
  on `did_open`. Disk changes outside open buffers are not
  observed today; the VSCode extension's
  `createFileSystemWatcher` notifies on save and triggers a
  per-document republish.
- **Multi-program directories**: each `.cd` forms its own
  project. Sibling projects don't share state.
- **`.cdmap` membership**: Phase 14 parses it; Phase 17 ignores
  it.
- **Workspace manifests / out-of-directory catalogs**:
  `coddl.toml` and cross-directory projects are deferred.
