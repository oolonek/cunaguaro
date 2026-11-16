//! Enrich current spectral-library candidates by exact full InChIKey.

#[cfg(test)]
#[path = "chemical_tests.rs"]
mod tests;
#[path = "chemical_index.rs"]
mod index;

pub use index::{QLEVER, Snapshot, copy_snapshot};
use index::refresh_at;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, RETRY_AFTER};
use rusqlite::{Connection, OpenFlags, params};
use serde_json::Value;

#[derive(Debug)]
struct RateLimited(Duration);

impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Wikidata Query Service rate limited (Retry-After: {} seconds)",
            self.0.as_secs()
        )
    }
}

impl std::error::Error for RateLimited {}

const WDQS: &str = "https://query.wikidata.org/sparql";
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS chemical_lookups (
    inchikey TEXT PRIMARY KEY,
    status TEXT NOT NULL CHECK (status IN ('matched','not_found','ambiguous')),
    checked_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE TABLE IF NOT EXISTS chemical_entities (
    inchikey TEXT NOT NULL REFERENCES chemical_lookups(inchikey) ON DELETE CASCADE,
    wikidata_qid TEXT NOT NULL,
    name TEXT,
    PRIMARY KEY(inchikey,wikidata_qid)
);
CREATE TABLE IF NOT EXISTS chemical_pubchem_ids (
    inchikey TEXT NOT NULL,
    wikidata_qid TEXT NOT NULL,
    pubchem_cid TEXT NOT NULL,
    PRIMARY KEY(inchikey,wikidata_qid,pubchem_cid),
    FOREIGN KEY(inchikey,wikidata_qid)
        REFERENCES chemical_entities(inchikey,wikidata_qid) ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS chemical_index_lookup_versions (
    inchikey TEXT PRIMARY KEY,
    generation INTEGER NOT NULL
);
"#;

/// A structure identified by its full InChIKey, not its short connectivity key.
pub trait ChemicalStructure {
    fn full_inchikey(&self) -> &str;
}

impl ChemicalStructure for str {
    fn full_inchikey(&self) -> &str {
        self
    }
}

impl ChemicalStructure for String {
    fn full_inchikey(&self) -> &str {
        self
    }
}

/// A Wikidata item whose exact P235 statement identifies a chemical structure.
/// Neither the item nor a spectral-library hit proves presence in a sample.
pub trait ChemicalEntity: ChemicalStructure {
    fn wikidata_qid(&self) -> &str;
    fn name(&self) -> Option<&str>;
    fn pubchem_cids(&self) -> &[String];
}

/// Metadata for one Wikidata item, retaining every distinct PubChem CID (P662).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WikidataChemical {
    pub inchikey: String,
    pub qid: String,
    pub name: Option<String>,
    pub pubchem_cids: Vec<String>,
}

impl ChemicalStructure for WikidataChemical {
    fn full_inchikey(&self) -> &str {
        &self.inchikey
    }
}

impl ChemicalEntity for WikidataChemical {
    fn wikidata_qid(&self) -> &str {
        &self.qid
    }

    fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    fn pubchem_cids(&self) -> &[String] {
        &self.pubchem_cids
    }
}

/// One enrichment run; `limit` caps new lookups, not the number of source hits.
pub struct EnrichOptions {
    pub database: PathBuf,
    /// Optional shared SQLite snapshot; read-only and never copied into this job.
    pub index: Option<PathBuf>,
    pub batch_size: usize,
    pub limit: Option<usize>,
    pub refresh: bool,
    pub verbose: bool,
}

/// Counts for distinct full InChIKeys, rather than spectral hit rows.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub distinct_keys: usize,
    pub reused: usize,
    pub pending: usize,
    pub attempted: usize,
    pub matched: usize,
    pub not_found: usize,
    pub ambiguous: usize,
}

/// Resolve current full InChIKeys against a complete local snapshot, if present,
/// otherwise bounded WDQS queries. Recheck cached keys when the index changes.
pub fn enrich(options: &EnrichOptions) -> Result<Summary> {
    enrich_at(options, WDQS)
}

/// Replace a standalone local QLever P235 snapshot; incomplete downloads retain the old index.
pub fn refresh_snapshot(database: &Path, endpoint: &str) -> Result<Snapshot> {
    refresh_at(database, endpoint)
}

