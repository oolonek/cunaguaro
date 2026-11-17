use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use serde_json::{Value, json};

use super::{ChemicalEntity, ChemicalStructure, EnrichOptions, enrich_at};

const A: &str = "AOPJIQXDBLPZDD-UHFFFAOYSA-N";
const B: &str = "CSLHHTJTXNNRSR-UHFFFAOYSA-N";
const C: &str = "JBFOLLJCGUCDQP-UHFFFAOYSA-N";
const STALE: &str = "TVFJNQQJLLVFGO-UHFFFAOYSA-N";

fn fixture() -> tempfile::NamedTempFile {
    let file = tempfile::NamedTempFile::new().unwrap();
    let db = Connection::open(file.path()).unwrap();
    db.execute_batch(&format!(
        "CREATE TABLE processed_spectra(dataset_id INTEGER,filename TEXT,ms2_sha256 TEXT);
         CREATE TABLE annotation_runs(id INTEGER,dataset_id INTEGER,filename TEXT,ms2_sha256 TEXT,status TEXT);
         CREATE TABLE annotation_hits(run_id INTEGER,feature_id TEXT,rank INTEGER,library_inchikey TEXT);
         CREATE VIEW annotation_current_runs AS
           SELECT r.id,r.status FROM annotation_runs r
           JOIN processed_spectra p USING(dataset_id,filename)
           WHERE r.ms2_sha256 = p.ms2_sha256;
         INSERT INTO processed_spectra VALUES(1,'one.mzML','current'),(1,'two.mzML','current');
         INSERT INTO annotation_runs VALUES
           (1,1,'one.mzML','old','hits'),(2,1,'one.mzML','current','hits'),
           (3,1,'two.mzML','current','hits');
         INSERT INTO annotation_hits VALUES
           (1,'old',1,'{STALE}'),
           (2,'f1',1,'{A}'),(2,'f2',1,'{A}'),
           (2,'f3',1,'{B}'),(2,'f4',1,'{C}'),(2,'f5',2,NULL),
           (3,'another-file',1,'{A}');"
    ))
    .unwrap();
    file
}

fn options(db: &Path) -> EnrichOptions {
    EnrichOptions {
        database: db.to_path_buf(),
        index: None,
        batch_size: 3,
        limit: None,
        refresh: false,
        verbose: false,
    }
}

fn sparql_row(key: &str, qid: &str, name: Option<&str>, cid: Option<&str>) -> Value {
    let mut row = json!({
        "inchikey": {"type": "literal", "value": key},
        "item": {"type": "uri", "value": format!("http://www.wikidata.org/entity/{qid}")}
    });
    if let Some(name) = name {
        row["name"] = json!({"type": "literal", "value": name});
    }
    if let Some(cid) = cid {
        row["cid"] = json!({"type": "literal", "value": cid});
    }
    row
}

fn answer(rows: Vec<Value>) -> String {
    json!({"results": {"bindings": rows}}).to_string()
}

fn server(replies: Vec<(u16, String)>) -> (String, thread::JoinHandle<Vec<String>>) {
    server_with_retry_after(
        replies
            .into_iter()
            .map(|(status, body)| (status, body, None))
            .collect(),
    )
}

fn server_with_retry_after(
    replies: Vec<(u16, String, Option<&'static str>)>,
) -> (String, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/sparql", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        for (status, body, retry_after) in replies {
            let deadline = Instant::now() + Duration::from_secs(10);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(pair) => break pair,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("waiting for Cunaguaro request: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            requests.push(line);
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header == "\r\n" || header.is_empty() {
                    break;
                }
            }
            let reason = match status {
                200 => "OK",
                429 => "Too Many Requests",
                502 => "Bad Gateway",
                _ => "Unexpected",
            };
            write!(
                stream,
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/sparql-results+json\r\nContent-Length: {}\r\n",
                body.len()
            )
            .unwrap();
            if let Some(seconds) = retry_after {
                write!(stream, "Retry-After: {seconds}\r\n").unwrap();
            }
            write!(stream, "Connection: close\r\n\r\n{body}").unwrap();
            stream.flush().unwrap();
        }
        requests
    });
    (url, handle)
}

