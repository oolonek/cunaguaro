//! Read-only conceptual mapping from Peregrino facts to a local, Wikidata-linked graph.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags};

// A source-table inventory, not a list of Wikidata item classes. Views are derived.
const SOURCES: &[(&str, &str, &[&str])] = &[
    (
        "datasets",
        "Dataset scope and DOI; paths identify source inputs, not WD claims",
        &["id", "doi", "root"],
    ),
    (
        "taxa",
        "Submitted taxon name; never infer a QID from the name",
        &["id", "name"],
    ),
    (
        "sample_rows",
        "SampleRecord metadata, distinct from a physical sample or acquisition",
        &["dataset_id", "row_number", "sample_id", "taxon_id"],
    ),
    (
        "spectrum_files",
        "Acquisition and RawFile for an ingested raw file, not one spectrum",
        &["dataset_id", "filename", "row_number", "relative_path"],
    ),
    (
        "processed_spectra",
        "Registered processing outputs: MGF and feature audit files",
        &[
            "dataset_id",
            "filename",
            "ms2_path",
            "ms2_sha256",
            "features_path",
            "paired_path",
        ],
    ),
    (
        "taxon_resolutions",
        "Local reconciliation outcome, not a new WD entity",
        &["taxon_id", "qid", "catalogue_of_life_id", "error"],
    ),
    (
        "taxon_resolution_ids",
        "Recorded external identifiers for an unambiguously reconciled taxon, including GBIF and iNaturalist",
        &["taxon_id", "property", "identifier"],
    ),
    (
        "taxon_resolution_versions",
        "Index generation used to reconcile each taxon, not a biological fact",
        &["taxon_id", "generation"],
    ),
    (
        "annotation_runs",
        "Historical AnnotationRun, inputs, hashes, taxon scoring context and outcome",
        &[
            "id",
            "dataset_id",
            "filename",
            "ms2_path",
            "ms2_sha256",
            "library_path",
            "library_sha256",
            "lotus_csv_path",
            "lotus_csv_sha256",
            "config_path",
            "config_sha256",
            "status",
            "error",
            "matcher_version",
            "created_at",
            "taxon_name",
            "taxon_qid",
            "taxonomy_query",
        ],
    ),
    (
        "annotation_features",
        "Run-scoped AnnotationFeature query acceptance and precursor measurement",
        &[
            "run_id",
            "feature_id",
            "precursor_mz",
            "hit_count",
            "status",
        ],
    ),
    (
        "annotation_hits",
        "CandidateHit to a library ReferenceSpectrum, not compound presence",
        &[
            "run_id",
            "feature_id",
            "rank",
            "library_spectrum_id",
            "library_inchikey",
            "spectral_score",
            "taxonomic_score",
            "combined_score",
            "rank_before_taxonomy",
            "taxonomic_support",
            "matches",
            "ms1_deviation_ppm",
            "matched_organism_name",
            "matched_organism_wikidata",
            "matched_shared_rank",
            "matched_lotus_inchikey",
            "matched_lotus_structure_wikidata",
            "matched_lotus_reference_wikidata",
            "matched_lotus_reference_doi",
        ],
    ),
    (
        "chemical_lookups",
        "Full-key lookup status; reconciliation metadata, not a compound",
        &["inchikey", "status"],
    ),
    (
        "chemical_entities",
        "All QID matches to a full key, including ambiguous alternatives",
        &["inchikey", "wikidata_qid"],
    ),
    (
        "chemical_pubchem_ids",
        "Observed WD PubChem identifier values for each matched QID",
        &["inchikey", "wikidata_qid", "pubchem_cid"],
    ),
    (
        "wikidata_chemical_index",
        "Bulk QLever P235 lookup cache; Wikidata source data, not local sample evidence",
        &["inchikey", "wikidata_qid", "name"],
    ),
    (
        "wikidata_chemical_cids",
        "Bulk QLever P662 lookup cache, not a separate candidate chemical",
        &["inchikey", "wikidata_qid", "pubchem_cid"],
    ),
    (
        "wikidata_chemical_snapshot",
        "Source and completeness provenance for the atomic local cache",
        &["generation", "fetched_at", "endpoint", "pairs"],
    ),
    (
        "chemical_index_lookup_versions",
        "Cache generation last used for each candidate-key resolution",
        &["inchikey", "generation"],
    ),
];

