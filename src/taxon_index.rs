//! Complete, replaceable local QLever index of truthy Wikidata taxon names and identifiers.

use std::fs;
use std::path::Path;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use reqwest::blocking::{Client, Response};
use reqwest::header::{ACCEPT, RETRY_AFTER};
use reqwest::StatusCode;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde_json::Value;

pub const QLEVER: &str = "https://qlever.dev/api/wikidata";
const PROPERTIES: [&str; 5] = ["P225", "P10585", "P846", "P14607", "P3151"];
// Counting each predicate independently avoids OPTIONAL-join cartesian products.
const COUNT: &str = "PREFIX wdt: <http://www.wikidata.org/prop/direct/> SELECT ?property (COUNT(*) AS ?count) WHERE { VALUES ?property { wdt:P225 wdt:P10585 wdt:P846 wdt:P14607 wdt:P3151 } ?item ?property ?value . FILTER(STRSTARTS(STR(?item), \"http://www.wikidata.org/entity/Q\")) } GROUP BY ?property";
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS wikidata_taxon_names (
    name TEXT NOT NULL,
    qid TEXT NOT NULL,
    PRIMARY KEY(name,qid)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS wikidata_taxon_ids (
    qid TEXT NOT NULL,
    property TEXT NOT NULL,
    identifier TEXT NOT NULL,
    PRIMARY KEY(qid,property,identifier)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS wikidata_taxon_snapshot (
    id INTEGER PRIMARY KEY CHECK(id=1),
    generation INTEGER NOT NULL,
    fetched_at TEXT NOT NULL,
    endpoint TEXT NOT NULL,
    names INTEGER NOT NULL
);
"#;

#[derive(Debug)]
pub struct Snapshot {
    pub names: u64,
    pub generation: i64,
    pub fetched_at: String,
}

pub fn generation(db: &Connection) -> Result<Option<i64>> {
    let exists: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='wikidata_taxon_snapshot' AND type='table')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(None);
    }
    Ok(db
        .query_row(
            "SELECT generation FROM wikidata_taxon_snapshot WHERE id=1",
            [],
            |row| row.get(0),
        )
        .optional()?)
}

fn property(uri: &str) -> Result<&'static str> {
    let code = uri
        .strip_prefix("http://www.wikidata.org/prop/direct/")
        .or_else(|| uri.strip_prefix("https://www.wikidata.org/prop/direct/"))
        .with_context(|| format!("QLever returned an invalid property URI: {uri}"))?;
    PROPERTIES
        .iter()
        .copied()
        .find(|known| *known == code)
        .with_context(|| format!("QLever returned an unexpected property: {uri}"))
}

fn qid(uri: &str) -> Result<&str> {
    let id = uri
        .strip_prefix("http://www.wikidata.org/entity/")
        .or_else(|| uri.strip_prefix("https://www.wikidata.org/entity/"))
        .with_context(|| format!("QLever item is not a Wikidata entity URI: {uri}"))?;
    ensure!(
        id.strip_prefix('Q')
            .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())),
        "QLever returned an invalid item QID: {uri}"
    );
    Ok(id)
}

fn request(client: &Client, endpoint: &str, query: &str, accept: &str, send: Option<&str>) -> Result<Response> {
    for attempt in 0..=4 {
        let mut request = client.get(endpoint).header(ACCEPT, accept).query(&[("query", query)]);
        if let Some(send) = send {
            request = request.query(&[("send", send)]);
        }
        let response = request.send().context("requesting QLever taxon index")?;
        if matches!(response.status(), StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE)
            && attempt < 4
        {
            let delay = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(1 << attempt)
                .min(8);
            thread::sleep(Duration::from_secs(delay));
            continue;
        }
        return response.error_for_status().context("QLever taxon index response");
    }
    unreachable!("the final request always returns a response or error")
}

fn counts(client: &Client, endpoint: &str) -> Result<[u64; 5]> {
    let response: Value = request(client, endpoint, COUNT, "application/sparql-results+json", None)?
        .json()
        .context("decoding QLever taxon counts")?;
    let bindings = response["results"]["bindings"]
        .as_array()
        .context("QLever taxon counts have no bindings")?;
    let mut expected = [None; 5];
    for binding in bindings {
        let uri = binding["property"]["value"]
            .as_str()
            .context("QLever taxon count has no property")?;
        let code = property(uri)?;
        let index = PROPERTIES.iter().position(|candidate| *candidate == code).unwrap();
        let count: u64 = binding["count"]["value"]
            .as_str()
            .context("QLever taxon count has no value")?
            .parse()
            .context("invalid QLever taxon count")?;
        ensure!(expected[index].replace(count).is_none(), "duplicate QLever count for {code}");
    }
    let expected = expected.map(|count| count.unwrap_or(0));
    ensure!(expected[0] > 0, "QLever returned an empty P225 index; refusing to replace snapshot");
    Ok(expected)
}

