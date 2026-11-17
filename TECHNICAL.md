# Cunaguaro technical guide

[Back to the Cunaguaro overview](README.md).

Cunaguaro resolves Peregrino's distinct `taxa(id, name)` names by exact Wikidata [taxon name (P225)](https://www.wikidata.org/wiki/Property:P225). Mato's workflow builds **one shared taxon index** and **one shared chemical index** from [QLever's Wikidata endpoint](https://qlever.dev/wikidata) and reuses them read-only across jobs. Peregrino `ingest` creates local taxa from the selected metadata column; blank/quality-control rows remain unlinked. Set `workspace` and `[indexes]` paths in the **same private Mato job TOML** ([toy template](../mato/config/toy.toml), [MassIVE template](../mato/config/massive.toml)). `mato run --config config/toy.local.toml` prepares configured inputs and, with `[indexes].create_missing = true`, builds missing snapshots; the same applies to a prepared MassIVE job via `mato run --config config/massive.local.toml --topN 50`. Use `mato indexes refresh --config JOB.toml` to deliberately refresh snapshots. Standalone Cunaguaro commands are documented below.


## Shared taxon index and reconciliation

The bulk snapshot stores truthy `wdt:P225` name/**item QID** associations and [Catalogue of Life P10585](https://www.wikidata.org/wiki/Property:P10585), historical [GBIF P846](https://www.wikidata.org/wiki/Property:P846), current [GBIF P14607](https://www.wikidata.org/wiki/Property:P14607) and [iNaturalist P3151](https://www.wikidata.org/wiki/Property:P3151) identifier assertions, retaining their exact Wikidata property and any multiple values; non-item Wikidata entities are excluded. P846 and P14607 identify different GBIF identifier generations and are **not** interchangeable. `wdt:` exposes Wikidata's best-ranked non-deprecated statements, not all non-deprecated statements; unlike the former Action API search, this snapshot does not match synonyms or labels. The index's source may lag current Wikidata edits. Inspect `wikidata_taxon_snapshot` for the recorded QLever endpoint, retrieval time and generation; no network access is needed during matching. The importer paginates by item, verifies each property's cardinality against QLever's start or end count, and atomically retains the previous snapshot on errors or truncation. A live QLever update during a refresh means it is not a transactionally frozen Wikidata revision. Do not run concurrent writers to one index path.

Only **one** QID for the exact name succeeds. Zero or multiple QIDs, and more than one distinct CoL ID for that QID, produce a recorded error rather than a guessed match. A missing CoL ID is valid. Results are in each job's `taxon_resolutions(taxon_id,qid,catalogue_of_life_id,error)`. All P10585/P846/P14607/P3151 identifiers for successful QIDs are copied with their property into `taxon_resolution_ids(taxon_id,property,identifier)`, including multiple iNaturalist or GBIF IDs. `taxon_resolution_versions(taxon_id,generation)` tracks which snapshot resolved a name. Successes on the same generation are reused; errors retry each invocation; after rebuilding the index, previous successes are re-evaluated automatically. `--refresh` rechecks even on the same generation. `--verbose` prints per-name results to stderr. Resolutions and IDs cascade when Peregrino prunes taxa.

```sh
sqlite3 -header -column /path/to/job/flock.sqlite \
  'SELECT t.name, r.qid, r.catalogue_of_life_id, r.error,
          group_concat(i.property || ":" || i.identifier) AS other_ids
   FROM taxa t LEFT JOIN taxon_resolutions r ON r.taxon_id=t.id
   LEFT JOIN taxon_resolution_ids i ON i.taxon_id=t.id
   GROUP BY t.id ORDER BY t.id'
```

No match means “absent from this QLever snapshot under exact P225,” **not** that no taxon exists. Matching is conservative, not taxonomic-status reconciliation. A missing/incomplete shared index fails before updating the job DB; set `[indexes].create_missing = true` to build missing snapshots during `mato run --config JOB.toml`, or deliberately use `mato indexes refresh --config JOB.toml`. An incomplete nonempty snapshot must be repaired explicitly, not silently replaced. Use the same workspace-root `[indexes]` destinations in each job config; `mato reset` does not delete shared indexes.

If an interrupted `taxon-index` refresh leaves `taxa.sqlite-journal`, the previous committed snapshot needs SQLite rollback before this read-only lookup can use it. Stop the writer and run `mato indexes recover --config JOB.toml --only taxon` to recover and check the snapshot locally, **without** querying QLever or deleting the journal yourself. The same command with `--only chemical` handles an interrupted chemical snapshot. See [Mato's recovery instructions](../mato/TECHNICAL.md#run-a-job).

### Standalone taxon CLI (advanced)

From the flock directory (the parent of `cunaguaro/`), to build a snapshot or enrich a job without Mato:

```sh
cargo run --release --manifest-path cunaguaro/Cargo.toml -- \
  taxon-index --index /path/to/shared/taxa.sqlite
cargo run --release --manifest-path cunaguaro/Cargo.toml -- \
  --db /path/to/job/flock.sqlite --index /path/to/shared/taxa.sqlite
```

## Candidate chemical enrichment

After Hocofaisan has annotated spectra, run `cargo run --release --manifest-path cunaguaro/Cargo.toml -- chemical-enrich --db /path/to/flock.sqlite`. Mato's optional `chemical_enrich` stage invokes the same command after `annotate`. It reads distinct **full** `annotation_hits.library_inchikey` values from current annotation runs, matching them locally when a complete QLever snapshot exists and otherwise querying Wikidata in bounded batches. Results remain in the same database: `chemical_lookups(inchikey,status,checked_at)`, `chemical_entities(inchikey,wikidata_qid,name)`, and `chemical_pubchem_ids(inchikey,wikidata_qid,pubchem_cid)`. Zero, one, or multiple matching QIDs and CIDs are retained; a spectral-library candidate is **not** evidence of chemical presence in the sample.

Completed keys are reused until the index is refreshed. With a snapshot present, `chemical-enrich` resolves **all** pending keys from SQLite without contacting Wikidata; even a key absent from the complete snapshot is recorded as `not_found` *as of that snapshot*. Replacing the snapshot increments its generation, so the next `chemical-enrich` rechecks prior matched, ambiguous, and missing keys automatically. `--batch-size 25` sets the per-transaction chunk, `--limit 25` caps pending keys this run, `--refresh` forces a recheck against the current source, and `--verbose` logs results. Without a snapshot, Wikidata Query Service is used as before: HTTP 502 batches are split and retried, HTTP 429 honors short numeric `Retry-After`, and failed requests remain pending for a later run. For identifier/rank semantics, see [Wikidata chemical lookup](WIKIDATA.md).

## Bulk QLever chemical index

Build **one shared SQLite index** from the [QLever Wikidata endpoint](https://qlever.dev/wikidata), then point any number of jobs at it. Run from the flock directory (the parent of `cunaguaro/`):

```sh
# First creation or a scheduled refresh (the target file may not yet exist):
cargo run --release --manifest-path cunaguaro/Cargo.toml -- chemical-index --index /path/to/shared/wikidata-chemicals.sqlite

# Alternatively, seed it offline from an already-complete index in a prior job DB:
cargo run --release --manifest-path cunaguaro/Cargo.toml -- chemical-index --index /path/to/shared/wikidata-chemicals.sqlite --from-db /path/to/old/flock.sqlite

# Neither job copies the index or contacts Wikidata:
cargo run --release --manifest-path cunaguaro/Cargo.toml -- chemical-enrich --db /path/to/job1/flock.sqlite --index /path/to/shared/wikidata-chemicals.sqlite
cargo run --release --manifest-path cunaguaro/Cargo.toml -- chemical-enrich --db /path/to/job2/flock.sqlite --index /path/to/shared/wikidata-chemicals.sqlite
```

`chemical-index` pages the complete `wdt:P235` QID/full-InChIKey associations plus available `wdt:P662` PubChem CIDs and English labels, verifies the independent pair count, and replaces the shared snapshot **atomically**; `--from-db` copies **only** snapshot tables and needs no download. A truncated, malformed, or failed page/copy retains the previous complete snapshot. Public QLever can rate-limit requests; bounded waits honor numeric `Retry-After` before retrying a page. Its parent directory is created if absent. The command prints pair count, generation, and retrieval time; schedule refreshes weekly if desired, but do not run them alongside other writers of the index file. Retrieval time is **not** Wikidata's most recent edit: QLever may lag. Inspect `wikidata_chemical_snapshot` for source and timestamp. Job DBs store only their own matched candidates and the index generation used; they open the shared snapshot **read-only**. Replacing it increments the generation and invalidates each job's old matches/misses on its next `chemical-enrich`. An explicitly selected `--index` without a complete snapshot fails instead of silently falling back to WDQS. The index is external metadata, not evidence of chemical presence. Resetting a job DB does not delete the separate shared index. A locally hosted full Wikidata QLever endpoint can be selected with `--endpoint URL`.

## Local KG mapping proposal

`cargo run --manifest-path cunaguaro/Cargo.toml -- propose --db /path/to/flock.sqlite --unresolved-log unresolved.tsv --svg graph.svg --mermaid graph.mmd > graph.md` reads the existing database without modifying it. Save the Markdown report alongside SVG/Mermaid with the **same basename** (`graph.md` in this example): separate diagram links navigate to its headings; embedded diagrams in the report use fragment links. It prints a **conceptual crosswalk** between selected source fields and local graph classes/predicates, a source-field coverage inventory (including unmodeled columns), and clickable Mermaid/SVG diagrams. A dataset DOI, taxon name, full InChIKey, and PubChem CID are values of existing Wikidata properties **when asserted about an eligible item**, not QID nodes or automatically minted PIDs. A raw file is not one spectrum; a sample metadata record is neither an instrument acquisition nor Picoto feature extraction.

The TSV columns are `source`, `key_json`, `status`, `reason`. It reports unresolved taxon and *full-InChIKey* chemical alignments, excluding ordinary local observations: an unresolved match is not a request to create a Wikidata item. The dataset DOI has no persisted QID lookup, and this command does not guess one. For the toy database, inspect the generated [Markdown report](toy-kg-schema.md), [clickable SVG](toy-kg-schema.svg), [clickable Mermaid](toy-kg-schema.mmd), and [unresolved alignments](toy-kg-unresolved.tsv). The older `toy-schema.md`, `toy-schema.mmd`, `toy-schema.svg`, and `toy-unresolved.tsv` were produced by an **obsolete row-as-item proposal** and must not be used as a current mapping. The executable local RDF export, SHACL validation, and SPARQL queries are described in [Peregrino's KG instructions](https://github.com/oolonek/peregrino/blob/master/TECHNICAL.md#wikidata-linked-local-knowledge-graph).

## Build and query a knowledge graph, end to end

The executable graph comes from **Peregrino `kg export`**, not `cunaguaro propose`. Cunaguaro builds Wikidata lookup snapshots and writes resolved taxon and chemical QIDs into a job DB; Mato runs the other species and can export and validate the graph for any configured dataset. A snapshot is **not** a job DB.

From `mato/`, the toy route needs a single private copy of [the job config](../mato/config/toy.toml). Set only its `workspace` to a writable directory outside the checkout, then run:

```sh
test -e config/toy.local.toml || cp config/toy.toml config/toy.local.toml
# Edit workspace in config/toy.local.toml.
cargo run --release -- run --config config/toy.local.toml
```

`mato run` fetches and verifies the version-pinned Zenodo toy release and configured annotation references, builds missing shared Wikidata snapshots when `[indexes].create_missing = true`, runs the configured stages through `kg_export` and `kg_validate`, and creates a fresh N-Quads graph checked against local SHACL shapes. `[dataset.archive] doi` enables managed single-ZIP preparation; without it, the dataset must already exist. `[annotation.library_source]` and `[annotation.lotus_source]` provide verified URL/MD5 reference sources (the LOTUS CSV can use `compression = "gzip"`). Re-running reuses completed inputs and DB state, never resets existing jobs and never replaces a previous graph. [Mato's technical guide](../mato/TECHNICAL.md#example-download-the-zenodo-toy-dataset-and-export-a-kg) covers verification and file locations.

For MassIVE or another prepared dataset, use a **separate** DB/Picoto output pair in its private job TOML; set `[indexes].taxon_index` and `[indexes].chemical_index` to the same shared workspace-root paths. From `mato/`, run `mato run --config config/massive.local.toml --topN 50` for the first 50 metadata rows, with isolated DB/Picoto paths and a fresh graph under `[knowledge_graph].output_dir/50`. Configure `top_n_stages` to end with `chemical_enrich`, `kg_export`, `kg_validate`, and `[knowledge_graph].allow_partial = true` to export and validate despite recorded per-file annotation errors: the report remains `partial` and Mato exits nonzero. SHACL conformity says nothing about annotation completeness. Use `mato indexes refresh --config config/massive.local.toml` only when you deliberately need new Wikidata snapshots. For large N-Quads graphs, [index in QLever](https://github.com/oolonek/peregrino/blob/master/kg/QLEVER.md) instead of loading them into memory for local SPARQL and SHACL validation. A candidate chemical QID remains a ranked annotation, **not proof** of chemical presence in a sample.
