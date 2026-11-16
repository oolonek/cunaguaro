use std::path::Path;

use rusqlite::{Connection, params};
use tempfile::tempdir;

use super::enrich;

fn snapshot(path: &Path) -> Connection {
    let db = Connection::open(path).unwrap();
    db.execute_batch(
        "CREATE TABLE wikidata_taxon_snapshot(
            id INTEGER PRIMARY KEY, generation INTEGER NOT NULL,
            fetched_at TEXT NOT NULL, endpoint TEXT NOT NULL, names INTEGER NOT NULL
         );
         INSERT INTO wikidata_taxon_snapshot VALUES(1,1,'2026-01-01','test',5);
         CREATE TABLE wikidata_taxon_names(
            name TEXT NOT NULL, qid TEXT NOT NULL, PRIMARY KEY(name,qid)
         ) WITHOUT ROWID;
         CREATE TABLE wikidata_taxon_ids(
            qid TEXT NOT NULL, property TEXT NOT NULL, identifier TEXT NOT NULL,
            PRIMARY KEY(qid,property,identifier)
         ) WITHOUT ROWID;
         INSERT INTO wikidata_taxon_names VALUES
            ('Alpha species','Q1'),('Ambiguous species','Q2'),('Ambiguous species','Q3'),
            ('Conflicting species','Q4'),('Datura stramonium','Q30959');
         INSERT INTO wikidata_taxon_ids VALUES
            ('Q30959','P10585','34B7C'),('Q30959','P846','2928751'),
            ('Q30959','P14607','NEW-Gbif'),('Q30959','P3151','359213'),('Q30959','P3151','55854'),
            ('Q4','P10585','A'),('Q4','P10585','B');"
    ).unwrap();
    db
}

fn flock(path: &Path) -> Connection {
    let db = Connection::open(path).unwrap();
    db.execute_batch(
        "PRAGMA foreign_keys=ON;
         CREATE TABLE taxa(id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
         INSERT INTO taxa VALUES
            (1,'Datura stramonium'),(2,'Ambiguous species'),
            (3,'Missing species'),(4,'Conflicting species'),(5,'Alpha species');"
    ).unwrap();
    db
}

#[test]
fn exact_taxon_resolution_preserves_every_identifier_and_records_ambiguity() {
    let folder = tempdir().unwrap();
    let index = folder.path().join("taxa.sqlite");
    let job = folder.path().join("job.sqlite");
    let _snapshot = snapshot(&index);
    let db = flock(&job);

    let results = enrich(&job, &index, false, false).unwrap();
    assert_eq!(results.len(), 5);
    assert_eq!(results[0].qid.as_deref(), Some("Q30959"));
    assert_eq!(results[0].catalogue_of_life_id.as_deref(), Some("34B7C"));
    assert!(results[1].error.as_deref().unwrap().contains("Q2, Q3"));
    assert!(results[2].error.as_deref().unwrap().contains("no unambiguous"));
    assert!(results[3].error.as_deref().unwrap().contains("multiple Catalogue of Life IDs"));
    assert_eq!(results[4].qid.as_deref(), Some("Q1"));
    let identifiers = db.prepare(
        "SELECT property,identifier FROM taxon_resolution_ids WHERE taxon_id=1 ORDER BY property,identifier"
    ).unwrap().query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    assert_eq!(identifiers, [
        ("P10585".into(), "34B7C".into()),
        ("P14607".into(), "NEW-Gbif".into()),
        ("P3151".into(), "359213".into()),
        ("P3151".into(), "55854".into()),
        ("P846".into(), "2928751".into()),
    ]);
    let repeated = enrich(&job, &index, false, false).unwrap();
    assert!(repeated[0].reused);
    assert!(!repeated[1].reused); // errors are retried, not cached as successes
}

#[test]
fn new_snapshot_generation_reconciles_old_successes_and_prunes_stale_ids() {
    let folder = tempdir().unwrap();
    let index = folder.path().join("taxa.sqlite");
    let job = folder.path().join("job.sqlite");
    let snapshot = snapshot(&index);
    let db = flock(&job);
    enrich(&job, &index, false, false).unwrap();
    snapshot.execute("UPDATE wikidata_taxon_snapshot SET generation=2 WHERE id=1", []).unwrap();
    snapshot.execute("DELETE FROM wikidata_taxon_names WHERE name='Datura stramonium'", []).unwrap();
    snapshot.execute("INSERT INTO wikidata_taxon_names VALUES('Datura stramonium','Q9')", []).unwrap();
    snapshot.execute("INSERT INTO wikidata_taxon_ids VALUES('Q9','P3151','777')", []).unwrap();
    let current = enrich(&job, &index, false, false).unwrap();
    assert_eq!(current[0].qid.as_deref(), Some("Q9"));
    assert!(!current[0].reused);
    assert_eq!(current[0].catalogue_of_life_id, None);
    let ids: Vec<String> = db.prepare("SELECT identifier FROM taxon_resolution_ids WHERE taxon_id=1 ORDER BY identifier")
        .unwrap().query_map([], |row| row.get(0)).unwrap()
        .collect::<rusqlite::Result<_>>().unwrap();
    assert_eq!(ids, ["777"]);
    assert!(enrich(&job, &index, false, false).unwrap()[0].reused);
    snapshot.execute("DELETE FROM wikidata_taxon_names WHERE name='Datura stramonium'", []).unwrap();
    let refreshed = enrich(&job, &index, false, true).unwrap();
    assert!(refreshed[0].error.is_some());
    let resolution: (Option<String>, Option<String>) = db.query_row(
        "SELECT qid, catalogue_of_life_id FROM taxon_resolutions WHERE taxon_id=?1",
        params![1], |row| Ok((row.get(0)?,row.get(1)?))
    ).unwrap();
    assert_eq!(resolution, (None,None));
}

#[test]
fn missing_snapshot_fails_before_creating_a_resolution() {
    let folder = tempdir().unwrap();
    let index = folder.path().join("taxa.sqlite");
    let job = folder.path().join("job.sqlite");
    let db = flock(&job);
    let error = enrich(&job, &index, false, false).unwrap_err();
    assert!(error.to_string().contains("open taxon index"));
    assert!(!index.exists());
    let exists: i64 = db.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE name='taxon_resolutions'", [], |row| row.get(0)
    ).unwrap();
    assert_eq!(exists, 0);
}