fn require_annotations(db: &Connection) -> Result<()> {
    let present: i64 = db.query_row(
        "SELECT count(*) FROM sqlite_master WHERE
         (name='annotation_hits' AND type='table') OR
         (name='annotation_current_runs' AND type='view')",
        [],
        |row| row.get(0),
    )?;
    ensure!(
        present == 2,
        "annotation hits and current-run view are required; run hocofaisan first"
    );
    Ok(())
}

fn pending_keys(
    db: &Connection,
    options: &EnrichOptions,
    summary: &mut Summary,
    generation: Option<i64>,
) -> Result<Vec<String>> {
    let sql = if generation.is_some() {
        "SELECT k.inchikey, l.status, v.generation FROM (
             SELECT DISTINCT h.library_inchikey AS inchikey
             FROM annotation_current_runs AS r JOIN annotation_hits AS h ON h.run_id = r.id
             WHERE r.status = 'hits' AND h.library_inchikey IS NOT NULL
               AND trim(h.library_inchikey) <> ''
         ) k LEFT JOIN chemical_lookups l ON l.inchikey=k.inchikey
         LEFT JOIN chemical_index_lookup_versions v ON v.inchikey=k.inchikey ORDER BY k.inchikey"
    } else {
        "SELECT k.inchikey, l.status, NULL FROM (
             SELECT DISTINCT h.library_inchikey AS inchikey
             FROM annotation_current_runs AS r JOIN annotation_hits AS h ON h.run_id = r.id
             WHERE r.status = 'hits' AND h.library_inchikey IS NOT NULL
               AND trim(h.library_inchikey) <> ''
         ) k LEFT JOIN chemical_lookups l ON l.inchikey=k.inchikey ORDER BY k.inchikey"
    };
    let mut query = db.prepare(sql)?;
    let mut rows = query.query([])?;
    let mut pending = Vec::new();
    while let Some(row) = rows.next()? {
        summary.distinct_keys += 1;
        let status: Option<&str> = row.get_ref(1)?.as_str().ok();
        let lookup_generation: Option<i64> = row.get(2)?;
        if !options.refresh && status.is_some()
            && generation.is_none_or(|current| lookup_generation == Some(current)) {
            summary.reused += 1;
            continue;
        }
        summary.pending += 1;
        if pending.len() < options.limit.unwrap_or(usize::MAX) {
            pending.push(row.get(0)?);
        }
    }
    Ok(pending)
}

fn sparql_query(keys: &[String]) -> Result<String> {
    let mut query = String::from(
        "PREFIX wdt: <http://www.wikidata.org/prop/direct/>\n\
         PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>\n\
         SELECT DISTINCT ?inchikey ?item ?name ?cid WHERE {\n\
         VALUES ?inchikey {",
    );
    for key in keys {
        query.push(' ');
        query.push_str(&serde_json::to_string(key)?);
    }
    query.push_str(
        " }\n?item wdt:P235 ?inchikey .\n\
         OPTIONAL { ?item rdfs:label ?name FILTER(LANG(?name) = \"en\") }\n\
         OPTIONAL { ?item wdt:P662 ?cid }\n}",
    );
    Ok(query)
}

fn binding<'a>(row: &'a Value, name: &str) -> Result<Option<&'a str>> {
    match row.get(name) {
        None => Ok(None),
        Some(field) => field["value"]
            .as_str()
            .map(Some)
            .with_context(|| format!("WDQS binding {name} is not a string")),
    }
}

fn required_binding<'a>(row: &'a Value, name: &str) -> Result<&'a str> {
    binding(row, name)?.with_context(|| format!("WDQS binding {name} is missing"))
}

fn parse_qid(uri: &str) -> Result<&str> {
    let qid = uri
        .strip_prefix("http://www.wikidata.org/entity/")
        .or_else(|| uri.strip_prefix("https://www.wikidata.org/entity/"))
        .context("WDQS item is not a Wikidata entity URI")?;
    ensure!(
        qid.strip_prefix('Q')
            .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())),
        "WDQS returned invalid item QID: {uri}"
    );
    Ok(qid)
}

