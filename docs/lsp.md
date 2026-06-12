# Coddl language server

This document is the authoritative spec for the LSP layer's
per-document and per-project models: how documents are tracked,
how snapshots are computed and cached, how the analyzer discovers
multi-file projects, and how diagnostics flow from the frontend
crates back to editor buffers.

For *why* the LSP is shaped this way, see `ARCHITECTURE.md §12
"Editor tooling"`. This document never duplicates that rationale —
it points at it and gets on with the rules.

**Last sync:** Phase 17. Every commit that changes the analyzer's
project model, snapshot caching, or diagnostic routing updates
this file in the same commit.


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