#[test]
fn matches_full_keys_and_preserves_ambiguous_items_and_multiple_pubchem_ids() {
    let file = fixture();
    let (url, worker) = server(vec![(
        200,
        answer(vec![
            sparql_row(B, "Q20", Some("compound beta"), Some("101")),
            sparql_row(B, "Q20", Some("compound beta"), Some("102")),
            sparql_row(C, "Q30", Some("compound gamma"), None),
            sparql_row(C, "Q31", None, Some("303")),
        ]),
    )]);
    let result = enrich_at(&options(file.path()), &url).unwrap();
    let requests = worker.join().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains("query="));
    assert!(requests[0].contains("P235"));
    assert!(requests[0].contains("P662"));
    assert_eq!(requests[0].matches(A).count(), 1);
    assert_eq!(result.distinct_keys, 3);
    assert_eq!(
        (result.matched, result.not_found, result.ambiguous),
        (1, 1, 1)
    );
    let db = Connection::open(file.path()).unwrap();
    let status: String = db
        .query_row(
            "SELECT status FROM chemical_lookups WHERE inchikey=?1",
            [C],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(status, "ambiguous");
    let (qid, name): (String, Option<String>) = db
        .query_row(
            "SELECT wikidata_qid,name FROM chemical_entities WHERE inchikey=?1 AND wikidata_qid='Q31'",
            [C],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((qid.as_str(), name), ("Q31", None));
    let cids = db
        .prepare(
            "SELECT pubchem_cid FROM chemical_pubchem_ids WHERE inchikey=?1 ORDER BY pubchem_cid",
        )
        .unwrap()
        .query_map([B], |r| r.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect::<Vec<_>>();
    assert_eq!(cids, ["101", "102"]);
    let no_match: String = db
        .query_row(
            "SELECT status FROM chemical_lookups WHERE inchikey=?1",
            [A],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(no_match, "not_found");
    let old: i64 = db
        .query_row(
            "SELECT count(*) FROM chemical_lookups WHERE inchikey=?1",
            [STALE],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(old, 0);
    let resumed = enrich_at(&options(file.path()), "http://127.0.0.1:1/sparql").unwrap();
    assert_eq!((resumed.reused, resumed.attempted), (3, 0));
}

#[test]
fn gateway_failure_splits_distinct_keys_and_commits_each_successful_piece() {
    let file = fixture();
    let (url, worker) = server(vec![
        (502, "Bad Gateway".into()),
        (200, answer(vec![sparql_row(A, "Q10", Some("alpha"), None)])),
        (
            200,
            answer(vec![sparql_row(B, "Q20", Some("beta"), Some("20"))]),
        ),
    ]);
    let result = enrich_at(&options(file.path()), &url).unwrap();
    let requests = worker.join().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].matches(A).count(), 1);
    assert_eq!(requests[1].matches(A).count(), 1);
    assert!(!requests[1].contains(B));
    assert!(!requests[2].contains(A));
    assert_eq!(
        (
            result.distinct_keys,
            result.attempted,
            result.matched,
            result.not_found
        ),
        (3, 3, 2, 1)
    );
    let resumed = enrich_at(&options(file.path()), "http://127.0.0.1:1/sparql").unwrap();
    assert_eq!((resumed.reused, resumed.attempted), (3, 0));
}

#[test]
fn persistent_single_key_gateway_failure_keeps_prior_piece_and_resumes() {
    let file = fixture();
    let (url, worker) = server(vec![
        (502, "Bad Gateway".into()),
        (200, answer(vec![sparql_row(A, "Q10", None, None)])),
        (502, "Bad Gateway".into()),
        (502, "Bad Gateway".into()),
        (502, "Bad Gateway".into()),
        (502, "Bad Gateway".into()),
    ]);
    let error = enrich_at(&options(file.path()), &url).unwrap_err();
    assert!(error.to_string().contains("three times"));
    assert!(error.to_string().contains(B));
    let requests = worker.join().unwrap();
    assert_eq!(requests.len(), 6);
    assert_eq!(requests[0].matches(A).count(), 1);
    let db = Connection::open(file.path()).unwrap();
    let stored: Vec<String> = db
        .prepare("SELECT inchikey FROM chemical_lookups ORDER BY inchikey")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(stored, [A]);
    drop(db);
    let (url, worker) = server(vec![(200, answer(vec![sparql_row(B, "Q20", None, None)]))]);
    let result = enrich_at(&options(file.path()), &url).unwrap();
    assert_eq!(worker.join().unwrap().len(), 1);
    assert_eq!((result.reused, result.attempted), (1, 2));
}

#[test]
fn rate_limit_waits_for_retry_after_and_continues_without_repeating_prior_keys() {
    let file = fixture();
    let mut opts = options(file.path());
    opts.batch_size = 1;
    let (url, worker) = server_with_retry_after(vec![
        (200, answer(vec![]), None),
        (429, "rate limited".into(), Some("1")),
        (200, answer(vec![sparql_row(B, "Q20", None, None)]), None),
        (200, answer(vec![]), None),
    ]);
    let started = Instant::now();
    let result = enrich_at(&opts, &url).unwrap();
    assert!(started.elapsed() >= Duration::from_secs(1));
    let requests = worker.join().unwrap();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[0].matches(A).count(), 1);
    assert_eq!(requests[1].matches(B).count(), 1);
    assert_eq!(requests[2].matches(B).count(), 1);
    assert!(!requests[2].contains(A));
    assert_eq!((result.reused, result.attempted, result.matched), (0, 3, 1));
    let resumed = enrich_at(&opts, "http://127.0.0.1:1/sparql").unwrap();
    assert_eq!((resumed.reused, resumed.attempted), (3, 0));
}

#[test]
fn repeated_rate_limits_stop_after_three_waits_without_caching_a_failed_key() {
    let file = fixture();
    let mut opts = options(file.path());
    opts.limit = Some(1);
    let (url, worker) = server_with_retry_after(vec![
        (429, "rate limited".into(), Some("0")),
        (429, "rate limited".into(), Some("0")),
        (429, "rate limited".into(), Some("0")),
        (429, "rate limited".into(), Some("0")),
    ]);
    let started = Instant::now();
    let error = enrich_at(&opts, &url).unwrap_err();
    assert!(error.to_string().contains("three waits"));
    assert!(started.elapsed() >= Duration::from_secs(3));
    assert_eq!(worker.join().unwrap().len(), 4);
    let db = Connection::open(file.path()).unwrap();
    let stored: i64 = db
        .query_row("SELECT count(*) FROM chemical_lookups", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(stored, 0);
    drop(db);
    let (url, worker) = server(vec![(200, answer(vec![sparql_row(A, "Q10", None, None)]))]);
    let result = enrich_at(&opts, &url).unwrap();
    assert_eq!(worker.join().unwrap().len(), 1);
    assert_eq!((result.reused, result.attempted), (0, 1));
}

#[test]
fn failure_keeps_completed_batches_and_restart_processes_only_pending_keys() {
    let file = fixture();
    let mut opts = options(file.path());
    opts.batch_size = 1;
    let (url, worker) = server(vec![(200, answer(vec![])), (429, "rate limited".into())]);
    let error = enrich_at(&opts, &url).unwrap_err();
    assert!(error.to_string().contains("rate limited"));
    assert_eq!(worker.join().unwrap().len(), 2);
    let db = Connection::open(file.path()).unwrap();
    let stored: i64 = db
        .query_row("SELECT count(*) FROM chemical_lookups", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored, 1);
    drop(db);
    let (url, worker) = server(vec![
        (
            200,
            answer(vec![sparql_row(B, "Q20", Some("beta"), Some("101"))]),
        ),
        (200, answer(vec![sparql_row(C, "Q30", Some("gamma"), None)])),
    ]);
    let result = enrich_at(&opts, &url).unwrap();
    assert_eq!(worker.join().unwrap().len(), 2);
    assert_eq!((result.reused, result.attempted, result.pending), (1, 2, 2));
}

#[test]
fn limited_rerun_advances_to_next_key_and_absent_database_is_not_created() {
    let file = fixture();
    let mut opts = options(file.path());
    opts.limit = Some(1);
    let (url, worker) = server(vec![(200, answer(vec![]))]);
    let result = enrich_at(&opts, &url).unwrap();
    assert_eq!(result.attempted, 1);
    assert_eq!(result.pending, 3);
    assert_eq!(worker.join().unwrap().len(), 1);
    let (url, worker) = server(vec![(200, answer(vec![sparql_row(B, "Q20", None, None)]))]);
    let result = enrich_at(&opts, &url).unwrap();
    assert_eq!((result.reused, result.attempted), (1, 1));
    assert_eq!(worker.join().unwrap().len(), 1);

    let missing = file.path().with_extension("missing.sqlite");
    assert!(!missing.exists());
    let error = enrich_at(&options(&missing), "http://127.0.0.1:1/sparql").unwrap_err();
    assert!(error.to_string().contains("open existing flock database"));
    assert!(!missing.exists());

    let empty = tempfile::NamedTempFile::new().unwrap();
    let error = enrich_at(&options(empty.path()), "http://127.0.0.1:1/sparql").unwrap_err();
    assert!(error.to_string().contains("annotation hits"));
    let db = Connection::open(empty.path()).unwrap();
    let created: i64 = db
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='chemical_lookups'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(created, 0);
}

#[test]
fn chemical_entity_traits_expose_exact_identity_and_optional_metadata() {
    let entity = super::WikidataChemical {
        inchikey: A.to_owned(),
        qid: "Q1".into(),
        name: None,
        pubchem_cids: vec!["1".into(), "2".into()],
    };
    assert_eq!(entity.full_inchikey(), A);
    assert_eq!(entity.wikidata_qid(), "Q1");
    assert_eq!(entity.name(), None);
    assert_eq!(entity.pubchem_cids(), &["1", "2"]);
}

#[test]
fn bulk_qlever_snapshot_resolves_current_keys_offline_without_losing_metadata_or_ambiguity() {
    let file = fixture();
    let rows = format!(
        "item,key,cid,name\n\
         http://www.wikidata.org/entity/Q10,{A},101,\"alpha, \"\"compound\"\"\"\n\
         http://www.wikidata.org/entity/Q10,{A},102,\"alpha, \"\"compound\"\"\"\n\
         http://www.wikidata.org/entity/Q30,{C},,gamma\n\
         http://www.wikidata.org/entity/Q31,{C},303,\n"
    );
    let count = json!({"results":{"bindings":[{"count":{"value":"3"}}]}}).to_string();
    let (url, worker) = server(vec![(200, count), (200, rows)]);
    let snapshot = super::refresh_at(file.path(), &url).unwrap();
    assert_eq!(snapshot.pairs, 3);
    let requests = worker.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("send="), "QLever otherwise truncates the CSV export");

    let result = enrich_at(&options(file.path()), "http://127.0.0.1:1/sparql").unwrap();
    assert_eq!((result.matched, result.not_found, result.ambiguous), (1, 1, 1));
    let db = Connection::open(file.path()).unwrap();
    let name: String = db.query_row(
        "SELECT name FROM chemical_entities WHERE inchikey=?1 AND wikidata_qid='Q10'",
        [A],
        |row| row.get(0),
    ).unwrap();
    assert_eq!(name, "alpha, \"compound\"");
    let cids: Vec<String> = db.prepare(
        "SELECT pubchem_cid FROM chemical_pubchem_ids WHERE inchikey=?1 ORDER BY pubchem_cid"
    ).unwrap().query_map([A], |row| row.get(0)).unwrap().map(Result::unwrap).collect();
    assert_eq!(cids, ["101", "102"]);
}

#[test]
fn paged_qlever_export_preserves_pairs_split_across_pages_and_optional_cids() {
    let file = fixture();
    let count = json!({"results":{"bindings":[{"count":{"value":"3"}}]}}).to_string();
    let first = format!(
        "item,key,cid,name\nhttp://www.wikidata.org/entity/Q10,{A},101,alpha\n\
         http://www.wikidata.org/entity/Q10,{A},102,alpha\n\
         http://www.wikidata.org/entity/Q30,{C},,gamma\n"
    );
    let second = format!(
        "item,key,cid,name\nhttp://www.wikidata.org/entity/Q30,{C},,gamma\n\
         http://www.wikidata.org/entity/Q31,{C},303,\n"
    );
    let (url, worker) = server(vec![(200, count), (200, first), (200, second)]);
    let snapshot = super::index::refresh_with_page_size(file.path(), &url, 3).unwrap();
    assert_eq!(snapshot.pairs, 3);
    let requests = worker.join().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[1].contains("LIMIT+3"));
    assert!(requests[2].contains("Q30") && requests[2].contains("FILTER"));
    let db = Connection::open(file.path()).unwrap();
    let cids: Vec<String> = db
        .prepare("SELECT pubchem_cid FROM wikidata_chemical_cids WHERE wikidata_qid='Q10' ORDER BY pubchem_cid")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(cids, ["101", "102"]);
}