fn lookup_batch(
    client: &Client,
    endpoint: &str,
    keys: &[String],
) -> Result<Vec<(String, Vec<WikidataChemical>)>> {
    let query = sparql_query(keys)?;
    let response = client
        .get(endpoint)
        .header(ACCEPT, "application/sparql-results+json")
        .query(&[("query", query.as_str()), ("format", "json")])
        .send()
        .context("Wikidata Query Service request")?;
    if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("not supplied");
        let seconds = retry_after.parse::<u64>().ok().filter(|&value| value <= 1800)
            .with_context(|| format!(
                "Wikidata Query Service rate limited (Retry-After: {retry_after}); cannot wait automatically; rerun after the indicated delay"
            ))?;
        return Err(RateLimited(Duration::from_secs(seconds.max(1))).into());
    }
    let response: Value = response
        .error_for_status()?
        .json()
        .context("WDQS JSON response")?;
    let rows = response["results"]["bindings"]
        .as_array()
        .context("WDQS response is missing results.bindings")?;
    let mut grouped: BTreeMap<&str, BTreeMap<String, WikidataChemical>> = keys
        .iter()
        .map(|key| (key.as_str(), BTreeMap::new()))
        .collect();
    for row in rows {
        let inchikey = required_binding(row, "inchikey")?;
        let entities = grouped
            .get_mut(inchikey)
            .with_context(|| format!("WDQS returned an unrequested InChIKey: {inchikey}"))?;
        let qid = parse_qid(required_binding(row, "item")?)?;
        let name = binding(row, "name")?;
        let cid = binding(row, "cid")?;
        let chemical = entities
            .entry(qid.to_owned())
            .or_insert_with(|| WikidataChemical {
                inchikey: inchikey.to_owned(),
                qid: qid.to_owned(),
                name: name.map(str::to_owned),
                pubchem_cids: Vec::new(),
            });
        if let Some(name) = name {
            if let Some(existing) = chemical.name.as_deref() {
                ensure!(
                    existing == name,
                    "WDQS gave conflicting English labels for {qid}"
                );
            } else {
                chemical.name = Some(name.to_owned());
            }
        }
        if let Some(cid) = cid {
            chemical.pubchem_cids.push(cid.to_owned());
        }
    }
    Ok(grouped
        .into_iter()
        .map(|(key, entities)| {
            let mut entities = entities.into_values().collect::<Vec<_>>();
            for entity in &mut entities {
                entity.pubchem_cids.sort_unstable();
                entity.pubchem_cids.dedup();
            }
            (key.to_owned(), entities)
        })
        .collect())
}

fn save_batch(
    db: &mut Connection,
    matches: &[(String, Vec<WikidataChemical>)],
    summary: &mut Summary,
    generation: Option<i64>,
    verbose: bool,
) -> Result<()> {
    let tx = db.transaction()?;
    for (inchikey, entities) in matches {
        let status = match entities.len() {
            0 => "not_found",
            1 => "matched",
            _ => "ambiguous",
        };
        tx.execute(
            "INSERT INTO chemical_lookups(inchikey,status) VALUES(?1,?2)
             ON CONFLICT(inchikey) DO UPDATE SET status=excluded.status,
                checked_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')",
            params![inchikey, status],
        )?;
        tx.execute(
            "DELETE FROM chemical_entities WHERE inchikey=?1",
            [inchikey],
        )?;
        for entity in entities {
            tx.execute(
                "INSERT INTO chemical_entities(inchikey,wikidata_qid,name) VALUES(?1,?2,?3)",
                params![inchikey, entity.qid, entity.name],
            )?;
            for cid in &entity.pubchem_cids {
                tx.execute(
                    "INSERT INTO chemical_pubchem_ids(inchikey,wikidata_qid,pubchem_cid) VALUES(?1,?2,?3)",
                    params![inchikey, entity.qid, cid],
                )?;
            }
        }
        if let Some(generation) = generation {
            tx.execute(
                "INSERT INTO chemical_index_lookup_versions(inchikey,generation) VALUES(?1,?2)
                 ON CONFLICT(inchikey) DO UPDATE SET generation=excluded.generation",
                params![inchikey, generation],
            )?;
        }
        match status {
            "matched" => summary.matched += 1,
            "not_found" => summary.not_found += 1,
            _ => summary.ambiguous += 1,
        }
        summary.attempted += 1;
    }
    tx.commit()?;
    if verbose {
        for (inchikey, entities) in matches {
            if entities.is_empty() {
                eprintln!("{inchikey}: not_found");
            }
            for entity in entities {
                let status = if entities.len() == 1 {
                    "matched"
                } else {
                    "ambiguous"
                };
                eprintln!(
                    "{inchikey}: {status} QID={} name={} PubChem CIDs={:?}",
                    entity.wikidata_qid(),
                    entity.name().unwrap_or("-"),
                    entity.pubchem_cids()
                );
            }
        }
    }
    Ok(())
}

