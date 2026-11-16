# Cunaguaro technical guide

[Back to the Cunaguaro overview](README.md).

Cunaguaro resolves Peregrino's distinct `taxa(id, name)` names by exact Wikidata [taxon name (P225)](https://www.wikidata.org/wiki/Property:P225). Mato's normal workflow builds **one shared taxon index** and **one shared chemical index** from [QLever's Wikidata endpoint](https://qlever.dev/wikidata), then reuses them read-only across jobs. Peregrino `ingest` creates the local taxa from the configured metadata column; blank/quality-control rows remain unlinked. Choose destinations in a private `mato/config/wikidata.local.toml` based on the portable `mato/config/wikidata.toml`. Pass that file to `mato indexes refresh --config`, and set `wikidata_config` in each private job TOML; see the [end-to-end procedure](#build-and-query-a-knowledge-graph-end-to-end). Standalone Cunaguaro CLI examples remain in the advanced sections below.


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

No match means “absent from this QLever snapshot under exact P225,” **not** that no taxon exists. Matching is conservative, not taxonomic-status reconciliation. A missing/incomplete shared index fails before updating the job DB; build it first. Set each job's `wikidata_config` to the same private shared-index TOML used by `indexes refresh`; `mato reset` does not delete shared indexes.

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

`chemical-index` streams the complete `wdt:P235` QID/full-InChIKey associations plus available `wdt:P662` PubChem CIDs and English labels, verifies the count, and replaces the shared snapshot **atomically**; `--from-db` copies **only** snapshot tables and needs no download. Its parent directory is created if absent. A truncated, malformed, or failed export/copy retains the previous complete snapshot. The command prints pair count, generation, and retrieval time; schedule refreshes weekly if desired, but do not run them alongside other writers of the index file. The retrieval time is **not** Wikidata's most recent edit: QLever's source may lag. Inspect `wikidata_chemical_snapshot` for source and timestamp. Job DBs store only their own matched candidates and the index generation used; they open the shared snapshot **read-only**. Replacing it increments the generation and invalidates each job's old matches/misses on its next `chemical-enrich`. An explicitly selected `--index` without a complete snapshot fails instead of silently falling back to WDQS. The index is external metadata, not evidence of chemical presence. Resetting a job DB does not delete the separate shared index. A locally hosted full Wikidata QLever endpoint can be selected with `--endpoint URL`.

## Local KG mapping proposal

`cargo run --manifest-path cunaguaro/Cargo.toml -- propose --db /path/to/flock.sqlite --unresolved-log unresolved.tsv --svg graph.svg --mermaid graph.mmd > graph.md` reads the existing database without modifying it. Save the Markdown report alongside SVG/Mermaid with the **same basename** (`graph.md` in this example): separate diagram links navigate to its headings; embedded diagrams in the report use fragment links. It prints a **conceptual crosswalk** between selected source fields and local graph classes/predicates, a source-field coverage inventory (including unmodeled columns), and clickable Mermaid/SVG diagrams. A dataset DOI, taxon name, full InChIKey, and PubChem CID are values of existing Wikidata properties **when asserted about an eligible item**, not QID nodes or automatically minted PIDs. A raw file is not one spectrum; a sample metadata record is neither an instrument acquisition nor Picoto feature extraction.

The TSV columns are `source`, `key_json`, `status`, `reason`. It reports unresolved taxon and *full-InChIKey* chemical alignments, excluding ordinary local observations: an unresolved match is not a request to create a Wikidata item. The dataset DOI has no persisted QID lookup, and this command does not guess one. For the toy database, inspect the generated [Markdown report](toy-kg-schema.md), [clickable SVG](toy-kg-schema.svg), [clickable Mermaid](toy-kg-schema.mmd), and [unresolved alignments](toy-kg-unresolved.tsv). The older `toy-schema.md`, `toy-schema.mmd`, `toy-schema.svg`, and `toy-unresolved.tsv` were produced by an **obsolete row-as-item proposal** and must not be used as a current mapping. The executable local RDF export, SHACL validation, and SPARQL queries are described in [Peregrino's KG instructions](https://github.com/oolonek/peregrino/blob/master/TECHNICAL.md#wikidata-linked-local-knowledge-graph).

## Build and query a knowledge graph, end to end

The executable graph comes from **Peregrino `kg export`**, not from `cunaguaro propose`: Cunaguaro builds shared Wikidata indexes, reconciles taxon and chemical QIDs into the job DB, and optionally reports the conceptual mapping. Mato orchestrates validation, ingest, Picoto, annotation and Cunaguaro. Use one index per Wikidata release/refresh and one **separate job DB per dataset**; a taxon/chemical index is not the job DB.

General procedure, from the `mato` directory:

1. Copy portable `config/wikidata.toml` to a private `config/wikidata.local.toml`. Choose writable, persistent, **distinct** paths for `indexes.taxon_index` and `indexes.chemical_index` outside any job DB or dataset. Copy `config/toy.toml` to `config/toy.local.toml` (or prepare another private job config); supply the actual Zenodo source, database, Picoto output, library and LOTUS locations. Keep outputs outside downloaded datasets and other jobs. Set the job's `wikidata_config` to the private shared-index TOML (paths in a job TOML resolve relative to its containing directory). Obtain raw spectra and library/LOTUS inputs first. For a different dataset, choose the ordered stages `["validate", "ingest", "enrich", "picoto", "register", "annotate", "chemical_enrich"]` as appropriate.
2. Inspect the private job and index locations before any write. Build Mato, then refresh **both** shared snapshots once with `mato indexes refresh --config config/wikidata.local.toml`. Reuse them across jobs; later refresh one independently with `--only taxon` or `--only chemical`. Do not refresh an index alongside its writers. `inspect` reports index presence and paths; do not confuse the job database with a shared snapshot.
3. Run Mato on the private toy job config. Inspect `datasets`, `taxon_resolutions`, `annotation_current_runs` and `chemical_lookups`; recorded annotation-file errors can leave a partial pipeline, so inspect those before exporting. For MassIVE top-N, choose a private copy of the MassIVE job config with a `top_n_root` isolated from production. `--topN 250` selects the first 250 metadata rows and their referenced raw files and uses `<top_n_root>/250/flock.sqlite` and `<top_n_root>/250/picoto`; pass the **same** `--topN 250` on `inspect`, `run` and every `show`. The portable MassIVE template may configure production stages differently from `top_n_stages`; inspect selected stages, paths and input files first.
4. Optional: Cunaguaro `propose` inventories mappings and unresolved links; this report is **not** RDF. Discover the actual dataset ID in the job DB; do not assume it is 1. Peregrino `kg export` creates a new N-Quads file, refuses to overwrite existing files and checks registered file content. `peregrino kg validate --graph GRAPH` checks local SHACL shapes in Rust (optional; needs memory proportional to graph size), and `peregrino kg query` queries bounded graphs. For large graphs, [index the N-Quads in QLever](https://github.com/oolonek/peregrino/blob/master/kg/QLEVER.md) instead of loading them into in-process memory.

Example using **paths you chose**, from `mato/`. Set `JOB_CONFIG` to your private job TOML, `WIKIDATA_CONFIG` to the same shared-index TOML referenced by that job's `wikidata_config`, and `DB` to the job DB shown by `inspect`. These commands exit if any required path variable is unset. Read inspected paths before running `run` or `indexes refresh`; those commands write to their configured destinations.

```sh
: "${JOB_CONFIG:?set path to your private Mato toy job TOML}"
: "${WIKIDATA_CONFIG:?set path to your private shared-index TOML}"
: "${DB:?set the job database path from JOB_CONFIG}"
./target/release/mato inspect --config "$JOB_CONFIG"
./target/release/mato indexes refresh --config "$WIKIDATA_CONFIG"
./target/release/mato run --config "$JOB_CONFIG"
./target/release/mato show datasets --config "$JOB_CONFIG"
./target/release/mato show taxon_resolutions --config "$JOB_CONFIG"
./target/release/mato show annotation_current_runs --config "$JOB_CONFIG"
./target/release/mato show chemical_lookups --config "$JOB_CONFIG"
```

After checking those results, choose `DATASET_ID` from the `datasets` output and a **new** output directory `WORK` that does not already exist. Export and inspect it separately:

```sh
: "${DB:?set the inspected job database path}"
: "${DATASET_ID:?select the ID from Mato show datasets}"
: "${WORK:?set a new directory for this export}"
mkdir -- "$WORK" || exit 1
cargo run --release --manifest-path ../cunaguaro/Cargo.toml -- propose --db "$DB" \
  --unresolved-log "$WORK/graph-unresolved.tsv" --svg "$WORK/graph.svg" \
  --mermaid "$WORK/graph.mmd" > "$WORK/graph.md"
cargo run --release --manifest-path ../peregrino/Cargo.toml -- kg export \
  --db "$DB" --dataset-id "$DATASET_ID" --out "$WORK/graph.nq"
cargo run --release --manifest-path ../peregrino/Cargo.toml -- kg query \
  --graph "$WORK/graph.nq" --query ../peregrino/kg/taxonomic-evidence.rq
```

Optional Rust SHACL check for a graph that fits in memory: `cargo run --release --manifest-path ../peregrino/Cargo.toml -- kg validate --graph "$WORK/graph.nq"`. Peregrino uses Rudof's Rust SHACL library and [local shapes](https://github.com/oolonek/peregrino/blob/master/kg/shapes.ttl); the command exits nonzero on violations. For multi-gigabyte exports, this check and in-memory `kg query` need substantial RAM and time; indexing in QLever does not require either. The `qlever` CLI and Docker are optional for a persistent endpoint; follow the [QLever deployment guide](https://github.com/oolonek/peregrino/blob/master/kg/QLEVER.md) and choose a new index directory, free localhost port and container name. Avoid re-indexing an occupied directory or replacing a running service.

For a MassIVE top-N full-stage rerun, restore the full `top_n_stages` list in the **same private TOML** and keep `--topN 250`; for a resume, shorten `top_n_stages` to the remaining stages, retain isolated outputs, and keep `--topN 250`. Do not automatically reset. Reset deletes the selected job DB and Picoto outputs: only consider it when intentionally discarding that N's outputs, after stopping writers and inspecting all paths. Sharing any job DB/Picoto pair with another job makes reset unsafe. Shared taxon and chemical indexes are not reset. A relative `top_n_root` resolves beside the job TOML, so moving the TOML can change the selected output location.

The toy DOI is [10.5281/zenodo.10018264](https://doi.org/10.5281/zenodo.10018264); configure the extracted source path before using the example. `kg export` reads the job DB but creates a graph output; use a **new** `$WORK` for every export. A candidate hit with a QID is a ranked annotation, **not proof of chemical presence**. Local samples, analyses, features, spectra and hits receive local IRIs, while taxon/chemical QIDs link to Wikidata items; the export does not download Wikidata statements into the graph. See [Peregrino KG semantics and query recipes](https://github.com/oolonek/peregrino/blob/master/TECHNICAL.md#wikidata-linked-local-knowledge-graph) and the [QLever deployment guide](https://github.com/oolonek/peregrino/blob/master/kg/QLEVER.md) for persistent services.
