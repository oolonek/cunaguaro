//! Resolve Peregrino's distinct taxon names against a shared Wikidata snapshot.

pub mod chemical;
pub mod diagrams;
pub mod proposal;
pub mod taxon_index;

#[cfg(test)]
mod tests;

use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, params};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS taxon_resolutions (
    taxon_id INTEGER PRIMARY KEY REFERENCES taxa(id) ON DELETE CASCADE,
    qid TEXT,
    catalogue_of_life_id TEXT,
    error TEXT,
    CHECK ((qid IS NOT NULL AND error IS NULL) OR
           (qid IS NULL AND catalogue_of_life_id IS NULL AND error IS NOT NULL))
);
CREATE TABLE IF NOT EXISTS taxon_resolution_ids (
    taxon_id INTEGER NOT NULL REFERENCES taxa(id) ON DELETE CASCADE,
    property TEXT NOT NULL,
    identifier TEXT NOT NULL,
    PRIMARY KEY(taxon_id, property, identifier)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS taxon_resolution_versions (
    taxon_id INTEGER PRIMARY KEY REFERENCES taxa(id) ON DELETE CASCADE,
    generation INTEGER NOT NULL
);
"#;

/// Current reconciliation outcome for the distinct name shared by its samples.
#[derive(Debug, PartialEq, Eq)]
pub struct Outcome {
    pub taxon_id: i64,
    pub name: String,
    pub qid: Option<String>,
    pub catalogue_of_life_id: Option<String>,
    pub error: Option<String>,
    /// Whether the stored result was already reconciled against this snapshot generation.
    pub reused: bool,
}

struct Match {
    qid: String,
    catalogue_of_life_id: Option<String>,
    identifiers: Vec<(String, String)>,
}

fn resolve(index: &Connection, name: &str) -> Result<Match> {
    let mut qids = index.prepare_cached("SELECT qid FROM wikidata_taxon_names WHERE name=?1 ORDER BY qid")?;
    let found = qids.query_map([name], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let qid = match found.as_slice() {
        [] => bail!("{name}: no unambiguous Wikidata taxon with an exact P225 name"),
        [qid] => qid,
        _ => bail!("{name}: ambiguous Wikidata items: {}", found.join(", ")),
    };
    let mut query = index.prepare_cached(
        "SELECT property, identifier FROM wikidata_taxon_ids WHERE qid=?1 ORDER BY property, identifier"
    )?;
    let identifiers = query.query_map([qid], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut col = identifiers.iter().filter(|(property, _)| property == "P10585").map(|(_, id)| id);
    let catalogue_of_life_id = col.next().cloned();
    if col.next().is_some() {
        bail!("{name}: multiple Catalogue of Life IDs for {qid}");
    }
    Ok(Match { qid: qid.clone(), catalogue_of_life_id, identifiers })
}

/// Match exact P225 names against a complete shared read-only QLever snapshot.
/// Successful results are reused only for the same snapshot generation; errors retry.
/// All matched P10585/P846/P14607/P3151 identifiers are retained with their property.
pub fn enrich(database: &Path, index_path: &Path, verbose: bool, refresh: bool) -> Result<Vec<Outcome>> {
    let index = Connection::open_with_flags(index_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open taxon index {} (build with cunaguaro taxon-index --index {})", index_path.display(), index_path.display()))?;
    index.execute_batch("BEGIN")?;
    let generation = taxon_index::generation(&index)?
        .with_context(|| format!("taxon index {} has no complete snapshot; run cunaguaro taxon-index --index {}", index_path.display(), index_path.display()))?;
    let mut db = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .with_context(|| format!("open existing flock database {}", database.display()))?;
    db.pragma_update(None, "foreign_keys", "ON")?;
    db.execute_batch(SCHEMA)?;
    let taxa = {
        let mut query = db.prepare(
            "SELECT t.id, t.name, r.qid, r.catalogue_of_life_id, v.generation \
             FROM taxa t LEFT JOIN taxon_resolutions r ON r.taxon_id=t.id \
             LEFT JOIN taxon_resolution_versions v ON v.taxon_id=t.id ORDER BY t.id"
        ).context("read taxa (run peregrino ingest to migrate an older database)")?;
        query.query_map([], |row| Ok((
            row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?, row.get::<_, Option<i64>>(4)?
        )))?.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut outcomes = Vec::with_capacity(taxa.len());
    let tx = db.transaction()?;
    for (taxon_id, name, old_qid, old_col, previous_generation) in taxa {
        if !refresh && previous_generation == Some(generation) && let Some(qid) = old_qid {
            outcomes.push(Outcome { taxon_id, name, qid: Some(qid), catalogue_of_life_id: old_col,
                error: None, reused: true });
            continue;
        }
        let (qid, catalogue_of_life_id, identifiers, error) = match resolve(&index, &name) {
            Ok(found) => (Some(found.qid), found.catalogue_of_life_id, found.identifiers, None),
            Err(error) => (None, None, Vec::new(), Some(format!("{error:#}"))),
        };
        tx.execute(
            "INSERT INTO taxon_resolutions(taxon_id,qid,catalogue_of_life_id,error) VALUES(?1,?2,?3,?4)
             ON CONFLICT(taxon_id) DO UPDATE SET qid=excluded.qid,
                 catalogue_of_life_id=excluded.catalogue_of_life_id,error=excluded.error",
            params![taxon_id, qid, catalogue_of_life_id, error]
        )?;
        tx.execute("DELETE FROM taxon_resolution_ids WHERE taxon_id=?1", [taxon_id])?;
        for (property, identifier) in identifiers {
            tx.execute("INSERT INTO taxon_resolution_ids(taxon_id,property,identifier) VALUES(?1,?2,?3)",
                params![taxon_id, property, identifier])?;
        }
        tx.execute(
            "INSERT INTO taxon_resolution_versions(taxon_id,generation) VALUES(?1,?2)
             ON CONFLICT(taxon_id) DO UPDATE SET generation=excluded.generation",
            params![taxon_id, generation]
        )?;
        if verbose {
            if let Some(error) = &error {
                eprintln!("{name} ({taxon_id}): {error}");
            } else {
                eprintln!("{name} ({taxon_id}): Wikidata {} | Catalogue of Life {}",
                    qid.as_deref().unwrap_or("-"), catalogue_of_life_id.as_deref().unwrap_or("not listed"));
            }
        }
        outcomes.push(Outcome { taxon_id, name, qid, catalogue_of_life_id, error, reused: false });
    }
    tx.commit()?;
    Ok(outcomes)
}