#[test]
fn chemical_snapshot_waits_for_rate_limited_qlever_page() {
    let file = fixture();
    let count = json!({"results":{"bindings":[{"count":{"value":"1"}}]}}).to_string();
    let rows = format!("item,key,cid,name\nhttp://www.wikidata.org/entity/Q10,{A},101,alpha\n");
    let (url, worker) = server(vec![(200, count), (429, String::new()), (200, rows)]);
    assert_eq!(super::refresh_at(file.path(), &url).unwrap().pairs, 1);
    let requests = worker.join().unwrap();
    assert_eq!(requests.len(), 3);
}

#[test]
fn incomplete_refresh_retains_prior_index_and_successful_refresh_rechecks_cached_keys() {
    let file = fixture();
    let count = |pairs: usize| json!({"results":{"bindings":[{"count":{"value":pairs.to_string()}}]}}).to_string();
    let rows = |qid| format!(
        "item,key,cid,name\nhttp://www.wikidata.org/entity/{qid},{A},101,alpha\n\
         http://www.wikidata.org/entity/Q30,{C},,gamma\n\
         http://www.wikidata.org/entity/Q31,{C},,\n"
    );
    let (url, worker) = server(vec![(200, count(3)), (200, rows("Q10"))]);
    let first = super::refresh_at(file.path(), &url).unwrap();
    worker.join().unwrap();
    enrich_at(&options(file.path()), "http://127.0.0.1:1/sparql").unwrap();

    let (url, worker) = server(vec![(200, count(4)), (200, rows("Q99"))]);
    let error = super::refresh_at(file.path(), &url).unwrap_err();
    worker.join().unwrap();
    assert!(error.to_string().contains("previous snapshot retained"), "{error:#}");
    let db = Connection::open(file.path()).unwrap();
    let current: (i64, i64) = db.query_row(
        "SELECT generation,pairs FROM wikidata_chemical_snapshot WHERE id=1", [], |row| Ok((row.get(0)?, row.get(1)?))
    ).unwrap();
    assert_eq!(current, (first.generation, 3));
    let qid: String = db.query_row(
        "SELECT wikidata_qid FROM wikidata_chemical_index WHERE inchikey=?1", [A], |row| row.get(0)
    ).unwrap();
    assert_eq!(qid, "Q10");
    drop(db);

    let (url, worker) = server(vec![(200, count(3)), (200, rows("Q99"))]);
    let refreshed = super::refresh_at(file.path(), &url).unwrap();
    worker.join().unwrap();
    assert_eq!(refreshed.generation, first.generation + 1);
    let result = enrich_at(&options(file.path()), "http://127.0.0.1:1/sparql").unwrap();
    assert_eq!((result.reused, result.attempted), (0, 3));
    let db = Connection::open(file.path()).unwrap();
    let qid: String = db.query_row(
        "SELECT wikidata_qid FROM chemical_entities WHERE inchikey=?1", [A], |row| row.get(0)
    ).unwrap();
    assert_eq!(qid, "Q99");
}

