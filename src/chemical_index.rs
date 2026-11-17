//! A complete, replaceable local mirror of QLever Wikidata's truthy P235 index.

use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use reqwest::blocking::Client;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde_json::Value;

use super::{WikidataChemical, parse_qid};
use crate::qlever::request;

pub const QLEVER: &str = "https://qlever.dev/api/wikidata";
const COUNT: &str = "PREFIX wdt: <http://www.wikidata.org/prop/direct/> SELECT (COUNT(*) AS ?count) WHERE { ?item wdt:P235 ?key }";
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS wikidata_chemical_index (
    inchikey TEXT NOT NULL,
    wikidata_qid TEXT NOT NULL,
    name TEXT,
    PRIMARY KEY(inchikey,wikidata_qid)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS wikidata_chemical_cids (
    inchikey TEXT NOT NULL,
    wikidata_qid TEXT NOT NULL,
    pubchem_cid TEXT NOT NULL,
    PRIMARY KEY(inchikey,wikidata_qid,pubchem_cid),
    FOREIGN KEY(inchikey,wikidata_qid) REFERENCES wikidata_chemical_index(inchikey,wikidata_qid)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS wikidata_chemical_snapshot (
    id INTEGER PRIMARY KEY CHECK(id=1),
    generation INTEGER NOT NULL,
    fetched_at TEXT NOT NULL,
    endpoint TEXT NOT NULL,
    pairs INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS chemical_index_lookup_versions (
    inchikey TEXT PRIMARY KEY,
    generation INTEGER NOT NULL
);
"#;

/// A complete QLever index refresh; pairs count unique (full InChIKey, QID) associations.
#[derive(Debug)]
pub struct Snapshot {
    pub pairs: u64,
    pub generation: i64,
    pub fetched_at: String,
}

pub(super) fn generation(db: &Connection) -> Result<Option<i64>> {
    let exists: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='wikidata_chemical_snapshot' AND type='table')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(None);
    }
    Ok(db
        .query_row(
            "SELECT generation FROM wikidata_chemical_snapshot WHERE id=1",
            [],
            |row| row.get(0),
        )
        .optional()?)
}

fn writable_index(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create index directory {}", parent.display()))?;
    }
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("open chemical index {}", path.display()))?;
    db.busy_timeout(Duration::from_secs(30))?;
    db.pragma_update(None, "foreign_keys", "ON")?;
    Ok(db)
}

pub(super) fn refresh_at(database: &Path, endpoint: &str) -> Result<Snapshot> {
    refresh_with_page_size(database, endpoint, 200_000)
}