fn exists(db: &Connection, table: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |row| row.get(0),
    )?)
}

fn source_coverage(db: &Connection, report: &mut impl Write) -> Result<()> {
    writeln!(report, "## Source coverage and limits\n")?;
    writeln!(
        report,
        "The mappings below describe source fields used for graph identity, linkage, measures, outcomes or reconciliation. Fields not listed as mapped stay in the authoritative SQLite/source files; this proposal does **not** assert that they have RDF predicates. Metadata JSON keys and library headers must not become invented Wikidata properties. Unknown tables/columns are disclosed rather than silently treated as mapped.\n"
    )?;
    let known = SOURCES
        .iter()
        .map(|(name, _, _)| *name)
        .collect::<BTreeSet<_>>();
    for &(table, meaning, mapped) in SOURCES {
        if !exists(db, table)? {
            writeln!(
                report,
                "- `{table}`: absent (optional stage); no facts inferred."
            )?;
            continue;
        }
        let count: i64 = db.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })?;
        let mut pragma = db.prepare(&format!("PRAGMA table_info({table})"))?;
        let columns = pragma
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let retained = columns
            .iter()
            .filter(|name| !mapped.contains(&name.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        let absent = mapped
            .iter()
            .filter(|name| !columns.iter().any(|column| column.as_str() == **name))
            .collect::<Vec<_>>();
        writeln!(
            report,
            "- `{table}` ({count} rows): {meaning}. Mapped available fields: {}. Retained at source, not modeled: {}.{}",
            mapped
                .iter()
                .filter(|name| columns.iter().any(|column| column.as_str() == **name))
                .copied()
                .collect::<Vec<_>>()
                .join(", "),
            if retained.is_empty() {
                "none".to_owned()
            } else {
                retained.join(", ")
            },
            if absent.is_empty() {
                String::new()
            } else {
                format!(
                    " Absent in this schema: {}.",
                    absent
                        .iter()
                        .map(|name| **name)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        )?;
    }
    let mut tables = db.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")?;
    for table in tables.query_map([], |row| row.get::<_, String>(0))? {
        let table = table?;
        if !known.contains(table.as_str()) {
            writeln!(
                report,
                "- `{table}`: **unmapped source table**; not claimed as graph coverage."
            )?;
        }
    }
    writeln!(
        report,
        "\n`sample_rows.attributes_json` preserves original TSV header/value pairs (including empty strings); `annotation_hits.library_headers_json` preserves library metadata. The source also retains unmodeled paths, errors and recorded measurements. Picoto audit `features_path` supplies retention time in **seconds**, quality fields and source scan IDs; these do not live in `annotation_features`. Registered `processed_spectra` stores paths and the MGF hash but not Picoto settings/version or an audit-file hash. Never claim a fully reproducible processing provenance from those absent fields. `annotation_run_status` and `annotation_current_runs` are derived views, not extra observations.\n"
    )?;
    Ok(())
}

fn log_taxa(db: &Connection, log: &mut impl Write) -> Result<u64> {
    let has_resolution = exists(db, "taxon_resolutions")?;
    let sql = if has_resolution {
        "SELECT json_array(t.id), COALESCE(r.error, 'no QID recorded') FROM taxa t LEFT JOIN taxon_resolutions r ON r.taxon_id=t.id WHERE r.qid IS NULL"
    } else {
        "SELECT json_array(id), 'taxon enrichment not run' FROM taxa"
    };
    let mut query = db.prepare(sql)?;
    let mut count = 0;
    for row in query.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (key, reason) = row?;
        writeln!(
            log,
            "taxa\t{key}\tunresolved\t{}",
            reason.replace(['\t', '\r', '\n'], " ")
        )?;
        count += 1;
    }
    Ok(count)
}

fn full_inchikey(key: &str) -> bool {
    let bytes = key.as_bytes();
    bytes.len() == 27
        && bytes[14] == b'-'
        && bytes[25] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, byte)| i == 14 || i == 25 || byte.is_ascii_uppercase())
}