/// Replace the index only after all independently counted QLever triples have been imported.
pub fn refresh_at(database: &Path, endpoint: &str) -> Result<Snapshot> {
    refresh_with_page_size(database, endpoint, 200_000)
}

fn refresh_with_page_size(database: &Path, endpoint: &str, page_size: u64) -> Result<Snapshot> {
    ensure!(page_size > 0, "QLever taxon page size must be positive");
    if let Some(parent) = database.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create taxon index directory {}", parent.display()))?;
    }
    let mut db = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("open taxon index {}", database.display()))?;
    db.busy_timeout(Duration::from_secs(30))?;
    let client = Client::builder()
        .user_agent("cunaguaro/0.1 (https://github.com/oolonek/cunaguaro; Wikidata taxon snapshot)")
        .timeout(Duration::from_secs(1800))
        .build()?;
    let expected = counts(&client, endpoint)?;
    // HTTP can terminate a multi-million-row CSV mid-stream. Page each fixed
    // predicate by item key instead of OFFSET: a concurrent Wikidata update
    // before the cursor cannot shift later pages across a numeric offset.
    // Re-read the boundary item on the next page to include all its IDs.
    // QLever's `send` is independent of SPARQL LIMIT; verify counts after import.

    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(SCHEMA)?;
    let next = generation(&tx)?.unwrap_or(0).checked_add(1)
        .context("taxon snapshot generation overflow")?;
    tx.execute("DELETE FROM wikidata_taxon_ids", [])?;
    tx.execute("DELETE FROM wikidata_taxon_names", [])?;
    {
        let mut insert_name = tx.prepare(
            "INSERT OR IGNORE INTO wikidata_taxon_names(name,qid) VALUES(?1,?2)",
        )?;
        let mut insert_id = tx.prepare(
            "INSERT OR IGNORE INTO wikidata_taxon_ids(qid,property,identifier) VALUES(?1,?2,?3)",
        )?;
        for (code, expected_count) in PROPERTIES.iter().zip(expected) {
            if expected_count == 0 {
                continue;
            }
            let mut cursor: Option<String> = None;
            loop {
                let after = match &cursor {
                    Some(last) => format!(
                        "FILTER(STR(?item) >= {})",
                        serde_json::to_string(last)?
                    ),
                    None => String::new(),
                };
                let query = format!(
                    "PREFIX wdt: <http://www.wikidata.org/prop/direct/> \
                     SELECT ?item ?value WHERE {{ ?item wdt:{code} ?value . \
                     FILTER(STRSTARTS(STR(?item), \"http://www.wikidata.org/entity/Q\")) {after} \
                     }} ORDER BY ?item ?value LIMIT {page_size}"
                );
                let send = page_size.to_string();
                let response = request(&client, endpoint, &query, "text/csv", Some(&send))?;
                let mut csv = csv::Reader::from_reader(response);
                ensure!(
                    csv.headers()?.iter().eq(["item", "value"]),
                    "unexpected QLever {code} CSV columns"
                );
                let mut row = csv::StringRecord::new();
                let mut received = 0_u64;
                let mut last_item = String::new();
                while csv.read_record(&mut row)? {
                    received += 1;
                    ensure!(received <= page_size, "QLever {code} page exceeded LIMIT {page_size}");
                    let uri = row.get(0).context("QLever taxon row has no item")?;
                    let item = qid(uri)?;
                    let value = row.get(1).context("QLever taxon row has no value")?;
                    ensure!(!value.is_empty(), "QLever returned an empty {code} value for {item}");
                    if *code == "P225" {
                        insert_name.execute(params![value, item])?;
                    } else {
                        insert_id.execute(params![item, code, value])?;
                    }
                    last_item.clear();
                    last_item.push_str(uri);
                }
                if received < page_size {
                    break;
                }
                ensure!(
                    cursor.as_deref() != Some(last_item.as_str()),
                    "QLever {code} has at least {page_size} identifiers for one item; previous snapshot retained"
                );
                cursor = Some(last_item);
            }
        }
    }
    let names: u64 = tx.query_row("SELECT COUNT(*) FROM wikidata_taxon_names", [], |row| row.get(0))?;
    let mut actual = [0_u64; 5];
    actual[0] = names;
    let mut count_ids = tx.prepare("SELECT COUNT(*) FROM wikidata_taxon_ids WHERE property=?1")?;
    for (position, code) in PROPERTIES.iter().enumerate().skip(1) {
        actual[position] = count_ids.query_row([*code], |row| row.get(0))?;
    }
    drop(count_ids);
    // A live QLever mirror can gain or lose a statement during a long export.
    // Accept a completed stream only if every cardinality matches its starting
    // count or a fresh ending count. Keyset pages avoid OFFSET drift.
    let latest = if actual != expected {
        Some(counts(&client, endpoint)?)
    } else {
        None
    };
    for (position, code) in PROPERTIES.iter().enumerate() {
        ensure!(
            actual[position] == expected[position]
                || latest.is_some_and(|counts| actual[position] == counts[position]),
            "QLever {code} export incomplete: expected {} pairs, received {}, latest count {}; previous snapshot retained",
            expected[position], actual[position],
            latest.map_or(expected[position], |counts| counts[position])
        );
    }
    tx.execute(
        "INSERT INTO wikidata_taxon_snapshot(id,generation,fetched_at,endpoint,names)
         VALUES(1,?1,strftime('%Y-%m-%dT%H:%M:%fZ','now'),?2,?3)
         ON CONFLICT(id) DO UPDATE SET generation=excluded.generation,fetched_at=excluded.fetched_at,
            endpoint=excluded.endpoint,names=excluded.names",
        params![next, endpoint, names],
    )?;
    let fetched_at: String = tx.query_row(
        "SELECT fetched_at FROM wikidata_taxon_snapshot WHERE id=1", [], |row| row.get(0)
    )?;
    tx.commit()?;
    Ok(Snapshot { names, generation: next, fetched_at })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::time::Instant;

    const PREFIX: &str = "http://www.wikidata.org/prop/direct/";
    const ENTITY: &str = "http://www.wikidata.org/entity/";

    fn counts_json(names: u64, col: u64, gbif: u64, current: u64, inat: u64) -> String {
        let bindings: Vec<_> = PROPERTIES.iter().zip([names, col, gbif, current, inat]).map(|(code, count)| {
            serde_json::json!({"property": {"value": format!("{PREFIX}{code}")}, "count": {"value": count.to_string()}})
        }).collect();
        serde_json::json!({"results": {"bindings": bindings}}).to_string()
    }

    fn rows() -> String {
        format!(
            "item,property,value\n\
             {ENTITY}Q10,{PREFIX}P225,Alpha\n\
             {ENTITY}Q20,{PREFIX}P225,Alpha\n\
             {ENTITY}Q30,{PREFIX}P225,\"Beta, gamma\"\n\
             {ENTITY}Q10,{PREFIX}P10585,COL-A\n\
             {ENTITY}Q10,{PREFIX}P10585,COL-B\n\
             {ENTITY}Q10,{PREFIX}P846,123\n\
             {ENTITY}Q10,{PREFIX}P14607,987\n\
             {ENTITY}Q20,{PREFIX}P14607,654\n\
             {ENTITY}Q30,{PREFIX}P3151,456\n"
        )
    }

    fn page(code: &str) -> String {
        let mut csv = String::from("item,value\n");
        for line in rows().lines().skip(1) {
            let mut fields = line.splitn(3, ',');
            let item = fields.next().unwrap();
            let property = fields.next().unwrap();
            let value = fields.next().unwrap();
            if property == format!("{PREFIX}{code}") {
                csv.push_str(item);
                csv.push(',');
                csv.push_str(value);
                csv.push('\n');
            }
        }
        csv
    }

    fn replies() -> Vec<(u16, String)> {
        let mut replies = vec![(200, counts_json(3, 2, 1, 2, 1))];
        for code in PROPERTIES {
            replies.push((200, page(code)));
        }
        replies
    }

    fn server(replies: Vec<(u16, String)>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/sparql", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, body) in replies {
                let deadline = Instant::now() + Duration::from_secs(10);
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(pair) => break pair,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("waiting for taxon snapshot request: {error}"),
                    }
                };
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
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
                    _ => "Service Unavailable",
                };
                write!(stream, "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                stream.flush().unwrap();
            }
            requests
        });
        (url, handle)
    }

    #[test]
    fn complete_import_preserves_ambiguous_names_and_each_propertys_multiple_ids() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let (url, worker) = server(replies());
        let snapshot = refresh_at(file.path(), &url).unwrap();
        let requests = worker.join().unwrap();
        assert_eq!(requests.len(), 6);
        assert!(requests[1].contains("send="), "QLever silently truncates unsized exports");
        assert_eq!((snapshot.names, snapshot.generation), (3, 1));
        assert!(!snapshot.fetched_at.is_empty());
        let db = Connection::open_with_flags(file.path(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert_eq!(generation(&db).unwrap(), Some(1));
        let names: Vec<String> = db.prepare("SELECT qid FROM wikidata_taxon_names WHERE name='Alpha' ORDER BY qid")
            .unwrap().query_map([], |row| row.get(0)).unwrap().map(|row| row.unwrap()).collect();
        assert_eq!(names, ["Q10", "Q20"]);
        let ids: Vec<(String, String, String)> = db.prepare(
            "SELECT qid,property,identifier FROM wikidata_taxon_ids ORDER BY qid,property,identifier"
        ).unwrap().query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap().map(|row| row.unwrap()).collect();
        assert_eq!(ids, [
            ("Q10".into(), "P10585".into(), "COL-A".into()),
            ("Q10".into(), "P10585".into(), "COL-B".into()),
            ("Q10".into(), "P14607".into(), "987".into()),
            ("Q10".into(), "P846".into(), "123".into()),
            ("Q20".into(), "P14607".into(), "654".into()),
            ("Q30".into(), "P3151".into(), "456".into()),
        ]);
        let provenance: (String, i64) = db.query_row(
            "SELECT endpoint,names FROM wikidata_taxon_snapshot WHERE id=1", [],
            |row| Ok((row.get(0)?, row.get(1)?))
        ).unwrap();
        assert_eq!(provenance, (url, 3));
    }

    #[test]
    fn multiple_export_pages_preserve_complete_snapshot() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut pages = replies();
        pages.insert(2, (200, format!("item,value\n{ENTITY}Q30,\"Beta, gamma\"\n")));
        let (url, worker) = server(pages);
        let snapshot = refresh_with_page_size(file.path(), &url, 3).unwrap();
        assert_eq!(snapshot.names, 3);
        assert_eq!(worker.join().unwrap().len(), 7);
        let db = Connection::open(file.path()).unwrap();
        let value: String = db.query_row(
            "SELECT identifier FROM wikidata_taxon_ids WHERE qid='Q30' AND property='P3151'",
            [], |row| row.get(0)
        ).unwrap();
        assert_eq!(value, "456");
    }

    #[test]
    fn live_count_change_can_match_completed_export_without_losing_names() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut export = replies();
        export[0].1 = counts_json(2, 2, 1, 2, 1);
        export.push((200, counts_json(3, 2, 1, 2, 1)));
        let (url, worker) = server(export);
        let snapshot = refresh_at(file.path(), &url).unwrap();
        worker.join().unwrap();
        assert_eq!(snapshot.names, 3);
        let db = Connection::open(file.path()).unwrap();
        let qid: String = db.query_row(
            "SELECT qid FROM wikidata_taxon_names WHERE name='Beta, gamma'",
            [], |row| row.get(0)
        ).unwrap();
        assert_eq!(qid, "Q30");
    }

    #[test]
    fn incomplete_property_export_preserves_old_snapshot_then_success_increments_generation() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let (initial_url, worker) = server(replies());
        let first = refresh_at(file.path(), &initial_url).unwrap();
        worker.join().unwrap();
        let mut truncated = replies();
        truncated[5].1 = "item,value\n".to_owned();
        truncated.push((200, counts_json(3, 2, 1, 2, 1)));
        let (url, worker) = server(truncated);
        let error = refresh_at(file.path(), &url).unwrap_err();
        worker.join().unwrap();
        assert!(error.to_string().contains("P3151 export incomplete"), "{error:#}");
        let db = Connection::open_with_flags(file.path(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert_eq!(generation(&db).unwrap(), Some(first.generation));
        let previous: (i64, String) = db.query_row(
            "SELECT names,endpoint FROM wikidata_taxon_snapshot WHERE id=1", [],
            |row| Ok((row.get(0)?, row.get(1)?))
        ).unwrap();
        assert_eq!(previous, (3, initial_url));
        let count: i64 = db.query_row("SELECT COUNT(*) FROM wikidata_taxon_ids WHERE property='P3151'", [], |row| row.get(0)).unwrap();
        assert_eq!(count, 1);
        drop(db);
        let (url, worker) = server(replies());
        let second = refresh_at(file.path(), &url).unwrap();
        worker.join().unwrap();
        assert_eq!(second.generation, first.generation + 1);
    }

    #[test]
    fn retries_rate_limit_without_publishing_an_incomplete_snapshot() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut pages = replies();
        pages.insert(0, (429, "rate limited".into()));
        let (url, worker) = server(pages);
        assert_eq!(refresh_at(file.path(), &url).unwrap().names, 3);
        assert_eq!(worker.join().unwrap().len(), 7);
    }
}