pub(super) fn refresh_with_page_size(database: &Path, endpoint: &str, page_size: u64) -> Result<Snapshot> {
    ensure!(page_size > 0, "QLever chemical page size must be positive");
    let mut db = writable_index(database)?;
    let client = Client::builder()
        .user_agent(
            "cunaguaro/0.1 (https://github.com/oolonek/cunaguaro; Wikidata chemical snapshot)",
        )
        .timeout(Duration::from_secs(1800))
        .build()?;
    let response: Value = request(&client, endpoint, COUNT, "application/sparql-results+json", None)?
        .json()
        .context("decoding QLever P235 count")?;
    let expected: u64 = response["results"]["bindings"][0]["count"]["value"]
        .as_str()
        .context("QLever count is missing")?
        .parse()
        .context("invalid QLever P235 count")?;
    ensure!(
        expected > 0,
        "QLever returned an empty P235 index; refusing to replace snapshot"
    );
    // QLever may end a successful multi-million-row CSV before all pairs
    // have arrived. Bound each request, and re-read the boundary item so
    // multiple keys/CIDs on that item cannot be split or silently lost.
    // The independent count below still rejects incomplete or changing exports.
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(SCHEMA)?;
    let next = generation(&tx)?.unwrap_or(0) + 1;
    tx.execute("DELETE FROM wikidata_chemical_cids", [])?;
    tx.execute("DELETE FROM wikidata_chemical_index", [])?;
    {
        let mut chemical = tx.prepare(
            "INSERT INTO wikidata_chemical_index(inchikey,wikidata_qid,name) VALUES(?1,?2,?3)
             ON CONFLICT(inchikey,wikidata_qid) DO UPDATE SET name=COALESCE(wikidata_chemical_index.name,excluded.name)",
        )?;
        let mut cid = tx.prepare(
            "INSERT OR IGNORE INTO wikidata_chemical_cids(inchikey,wikidata_qid,pubchem_cid) VALUES(?1,?2,?3)",
        )?;
        let mut cursor: Option<String> = None;
        let send = page_size.to_string();
        loop {
            let after = match &cursor {
                Some(last) => format!("FILTER(STR(?item) >= {})", serde_json::to_string(last)?),
                None => String::new(),
            };
            let query = format!(
                "PREFIX wdt: <http://www.wikidata.org/prop/direct/> \
                 PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#> \
                 SELECT ?item ?key ?cid ?name WHERE {{ ?item wdt:P235 ?key . \
                 FILTER(STRSTARTS(STR(?item), \"http://www.wikidata.org/entity/Q\")) \
                 OPTIONAL {{ ?item wdt:P662 ?cid }} \
                 OPTIONAL {{ ?item rdfs:label ?name . FILTER(LANG(?name) = \"en\") }} \
                 {after} }} ORDER BY ?item ?key ?cid ?name LIMIT {page_size}"
            );
            let response = request(&client, endpoint, &query, "text/csv", Some(&send))
                .context("streaming QLever P235 export page")?;
            let mut csv = csv::Reader::from_reader(response);
            ensure!(
                csv.headers()?.iter().eq(["item", "key", "cid", "name"]),
                "unexpected QLever CSV columns"
            );
            let mut row = csv::StringRecord::new();
            let mut received = 0_u64;
            let mut last_item = String::new();
            while csv.read_record(&mut row)? {
                received += 1;
                ensure!(received <= page_size, "QLever P235 page exceeded LIMIT {page_size}");
                let item = row.get(0).context("QLever row has no item")?;
                let qid = parse_qid(item)?;
                let key = row.get(1).context("QLever row has no InChIKey")?;
                ensure!(!key.is_empty(), "QLever row has an empty P235 InChIKey for {qid}");
                let name = row.get(3).filter(|value| !value.is_empty());
                chemical.execute(params![key, qid, name])?;
                if let Some(value) = row.get(2).filter(|value| !value.is_empty()) {
                    cid.execute(params![key, qid, value])?;
                }
                last_item.clear();
                last_item.push_str(item);
            }
            if received < page_size {
                break;
            }
            ensure!(
                cursor.as_deref() != Some(last_item.as_str()),
                "QLever P235 has at least {page_size} rows on one item; previous snapshot retained"
            );
            cursor = Some(last_item);
        }
    }
    let pairs: u64 = tx.query_row("SELECT COUNT(*) FROM wikidata_chemical_index", [], |row| {
        row.get(0)
    })?;
    ensure!(
        pairs == expected,
        "QLever P235 export incomplete: expected {expected} pairs but received {pairs}; previous snapshot retained"
    );
    tx.execute(
        "INSERT INTO wikidata_chemical_snapshot(id,generation,fetched_at,endpoint,pairs)
         VALUES(1,?1,strftime('%Y-%m-%dT%H:%M:%fZ','now'),?2,?3)
         ON CONFLICT(id) DO UPDATE SET generation=excluded.generation,fetched_at=excluded.fetched_at,
            endpoint=excluded.endpoint,pairs=excluded.pairs",
        params![next, endpoint, pairs],
    )?;
    let fetched_at: String = tx.query_row(
        "SELECT fetched_at FROM wikidata_chemical_snapshot WHERE id=1",
        [],
        |row| row.get(0),
    )?;
    tx.commit()?;
    Ok(Snapshot {
        pairs,
        generation: next,
        fetched_at,
    })
}

