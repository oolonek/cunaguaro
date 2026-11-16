use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use cunaguaro::{chemical, diagrams, enrich, proposal, taxon_index};

fn sibling_report_name(path: &Path) -> Result<String> {
    let report = path.with_extension("md");
    let name = report
        .file_name()
        .and_then(|name| name.to_str())
        .context("diagram basename must be UTF-8")?;
    let mut encoded = String::with_capacity(name.len());
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            write!(encoded, "%{byte:02X}")?;
        }
    }
    Ok(encoded)
}

/// Resolve Peregrino taxa and candidate chemicals; propose a Wikidata graph.
#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Existing Peregrino database (for the default taxon enrichment command).
    #[arg(long)]
    db: Option<PathBuf>,
    /// Complete shared read-only QLever taxon index for the default enrichment command.
    #[arg(long)]
    index: Option<PathBuf>,
    /// Log each taxon lookup as it completes.
    #[arg(long)]
    verbose: bool,
    /// Recheck successful taxon resolutions instead of reusing stored QIDs.
    #[arg(long)]
    refresh: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Resolve distinct full InChIKeys from current candidate hits.
    ChemicalEnrich {
        #[arg(long)]
        db: PathBuf,
        /// Shared read-only QLever chemical index; avoids one copy per job.
        #[arg(long)]
        index: Option<PathBuf>,
        #[arg(long, default_value_t = 25)]
        batch_size: usize,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        verbose: bool,
    },
    /// Build or refresh one shared SQLite index of Wikidata taxon names and identifiers.
    TaxonIndex {
        #[arg(long)]
        index: PathBuf,
        /// QLever Wikidata SPARQL endpoint.
        #[arg(long, default_value = taxon_index::QLEVER)]
        endpoint: String,
    },
    /// Build one shared SQLite QLever index or copy an existing job's snapshot.
    ChemicalIndex {
        #[arg(long)]
        index: PathBuf,
        /// Seed a shared index offline from an existing complete snapshot (no QLever request).
        #[arg(long)]
        from_db: Option<PathBuf>,
        /// QLever Wikidata SPARQL endpoint used when --from-db is absent.
        #[arg(long, default_value = chemical::QLEVER)]
        endpoint: String,
    },
    /// Print a local graph/Wikidata crosswalk and log unresolved external alignments.
    Propose {
        #[arg(long)]
        db: PathBuf,
        /// Write unresolved taxon/chemical TSV statuses here instead of stderr.
        #[arg(long)]
        unresolved_log: Option<PathBuf>,
        /// Also save the clickable SVG diagram separately.
        #[arg(long)]
        svg: Option<PathBuf>,
        /// Also save the clickable Mermaid diagram separately.
        #[arg(long)]
        mermaid: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::ChemicalEnrich {
            db,
            index,
            batch_size,
            limit,
            refresh,
            verbose,
        }) => {
            let result = chemical::enrich(&chemical::EnrichOptions {
                index,
                database: db,
                batch_size,
                limit,
                refresh,
                verbose,
            })?;
            println!(
                "distinct_keys={} reused={} pending={} attempted={} matched={} not_found={} ambiguous={}",
                result.distinct_keys,
                result.reused,
                result.pending,
                result.attempted,
                result.matched,
                result.not_found,
                result.ambiguous
            );
        }
        Some(Command::ChemicalIndex { index, from_db, endpoint }) => {
            let snapshot = if let Some(source) = &from_db {
                chemical::copy_snapshot(source, &index)?
            } else {
                chemical::refresh_snapshot(&index, &endpoint)?
            };
            println!(
                "chemical_index_pairs={} generation={} fetched_at={} index={}",
                snapshot.pairs, snapshot.generation, snapshot.fetched_at, index.display()
            );
        }
        Some(Command::TaxonIndex { index, endpoint }) => {
            let snapshot = taxon_index::refresh_at(&index, &endpoint)?;
            println!(
                "taxon_index_names={} generation={} fetched_at={} index={}",
                snapshot.names, snapshot.generation, snapshot.fetched_at, index.display()
            );
        }
        Some(Command::Propose {
            db,
            unresolved_log,
            svg,
            mermaid,
        }) => {
            let mut report = BufWriter::new(io::stdout().lock());
            if let Some(path) = unresolved_log {
                let file =
                    File::create(&path).with_context(|| format!("create {}", path.display()))?;
                let mut log = BufWriter::new(file);
                proposal::propose(&db, &mut report, &mut log)?;
                log.flush()?;
            } else {
                let mut log = BufWriter::new(io::stderr().lock());
                proposal::propose(&db, &mut report, &mut log)?;
                log.flush()?;
            }
            writeln!(report, "\n## Clickable Mermaid graph\n\n```mermaid")?;
            diagrams::mermaid(&mut report)?;
            writeln!(report, "```\n\n## Clickable SVG graph\n\n```svg")?;
            diagrams::svg(&mut report)?;
            writeln!(report, "```")?;
            if let Some(path) = svg {
                let mut file = BufWriter::new(
                    File::create(&path).with_context(|| format!("create {}", path.display()))?,
                );
                diagrams::svg_with_report(&mut file, &sibling_report_name(&path)?)?;
                file.flush()?;
            }
            if let Some(path) = mermaid {
                let mut file = BufWriter::new(
                    File::create(&path).with_context(|| format!("create {}", path.display()))?,
                );
                diagrams::mermaid_with_report(&mut file, &sibling_report_name(&path)?)?;
                file.flush()?;
            }
            report.flush()?;
        }
        None => {
            let db = cli.db.context("--db is required for taxon enrichment")?;
            let index = cli.index.context("--index is required for taxon enrichment; build with taxon-index --index PATH")?;
            let results = enrich(&db, &index, cli.verbose, cli.refresh)?;
            let mut reused = 0;
            let mut new_successes = 0;
            let mut errors = 0;
            for result in &results {
                if result.reused {
                    reused += 1;
                } else if result.error.is_some() {
                    errors += 1;
                } else {
                    new_successes += 1;
                }
                if !cli.verbose {
                    if let Some(error) = &result.error {
                        println!("{} ({}): {error}", result.name, result.taxon_id);
                    } else {
                        let prefix = if result.reused {
                            "reused Wikidata"
                        } else {
                            "Wikidata"
                        };
                        println!(
                            "{} ({}): {prefix} {} | Catalogue of Life {}",
                            result.name,
                            result.taxon_id,
                            result.qid.as_deref().unwrap_or("-"),
                            result
                                .catalogue_of_life_id
                                .as_deref()
                                .unwrap_or("not listed")
                        );
                    }
                }
            }
            println!(
                "Resolved {} of {} taxa ({} reused, {} new successes, {} errors)",
                reused + new_successes,
                results.len(),
                reused,
                new_successes,
                errors
            );
        }
    }
    Ok(())
}