fn log_chemicals(db: &Connection, log: &mut impl Write) -> Result<u64> {
    let hits = exists(db, "annotation_hits")?;
    let lookups = exists(db, "chemical_lookups")?;
    if !hits && !lookups {
        return Ok(0);
    }
    let keys = match (hits, lookups) {
        (true, true) => {
            "SELECT library_inchikey AS inchikey FROM annotation_hits WHERE library_inchikey IS NOT NULL AND trim(library_inchikey) <> '' UNION SELECT inchikey FROM chemical_lookups"
        }
        (true, false) => {
            "SELECT DISTINCT library_inchikey AS inchikey FROM annotation_hits WHERE library_inchikey IS NOT NULL AND trim(library_inchikey) <> ''"
        }
        (false, true) => "SELECT inchikey FROM chemical_lookups",
        (false, false) => unreachable!(),
    };
    let status = if lookups { "l.status" } else { "NULL" };
    let join = if lookups {
        "LEFT JOIN chemical_lookups l ON l.inchikey=k.inchikey"
    } else {
        ""
    };
    let match_count = if exists(db, "chemical_entities")? {
        "(SELECT count(*) FROM chemical_entities e WHERE e.inchikey=k.inchikey)"
    } else {
        "0"
    };
    let sql = format!(
        "SELECT k.inchikey, {status}, {match_count} FROM ({keys}) k {join} ORDER BY k.inchikey"
    );
    let mut query = db.prepare(&sql)?;
    let mut count = 0;
    for row in query.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })? {
        let (key, stored, matches) = row?;
        // Short/invalid keys have no exact full-key identity to reconcile.
        if !full_inchikey(&key) {
            continue;
        }
        let (state, reason) = if matches > 1 {
            ("ambiguous", "multiple Wikidata QIDs for the full key")
        } else if stored.as_deref() == Some("matched") && matches == 1 {
            continue;
        } else if stored.as_deref() == Some("not_found") && matches != 0 {
            ("ambiguous", "recorded no match conflicts with stored QID")
        } else if stored.as_deref() == Some("not_found") {
            ("not_found", "lookup recorded no Wikidata match")
        } else if stored.as_deref() == Some("ambiguous") || stored.as_deref() == Some("matched") {
            (
                "ambiguous",
                "recorded lookup and QID count do not establish one match",
            )
        } else {
            ("not_looked_up", "no completed Wikidata lookup for this key")
        };
        writeln!(
            log,
            "chemical_lookups\t{}\t{state}\t{reason}",
            serde_json::to_string(&vec![key])?
        )?;
        count += 1;
    }
    Ok(count)
}