/// Copy only the completed Wikidata cache, never annotation data, into a shared SQLite file.
pub fn copy_snapshot(source: &Path, destination: &Path) -> Result<Snapshot> {
    let source_path = source
        .canonicalize()
        .with_context(|| format!("open source index {}", source.display()))?;
    ensure!(
        source_path != destination.canonicalize().unwrap_or_default(),
        "source and destination chemical indexes must differ"
    );
    let old = Connection::open_with_flags(&source_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let expected: u64 = old
        .query_row(
            "SELECT pairs FROM wikidata_chemical_snapshot WHERE id=1",
            [],
            |row| row.get(0),
        )
        .context("source database has no complete Wikidata chemical snapshot")?;
    ensure!(expected > 0, "source Wikidata chemical snapshot is empty");
    drop(old);
    let mut db = writable_index(destination)?;
    let uri = format!(
        "{}?mode=ro",
        reqwest::Url::from_file_path(&source_path)
            .map_err(|_| anyhow::anyhow!("source index path cannot be encoded as file URL"))?
    );
    db.execute("ATTACH DATABASE ?1 AS source", [uri.as_str()])?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(SCHEMA)?;
    let next = generation(&tx)?.unwrap_or(0) + 1;
    tx.execute("DELETE FROM wikidata_chemical_cids", [])?;
    tx.execute("DELETE FROM wikidata_chemical_index", [])?;
    tx.execute(
        "INSERT INTO wikidata_chemical_index(inchikey,wikidata_qid,name)
         SELECT inchikey,wikidata_qid,name FROM source.wikidata_chemical_index",
        [],
    )?;
    tx.execute(
        "INSERT INTO wikidata_chemical_cids(inchikey,wikidata_qid,pubchem_cid)
         SELECT inchikey,wikidata_qid,pubchem_cid FROM source.wikidata_chemical_cids",
        [],
    )?;
    let (pairs, fetched_at, endpoint): (u64, String, String) = tx.query_row(
        "SELECT (SELECT COUNT(*) FROM wikidata_chemical_index),fetched_at,endpoint
         FROM source.wikidata_chemical_snapshot WHERE id=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    ensure!(
        pairs == expected,
        "source Wikidata chemical index changed or is incomplete during copy; previous shared snapshot retained"
    );
    tx.execute(
        "INSERT INTO wikidata_chemical_snapshot(id,generation,fetched_at,endpoint,pairs)
         VALUES(1,?1,?2,?3,?4)
         ON CONFLICT(id) DO UPDATE SET generation=excluded.generation,fetched_at=excluded.fetched_at,
            endpoint=excluded.endpoint,pairs=excluded.pairs",
        params![next, fetched_at, endpoint, pairs],
    )?;
    tx.commit()?;
    Ok(Snapshot {
        pairs,
        generation: next,
        fetched_at,
    })
}

pub(super) fn lookup(
    db: &Connection,
    keys: &[String],
) -> Result<Vec<(String, Vec<WikidataChemical>)>> {
    let mut query = db.prepare(
        "SELECT i.wikidata_qid,i.name,c.pubchem_cid FROM wikidata_chemical_index i
         LEFT JOIN wikidata_chemical_cids c USING(inchikey,wikidata_qid)
         WHERE i.inchikey=?1 ORDER BY i.wikidata_qid,c.pubchem_cid",
    )?;
    let mut result = Vec::with_capacity(keys.len());
    for key in keys {
        let mut matches: Vec<WikidataChemical> = Vec::new();
        let mut rows = query.query([key])?;
        while let Some(row) = rows.next()? {
            let qid: &str = row.get_ref(0)?.as_str()?;
            if matches.last().is_none_or(|entity| entity.qid != qid) {
                matches.push(WikidataChemical {
                    inchikey: key.clone(),
                    qid: qid.to_owned(),
                    name: row.get(1)?,
                    pubchem_cids: Vec::new(),
                });
            }
            if let Some(cid) = row.get::<_, Option<String>>(2)? {
                matches
                    .last_mut()
                    .expect("chemical was appended")
                    .pubchem_cids
                    .push(cid);
            }
        }
        result.push((key.clone(), matches));
    }
    Ok(result)
}