#[test]
fn one_read_only_wikidata_index_serves_two_independent_annotation_databases() {
    let source = fixture();
    let first = fixture();
    let second = fixture();
    let rows = format!(
        "item,key,cid,name\nhttp://www.wikidata.org/entity/Q10,{A},101,alpha\n\
         http://www.wikidata.org/entity/Q30,{C},,gamma\n\
         http://www.wikidata.org/entity/Q31,{C},,\n"
    );
    let count = json!({"results":{"bindings":[{"count":{"value":"3"}}]}}).to_string();
    let (url, worker) = server(vec![(200, count), (200, rows)]);
    super::refresh_at(source.path(), &url).unwrap();
    worker.join().unwrap();
    let shared_dir = tempfile::tempdir().unwrap();
    let shared_index = shared_dir.path().join("wikidata/chemicals.sqlite");
    let copied = super::index::copy_snapshot(source.path(), &shared_index).unwrap();
    assert_eq!(copied.pairs, 3);
    assert!(shared_index.is_file());

    for job in [&first, &second] {
        let mut opts = options(job.path());
        opts.index = Some(shared_index.clone());
        let result = enrich_at(&opts, "http://127.0.0.1:1/sparql").unwrap();
        assert_eq!((result.matched, result.not_found, result.ambiguous), (1, 1, 1));
        let db = Connection::open(job.path()).unwrap();
        let copies: i64 = db.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name='wikidata_chemical_index'", [], |row| row.get(0)
        ).unwrap();
        assert_eq!(copies, 0, "a job must not duplicate the shared index");
    }
}