/// Print a read-only local graph crosswalk and a TSV of unresolved external alignments.
/// No local observation requires a Wikidata QID. Neither output changes the database.
pub fn propose(database: &Path, report: &mut impl Write, log: &mut impl Write) -> Result<()> {
    let db = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open existing Peregrino database {}", database.display()))?;
    for table in ["datasets", "taxa", "sample_rows", "spectrum_files"] {
        if !exists(&db, table)? {
            bail!("not a Peregrino database (missing core table {table})");
        }
    }
    writeln!(
        report,
        "# Local metabolomics graph and Wikidata alignment proposal\n"
    )?;
    writeln!(
        report,
        "This is a **read-only conceptual mapping**, not an upload plan or an RDF export. In a subject–predicate–object statement, the **subject** is a local IRI or verified Wikidata item, the **predicate** is an RDF/PROV/local term or real Wikidata property, and the **object** is a class, linked node or typed literal. A Wikidata PID is never an entity QID; a database row is not automatically a Wikidata item.\n"
    )?;
    writeln!(report, "## Wikidata alignment\n")?;
    writeln!(
        report,
        "| Subject (only if the item exists and is verified) | Predicate and Wikidata datatype | Object |\n| --- | --- | --- |"
    )?;
    writeln!(
        report,
        "| Dataset QID (not currently stored or reconciled here) | [wdt:P356 DOI](https://www.wikidata.org/wiki/Property:P356) · External identifier | `datasets.doi` |"
    )?;
    writeln!(
        report,
        "| Taxon QID from successful `taxon_resolutions.qid` | [wdt:P225 taxon name](https://www.wikidata.org/wiki/Property:P225) · String | `taxa.name`, inspect existing claim |"
    )?;
    writeln!(
        report,
        "| Taxon QID with recorded ID | [wdt:P10585 Catalogue of Life ID](https://www.wikidata.org/wiki/Property:P10585) · External identifier | `taxon_resolutions.catalogue_of_life_id`, inspect existing claim |"
    )?;
    writeln!(
        report,
        "| Taxon QID with recorded historical GBIF ID | [wdt:P846 GBIF species ID (before 2026)](https://www.wikidata.org/wiki/Property:P846) · External identifier | `taxon_resolution_ids.identifier` where `property='P846'`; do not equate with new GBIF IDs |"
    )?;
    writeln!(
        report,
        "| Taxon QID with recorded current GBIF ID | [wdt:P14607 GBIF taxon ID](https://www.wikidata.org/wiki/Property:P14607) · External identifier | `taxon_resolution_ids.identifier` where `property='P14607'` |"
    )?;
    writeln!(
        report,
        "| Taxon QID with recorded iNaturalist ID | [wdt:P3151 iNaturalist taxon ID](https://www.wikidata.org/wiki/Property:P3151) · External identifier | `taxon_resolution_ids.identifier` where `property='P3151'`; preserve multiple values |"
    )?;
    writeln!(
        report,
        "| Chemical QID only for **one** exact full-key match | [wdt:P235 InChIKey](https://www.wikidata.org/wiki/Property:P235) · External identifier | full `chemical_entities.inchikey` |"
    )?;
    writeln!(
        report,
        "| Same matched chemical QID with recorded CID | [wdt:P662 PubChem CID](https://www.wikidata.org/wiki/Property:P662) · External identifier | `chemical_pubchem_ids.pubchem_cid` |\n"
    )?;
    writeln!(
        report,
        "QIDs use `http://www.wikidata.org/entity/Q…`; these PIDs are *existing predicates* on Wikidata subjects, not local columns to mint. DOI alone does not prove a dataset QID: no dataset DOI reconciliation is recorded, so the local Dataset keeps its DOI and no dataset QID is asserted. Taxon name and Catalogue of Life ID are source/lookup values, not a license to add claims. `matched_organism_wikidata` and `annotation_runs.taxon_qid` are recorded scoring context, not automatic identities of the sample. Ambiguous chemical QIDs remain alternatives, never a selected link; short InChIKeys cannot establish full-key identity. No WD item is proposed per feature, spectrum, sample, file or hit.\n"
    )?;
    writeln!(
        report,
        "## Local domain nodes\n\nAll classes and predicates below use `urn:mato:term:` unless prefixed `rdf:`, `prov:` or `wdt:`. Local dataset identity is scoped by DOI; metadata records additionally use a digest of the **ingested sample rows** and row number, not a mutable source file. Acquisitions use the scoped sample and raw-file digest; Picoto processing uses raw, MGF and audit digests; features use processing identity and feature ID; reference spectra use library digest and spectrum ID. Bare file names, sample IDs, feature IDs and row integers are not globally unique. These are graph nodes, **not Wikidata item creation requests**.\n"
    )?;
    for (name, meaning) in [
        (
            "Dataset",
            "local dataset scope; DOI is a value, optional verified WD dataset QID is an external alignment",
        ),
        (
            "SampleRecord",
            "one metadata row; sampleIdentifier and submitted taxon; neither a physical specimen nor an MS run",
        ),
        (
            "Acquisition",
            "MS acquisition associated with a sample record; source `spectrum_files` identifies its raw file, not individual scans",
        ),
        (
            "RawFile",
            "raw acquisition file with recorded relative path and filename; not a spectrum",
        ),
        (
            "Processing",
            "Picoto feature extraction using a raw file and producing registered outputs; settings/version absent in registration",
        ),
        (
            "FeatureAuditFile",
            "registered Picoto audit TSV, including rejected rows; an output of Processing",
        ),
        (
            "ProcessedSpectrumFile",
            "registered derived MS2 or paired MGF file; contains query spectra but is not itself one spectrum",
        ),
        (
            "Feature",
            "accepted audit feature keyed by registered source file and feature ID; not a chemical",
        ),
        (
            "QuerySpectrum",
            "feature-specific derived MS2 MGF spectrum, distinct from source raw scans and MGF file",
        ),
        (
            "AnnotationRun",
            "historical Hocofaisan attempt on registered MGF; includes error and no-hit outcomes",
        ),
        (
            "AnnotationFeature",
            "run-scoped record of an accepted query, including zero-hit outcomes; links to a Picoto Feature only when its MGF hash is current",
        ),
        (
            "CandidateHit",
            "one ranked match per run, feature and rank; never a compound-presence claim",
        ),
        (
            "ReferenceSpectrum",
            "library spectrum keyed by library identity/hash and library spectrum ID; not a raw file or compound",
        ),
    ] {
        writeln!(report, "### {name}\n\n{meaning}.\n")?;
    }
    writeln!(
        report,
        "## Local subject–predicate–object mapping\n\n| Subject | Predicate | Object (source) |\n| --- | --- | --- |"
    )?;
    for (subject, predicate, object) in [
        (
            "Dataset",
            "wdt:P356",
            "literal `datasets.doi` on the local dataset, without inventing a QID",
        ),
        (
            "Dataset",
            "hasSampleRecord",
            "SampleRecord (`sample_rows.dataset_id`)",
        ),
        (
            "SampleRecord",
            "sampleIdentifier",
            "literal `sample_rows.sample_id` (not unique; optional)",
        ),
        (
            "SampleRecord",
            "reportedSourceTaxon",
            "resolved taxon QID only if recorded; otherwise retain the submitted `taxa.name` locally",
        ),
        (
            "SampleRecord",
            "hasAcquisition",
            "Acquisition (`spectrum_files.dataset_id, row_number`)",
        ),
        (
            "Acquisition",
            "rawFile",
            "RawFile (`spectrum_files.relative_path`, `filename`)",
        ),
        (
            "Processing",
            "prov:used",
            "RawFile (raw Picoto input, never its output)",
        ),
        (
            "ProcessedSpectrumFile",
            "prov:wasGeneratedBy",
            "Processing (`processed_spectra.ms2_path`, SHA-256 `ms2_sha256`)",
        ),
        (
            "FeatureAuditFile",
            "prov:wasGeneratedBy",
            "Processing (`processed_spectra.features_path`, SHA-256 computed from audit bytes)",
        ),
        (
            "Processing",
            "processedSpectrumFile",
            "ProcessedSpectrumFile (registered MS2 MGF)",
        ),
        (
            "Processing",
            "featureAudit",
            "FeatureAuditFile (registered audit TSV)",
        ),
        (
            "Processing",
            "hasFeature",
            "Feature (accepted feature in the Picoto audit TSV)",
        ),
        (
            "Feature",
            "retentionTimeSeconds",
            "numeric `rt_seconds` from the audit TSV, not SQLite",
        ),
        (
            "Feature",
            "querySpectrum",
            "QuerySpectrum (feature-specific MS2 block in registered MGF)",
        ),
        (
            "QuerySpectrum",
            "derivedFrom",
            "ProcessedSpectrumFile (registered MGF output; selected raw scans in audit are distinct)",
        ),
        (
            "QuerySpectrum",
            "prov:wasGeneratedBy",
            "Processing; query spectrum is an output, not the raw input",
        ),
        (
            "Feature",
            "precursorMz",
            "numeric `mz` from the accepted Picoto audit TSV",
        ),
        (
            "AnnotationRun",
            "prov:used",
            "registered MGF query output and library inputs, with recorded hashes",
        ),
        (
            "AnnotationRun",
            "matcherVersion / createdAt",
            "recorded scoring software and UTC run timestamp",
        ),
        (
            "AnnotationRun",
            "scoringTaxon",
            "recorded `annotation_runs.taxon_qid` when valid; historical scoring context, not necessarily today's sample taxon",
        ),
        (
            "AnnotationRun",
            "scoringTaxonName / taxonomyQuery",
            "recorded query and taxon name even when a QID is not available",
        ),
        (
            "CandidateHit",
            "feature",
            "Feature only if the annotation run's MGF hashes match current processing; stale hits retain run-scoped IDs",
        ),
        (
            "AnnotationRun",
            "annotationFeature",
            "run-scoped AnnotationFeature (including zero-hit queries and status)",
        ),
        (
            "AnnotationFeature",
            "feature",
            "Feature only if the annotation run matches current registered processing",
        ),
        (
            "CandidateHit",
            "referenceSpectrum",
            "ReferenceSpectrum (`annotation_hits.library_spectrum_id` scoped to library hash)",
        ),
        (
            "CandidateHit",
            "candidateChemical",
            "chemical QID **only** for one exact full-key match; never presence",
        ),
        (
            "CandidateHit",
            "rank",
            "numeric final within-feature `annotation_hits.rank`",
        ),
        (
            "CandidateHit",
            "spectralScore",
            "numeric `annotation_hits.spectral_score`",
        ),
        (
            "CandidateHit",
            "taxonomicScore",
            "numeric `annotation_hits.taxonomic_score`",
        ),
        (
            "CandidateHit",
            "combinedScore",
            "numeric `annotation_hits.combined_score`; scores are not probabilities",
        ),
        (
            "CandidateHit",
            "rankBeforeTaxonomy / taxonomicSupport",
            "recorded original rank and support outcome, distinct from the final rank",
        ),
        (
            "CandidateHit",
            "matchedPeakCount / ms1DeviationPpm",
            "recorded matched peak count and signed precursor mass error",
        ),
        (
            "CandidateHit",
            "matchedOrganism / matchedOrganismName / matchedSharedRank",
            "matched LOTUS biosource organism QID where recorded and valid, name and shared taxonomic rank; not sample provenance",
        ),
        (
            "CandidateHit",
            "matchedLotusInchikey / matchedLotusStructure / matchedLotusReference / matchedLotusReferenceDoi",
            "selected LOTUS biosource row's full structure key, structure QID and cited reference QID/DOI; distinct from the candidate chemical",
        ),
        (
            "AnnotationRun / AnnotationFeature",
            "status",
            "recorded hits, no_hits or error outcomes where applicable",
        ),
        (
            "AnnotationRun",
            "metric",
            "read from config only when actual bytes match its recorded SHA-256",
        ),
        (
            "RawFile / processed files / run inputs",
            "sha256",
            "raw and output bytes hashed during export; registered MGF hash checked against SQLite",
        ),
    ] {
        writeln!(report, "| {subject} | `{predicate}` | {object} |")?;
    }
    writeln!(
        report,
        "\nUse `rdf:type` for each local node's `urn:mato:term:Class`; `prov:wasGeneratedBy` and `prov:used` describe actual artifact/activity provenance, with `derivedFrom` for a derived output. Registered `ms2_path` is a ProcessedSpectrumFile output containing feature-specific QuerySpectrum blocks; `features_path` is a FeatureAuditFile, not the feature itself. `annotation_features` represents run-scoped query observations, including zero-hit features; rejected audit rows are not query spectra. The scoring taxon of a historical run and matched organism of its reference hit must not be conflated with a sample's reported source taxon. Score and taxonomic bonus rank candidate evidence; neither is a probability or proof of chemical presence in the sample. Preserve historical runs and multiple candidate hits.\n"
    )?;
    source_coverage(&db, report)?;
    writeln!(log, "source\tkey_json\tstatus\treason")?;
    let count = log_taxa(&db, log)? + log_chemicals(&db, log)?;
    writeln!(
        report,
        "## Unresolved external alignments\n\n{count} unresolved taxon or **full-InChIKey** chemical alignments written to the TSV log (`source`, JSON key, status, reason). Short/invalid keys remain source data, not fake full-key lookup failures. A missing/ambiguous match does **not** mean 'create Wikidata item'. Dataset DOI QID resolution has not been attempted or persisted, so no false 'unresolved dataset' entry is emitted. Routine local nodes are deliberately absent from this log. No database or Wikidata writes were performed."
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_local_nodes_without_logging_them_and_preserves_lookup_outcomes() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let db = Connection::open(file.path()).unwrap();
        db.execute_batch(
            "CREATE TABLE datasets(id INTEGER PRIMARY KEY,doi TEXT,root TEXT);
             INSERT INTO datasets VALUES(1,'10.1/example','/source');
             CREATE TABLE taxa(id INTEGER PRIMARY KEY,name TEXT);
             INSERT INTO taxa VALUES(1,'Resolved'),(2,'Missing');
             CREATE TABLE taxon_resolutions(taxon_id INTEGER PRIMARY KEY,qid TEXT,error TEXT);
             INSERT INTO taxon_resolutions VALUES(1,'Q1',NULL),(2,NULL,'no exact match');
             CREATE TABLE sample_rows(dataset_id INTEGER,row_number INTEGER,attributes_json TEXT,PRIMARY KEY(dataset_id,row_number));
             INSERT INTO sample_rows VALUES(1,1,'{\"sample_id\":\"S1\"}');
             CREATE TABLE spectrum_files(dataset_id INTEGER,filename TEXT,PRIMARY KEY(dataset_id,filename));
             INSERT INTO spectrum_files VALUES(1,'raw.mzXML');
             CREATE TABLE annotation_hits(run_id INTEGER,feature_id TEXT,rank INTEGER,library_inchikey TEXT,PRIMARY KEY(run_id,feature_id,rank));
             INSERT INTO annotation_hits VALUES(1,'f',1,'AAAAAAAAAAAAAA-BBBBBBBBBB-C'),(1,'f',2,'DDDDDDDDDDDDDD-EEEEEEEEEE-F'),(1,'f',3,'GGGGGGGGGGGGGG-HHHHHHHHHH-I'),(1,'f',4,'SHORTKEY');
             CREATE TABLE chemical_lookups(inchikey TEXT PRIMARY KEY,status TEXT);
             INSERT INTO chemical_lookups VALUES('DDDDDDDDDDDDDD-EEEEEEEEEE-F','ambiguous'),('GGGGGGGGGGGGGG-HHHHHHHHHH-I','matched'),('JJJJJJJJJJJJJJ-KKKKKKKKKK-L','not_found');
             CREATE TABLE chemical_entities(inchikey TEXT,wikidata_qid TEXT,PRIMARY KEY(inchikey,wikidata_qid));
             INSERT INTO chemical_entities VALUES('DDDDDDDDDDDDDD-EEEEEEEEEE-F','Q2'),('DDDDDDDDDDDDDD-EEEEEEEEEE-F','Q3'),('GGGGGGGGGGGGGG-HHHHHHHHHH-I','Q4');"
        ).unwrap();
        let (mut report, mut log) = (Vec::new(), Vec::new());
        propose(file.path(), &mut report, &mut log).unwrap();
        let report = String::from_utf8(report).unwrap();
        let log = String::from_utf8(log).unwrap();
        for node in [
            "### SampleRecord",
            "### Acquisition",
            "### RawFile",
            "### Processing",
            "### FeatureAuditFile",
            "### ProcessedSpectrumFile",
            "### Feature",
            "### QuerySpectrum",
            "### AnnotationFeature",
            "### ReferenceSpectrum",
            "### CandidateHit",
        ] {
            assert!(report.contains(node), "missing {node}");
        }
        assert!(report.contains("[wdt:P356 DOI]"));
        assert!(report.contains("`prov:used` | RawFile"));
        assert!(report.contains("never a compound-presence claim"));
        assert!(log.contains("taxa\t[2]\tunresolved\tno exact match"));
        assert!(log.contains("[\"AAAAAAAAAAAAAA-BBBBBBBBBB-C\"]\tnot_looked_up"));
        assert!(!log.contains("SHORTKEY"));
        assert!(log.contains("[\"DDDDDDDDDDDDDD-EEEEEEEEEE-F\"]\tambiguous"));
        assert!(log.contains("[\"JJJJJJJJJJJJJJ-KKKKKKKKKK-L\"]\tnot_found"));
        assert!(!log.contains("[\"GGGGGGGGGGGGGG-HHHHHHHHHH-I\"]"));
        assert!(!log.contains("sample_rows\t"));
        assert!(!log.contains("spectrum_files\t"));
        assert!(!log.contains("datasets\t"));
    }

    #[test]
    fn discloses_unmapped_sources_without_claiming_coverage() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let db = Connection::open(file.path()).unwrap();
        db.execute_batch(
            "CREATE TABLE datasets(id INTEGER PRIMARY KEY,doi TEXT,root TEXT,extra TEXT);
            CREATE TABLE taxa(id INTEGER PRIMARY KEY,name TEXT);
            CREATE TABLE sample_rows(id INTEGER PRIMARY KEY);
            CREATE TABLE spectrum_files(id INTEGER PRIMARY KEY);
            CREATE TABLE added_facts(id INTEGER PRIMARY KEY);",
        )
        .unwrap();
        let mut report = Vec::new();
        propose(file.path(), &mut report, &mut Vec::new()).unwrap();
        let report = String::from_utf8(report).unwrap();
        assert!(report.contains("`added_facts`: **unmapped source table**"));
        assert!(report.contains("Retained at source, not modeled: extra"));
    }
}