fn run_batch(
    client: &Client,
    endpoint: &str,
    db: &mut Connection,
    keys: &[String],
    next_request: &mut Instant,
    summary: &mut Summary,
    verbose: bool,
    progress: &ProgressBar,
) -> Result<()> {
    let mut gateway_failures = 0;
    let mut rate_limits = 0;
    loop {
        if endpoint == WDQS {
            thread::sleep(next_request.saturating_duration_since(Instant::now()));
        }
        let result = lookup_batch(client, endpoint, keys);
        *next_request = Instant::now() + Duration::from_secs(1);
        match result {
            Ok(matches) => {
                save_batch(db, &matches, summary, None, verbose)?;
                progress.inc(keys.len() as u64);
                return Ok(());
            }
            Err(error) => {
                if let Some(rate) = error.downcast_ref::<RateLimited>() {
                    rate_limits += 1;
                    if rate_limits > 3 {
                        return Err(error).context("Wikidata remained rate limited after three waits; earlier batches are saved");
                    }
                    eprintln!(
                        "Wikidata rate limited; waiting {} seconds before retrying this batch ({rate_limits}/3)",
                        rate.0.as_secs()
                    );
                    thread::sleep(rate.0);
                    continue;
                }
                if error
                    .downcast_ref::<reqwest::Error>()
                    .is_some_and(|cause| cause.status() == Some(reqwest::StatusCode::BAD_GATEWAY))
                {
                    if keys.len() > 1 {
                        let middle = keys.len() / 2;
                        eprintln!(
                            "Wikidata returned HTTP 502 for {} distinct InChIKeys; splitting into {} and {}",
                            keys.len(),
                            middle,
                            keys.len() - middle
                        );
                        run_batch(
                            client,
                            endpoint,
                            db,
                            &keys[..middle],
                            next_request,
                            summary,
                            verbose,
                            progress,
                        )?;
                        return run_batch(
                            client,
                            endpoint,
                            db,
                            &keys[middle..],
                            next_request,
                            summary,
                            verbose,
                            progress,
                        );
                    }
                    gateway_failures += 1;
                    if gateway_failures < 3 {
                        eprintln!("Wikidata returned HTTP 502 for {}; retrying", keys[0]);
                        continue;
                    }
                    return Err(error).with_context(|| format!(
                        "Wikidata returned HTTP 502 three times for {}; earlier batches are saved",
                        keys[0]
                    ));
                }
                return Err(error);
            }
        }
    }
}

fn enrich_at(options: &EnrichOptions, endpoint: &str) -> Result<Summary> {
    ensure!(
        (1..=100).contains(&options.batch_size),
        "--batch-size must be 1..=100"
    );
    let mut db = Connection::open_with_flags(&options.database, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .with_context(|| {
            format!(
                "open existing flock database {}",
                options.database.display()
            )
        })?;
    db.busy_timeout(Duration::from_secs(30))?;
    db.pragma_update(None, "foreign_keys", "ON")?;
    require_annotations(&db)?;
    db.execute_batch(SCHEMA)?;
    let mut summary = Summary::default();
    let shared = options.index.as_ref().map(|path| {
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("open shared Wikidata chemical index {} read-only", path.display()))
    }).transpose()?;
    let generation = index::generation(shared.as_ref().unwrap_or(&db))?;
    ensure!(options.index.is_none() || generation.is_some(),
        "shared Wikidata chemical index has no complete snapshot: {}", options.index.as_ref().unwrap().display());
    let keys = pending_keys(&db, options, &mut summary, generation)?;
    if keys.is_empty() {
        return Ok(summary);
    }
    if let Some(generation) = generation {
        for batch in keys.chunks(options.batch_size) {
            let matches = index::lookup(shared.as_ref().unwrap_or(&db), batch)?;
            save_batch(&mut db, &matches, &mut summary, Some(generation), options.verbose)?;
        }
        return Ok(summary);
    }
    let client = Client::builder()
        .user_agent("cunaguaro/0.1 (https://github.com/oolonek/cunaguaro; chemical metadata lookup)")
        .timeout(Duration::from_secs(60))
        .build()?;
    let progress = ProgressBar::new(keys.len() as u64);
    progress.set_style(ProgressStyle::with_template(
        "{spinner:.cyan} [{elapsed_precise}] [{bar:32.cyan/blue}] {pos}/{len} InChIKeys",
    )?);
    let mut next_request = Instant::now();
    for batch in keys.chunks(options.batch_size) {
        run_batch(
            &client,
            endpoint,
            &mut db,
            batch,
            &mut next_request,
            &mut summary,
            options.verbose,
            &progress,
        )?;
    }
    progress.finish_and_clear();
    Ok(summary)
}
