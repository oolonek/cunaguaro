//! Clickable subject–predicate–object views of the conceptual local graph.

use std::io::Write;

use anyhow::Result;

// QID nodes are external alignments; every other entity node is local. Values
// are literal objects. Links navigate to the corresponding proposal headings.
const NODES: &[(&str, &str, Option<&str>)] = &[
    ("dataset", "Local Dataset", Some("#dataset")),
    (
        "dataset_class",
        "urn:mato:term:Dataset (class)",
        Some("#dataset"),
    ),
    ("doi", "DOI literal", Some("#wikidata-alignment")),
    (
        "wd_dataset",
        "Existing dataset QID (if verified)",
        Some("#wikidata-alignment"),
    ),
    ("sample", "Local SampleRecord", Some("#samplerecord")),
    (
        "sample_id",
        "Sample identifier literal (not unique)",
        Some("#samplerecord"),
    ),
    ("taxon", "Verified taxon QID", Some("#wikidata-alignment")),
    (
        "matched_organism",
        "Recorded reference organism QID (if valid)",
        Some("#local-subjectpredicateobject-mapping"),
    ),
    (
        "taxon_name",
        "Scientific name literal",
        Some("#wikidata-alignment"),
    ),
    (
        "col_id",
        "Catalogue of Life ID literal",
        Some("#wikidata-alignment"),
    ),
    (
        "acquisition",
        "Local Acquisition (MS run)",
        Some("#acquisition"),
    ),
    ("raw", "Local RawFile (raw input)", Some("#rawfile")),
    (
        "processing",
        "Local Processing (Picoto)",
        Some("#processing"),
    ),
    (
        "audit",
        "Local FeatureAuditFile (audit TSV)",
        Some("#featureauditfile"),
    ),
    (
        "feature",
        "Local Feature (accepted audit row)",
        Some("#feature"),
    ),
    (
        "mgf",
        "Local ProcessedSpectrumFile (MGF)",
        Some("#processedspectrumfile"),
    ),
    (
        "query",
        "Local QuerySpectrum (MGF block)",
        Some("#queryspectrum"),
    ),
    ("run", "Local AnnotationRun", Some("#annotationrun")),
    (
        "annotated",
        "Local AnnotationFeature (run-scoped)",
        Some("#annotationfeature"),
    ),
    (
        "hit",
        "Local CandidateHit (not presence)",
        Some("#candidatehit"),
    ),
    (
        "reference",
        "Local ReferenceSpectrum",
        Some("#referencespectrum"),
    ),
    (
        "chemical",
        "Verified chemical QID (one full-key match)",
        Some("#wikidata-alignment"),
    ),
    ("cid", "PubChem CID literal", Some("#wikidata-alignment")),
    (
        "inchikey",
        "Full InChIKey literal",
        Some("#wikidata-alignment"),
    ),
    (
        "mz",
        "Precursor m/z literal",
        Some("#local-subjectpredicateobject-mapping"),
    ),
    (
        "rt",
        "Retention time (seconds) literal",
        Some("#local-subjectpredicateobject-mapping"),
    ),
    (
        "score",
        "Score / rank literal",
        Some("#local-subjectpredicateobject-mapping"),
    ),
    (
        "peak_count",
        "Matched peak count literal",
        Some("#local-subjectpredicateobject-mapping"),
    ),
    (
        "support",
        "Taxonomic support literal",
        Some("#local-subjectpredicateobject-mapping"),
    ),
    (
        "version",
        "Scoring tool version literal",
        Some("#local-subjectpredicateobject-mapping"),
    ),
    (
        "status",
        "Outcome status literal",
        Some("#local-subjectpredicateobject-mapping"),
    ),
];

struct Statement {
    subject: &'static str,
    predicate: &'static str,
    object: &'static str,
}

// The PIDs label edges from Wikidata subjects; none is an item/node.
const STATEMENTS: &[Statement] = &[
    Statement {
        subject: "dataset",
        predicate: "rdf:type",
        object: "dataset_class",
    },
    Statement {
        subject: "dataset",
        predicate: "wdt:P356",
        object: "doi",
    },
    Statement {
        subject: "wd_dataset",
        predicate: "wdt:P356",
        object: "doi",
    },
    Statement {
        subject: "taxon",
        predicate: "wdt:P225",
        object: "taxon_name",
    },
    Statement {
        subject: "taxon",
        predicate: "wdt:P10585",
        object: "col_id",
    },
    Statement {
        subject: "chemical",
        predicate: "wdt:P235",
        object: "inchikey",
    },
    Statement {
        subject: "chemical",
        predicate: "wdt:P662",
        object: "cid",
    },
    Statement {
        subject: "dataset",
        predicate: "hasSampleRecord",
        object: "sample",
    },
    Statement {
        subject: "sample",
        predicate: "sampleIdentifier",
        object: "sample_id",
    },
    Statement {
        subject: "sample",
        predicate: "reportedSourceTaxon",
        object: "taxon",
    },
    Statement {
        subject: "sample",
        predicate: "hasAcquisition",
        object: "acquisition",
    },
    Statement {
        subject: "acquisition",
        predicate: "prov:used",
        object: "sample",
    },
    Statement {
        subject: "acquisition",
        predicate: "rawFile",
        object: "raw",
    },
    Statement {
        subject: "raw",
        predicate: "prov:wasGeneratedBy",
        object: "acquisition",
    },
    Statement {
        subject: "processing",
        predicate: "prov:used",
        object: "raw",
    },
    Statement {
        subject: "audit",
        predicate: "prov:wasGeneratedBy",
        object: "processing",
    },
    Statement {
        subject: "mgf",
        predicate: "prov:wasGeneratedBy",
        object: "processing",
    },
    Statement {
        subject: "processing",
        predicate: "featureAudit",
        object: "audit",
    },
    Statement {
        subject: "processing",
        predicate: "processedSpectrumFile",
        object: "mgf",
    },
    Statement {
        subject: "processing",
        predicate: "hasFeature",
        object: "feature",
    },
    Statement {
        subject: "feature",
        predicate: "retentionTimeSeconds",
        object: "rt",
    },
    Statement {
        subject: "feature",
        predicate: "precursorMz",
        object: "mz",
    },
    Statement {
        subject: "feature",
        predicate: "querySpectrum",
        object: "query",
    },
    Statement {
        subject: "query",
        predicate: "derivedFrom",
        object: "mgf",
    },
    Statement {
        subject: "run",
        predicate: "prov:used",
        object: "mgf",
    },
    Statement {
        subject: "run",
        predicate: "scoringTaxon",
        object: "taxon",
    },
    Statement {
        subject: "run",
        predicate: "matcherVersion",
        object: "version",
    },
    Statement {
        subject: "run",
        predicate: "annotationFeature",
        object: "annotated",
    },
    Statement {
        subject: "annotated",
        predicate: "feature",
        object: "feature",
    },
    Statement {
        subject: "run",
        predicate: "hasCandidateHit",
        object: "hit",
    },
    Statement {
        subject: "hit",
        predicate: "prov:wasGeneratedBy",
        object: "run",
    },
    Statement {
        subject: "hit",
        predicate: "feature",
        object: "feature",
    },
    Statement {
        subject: "hit",
        predicate: "referenceSpectrum",
        object: "reference",
    },
    Statement {
        subject: "hit",
        predicate: "candidateChemical",
        object: "chemical",
    },
    Statement {
        subject: "hit",
        predicate: "spectralScore",
        object: "score",
    },
    Statement {
        subject: "hit",
        predicate: "taxonomicScore",
        object: "score",
    },
    Statement {
        subject: "hit",
        predicate: "combinedScore",
        object: "score",
    },
    Statement {
        subject: "hit",
        predicate: "rank",
        object: "score",
    },
    Statement {
        subject: "hit",
        predicate: "matchedOrganism",
        object: "matched_organism",
    },
    Statement {
        subject: "hit",
        predicate: "matchedPeakCount",
        object: "peak_count",
    },
    Statement {
        subject: "hit",
        predicate: "taxonomicSupport",
        object: "support",
    },
    Statement {
        subject: "run",
        predicate: "status",
        object: "status",
    },
];

fn node(id: &str) -> (&'static str, Option<&'static str>) {
    let (_, label, link) = NODES
        .iter()
        .find(|node| node.0 == id)
        .expect("statement references declared node");
    (label, *link)
}

/// Mermaid fragment links navigate to the corresponding sections of the report.
pub fn mermaid(output: &mut impl Write) -> Result<()> {
    mermaid_with_report(output, "")
}

/// Render saved Mermaid links against a Markdown report in the same directory.
pub fn mermaid_with_report(output: &mut impl Write, report_name: &str) -> Result<()> {
    writeln!(output, "flowchart LR")?;
    for &(id, text, _) in NODES {
        writeln!(output, "    {id}[\"{text}\"]")?;
    }
    for statement in STATEMENTS {
        writeln!(
            output,
            "    {} -->|\"{}\"| {}",
            statement.subject, statement.predicate, statement.object
        )?;
    }
    for &(id, _, link) in NODES {
        if let Some(link) = link {
            writeln!(
                output,
                "    click {id} \"{report_name}{link}\" \"View {id} mapping\""
            )?;
        }
    }
    Ok(())
}

/// SVG statement rows; clickable entities navigate to report sections.
pub fn svg(output: &mut impl Write) -> Result<()> {
    svg_with_report(output, "")
}

/// Render saved SVG links against a Markdown report in the same directory.
pub fn svg_with_report(output: &mut impl Write, report_name: &str) -> Result<()> {
    let height = 40 + STATEMENTS.len() * 66;
    writeln!(
        output,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="1110" height="{height}" viewBox="0 0 1110 {height}" role="img" aria-label="Local metabolomics graph and optional Wikidata statement schema">"#
    )?;
    writeln!(
        output,
        r#"<style>text{{fill:currentColor;font:13px sans-serif}} .predicate{{font-weight:bold}} rect{{fill:none;pointer-events:all;stroke:var(--border,currentColor);stroke-width:1.5}} path{{fill:none;stroke:var(--muted,currentColor);stroke-width:2}} a:hover rect{{stroke:var(--accent,currentColor);stroke-width:3}}</style>"#
    )?;
    writeln!(
        output,
        r#"<defs><marker id="arrow" markerWidth="8" markerHeight="8" refX="7" refY="4" orient="auto"><path d="M0 0 L8 4 L0 8" style="fill:currentColor;stroke:none"/></marker></defs>"#
    )?;
    for (index, statement) in STATEMENTS.iter().enumerate() {
        let top = 20 + index * 66;
        let mid = top + 24;
        let text_y = mid + 5;
        let (subject, subject_link) = node(statement.subject);
        let (object, object_link) = node(statement.object);
        writeln!(
            output,
            r#"<path d="M360 {mid} H380 M739 {mid} H758" marker-end="url(#arrow)"/>"#
        )?;
        writeln!(
            output,
            r#"<a href="{}"><g><title>{subject}</title><rect x="10" y="{top}" width="350" height="48" rx="7"/><text x="18" y="{text_y}">{subject}</text></g></a>"#,
            format_args!(
                "{}{}",
                report_name,
                subject_link.unwrap_or("#local-domain-nodes")
            )
        )?;
        // Predicate is on the edge; neither a subject nor a target node.
        writeln!(
            output,
            r#"<text x="386" y="{text_y}" class="predicate">{}</text>"#,
            statement.predicate
        )?;
        writeln!(
            output,
            r#"<a href="{}"><g><title>{object}</title><rect x="760" y="{top}" width="340" height="48" rx="7"/><text x="768" y="{text_y}">{object}</text></g></a>"#,
            format_args!(
                "{}{}",
                report_name,
                object_link.unwrap_or("#local-domain-nodes")
            )
        )?;
    }
    writeln!(output, "</svg>")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_nodes_and_external_properties_stay_distinct() {
        let mut mermaid_graph = Vec::new();
        mermaid(&mut mermaid_graph).unwrap();
        let mermaid_graph = String::from_utf8(mermaid_graph).unwrap();
        for triple in [
            "wd_dataset -->|\"wdt:P356\"| doi",
            "chemical -->|\"wdt:P235\"| inchikey",
            "sample -->|\"hasAcquisition\"| acquisition",
            "acquisition -->|\"rawFile\"| raw",
            "processing -->|\"prov:used\"| raw",
            "processing -->|\"hasFeature\"| feature",
            "feature -->|\"querySpectrum\"| query",
            "hit -->|\"referenceSpectrum\"| reference",
            "hit -->|\"candidateChemical\"| chemical",
        ] {
            assert!(mermaid_graph.contains(triple), "missing {triple}");
        }
        assert!(mermaid_graph.contains("click sample \"#samplerecord\""));
        assert!(mermaid_graph.contains("click raw \"#rawfile\""));
        assert!(!mermaid_graph.contains("Processed spectrum item"));
        let mut svg_graph = Vec::new();
        svg(&mut svg_graph).unwrap();
        let svg_graph = String::from_utf8(svg_graph).unwrap();
        assert!(svg_graph.contains("class=\"predicate\">wdt:P235</text>"));
        assert!(svg_graph.contains("href=\"#rawfile\""));
        assert!(svg_graph.contains("Local QuerySpectrum (MGF block)"));
    }

    #[test]
    fn separate_diagrams_link_to_the_sibling_markdown_report() {
        let mut svg_graph = Vec::new();
        svg_with_report(&mut svg_graph, "toy-kg-schema.md").unwrap();
        assert!(
            String::from_utf8(svg_graph)
                .unwrap()
                .contains("href=\"toy-kg-schema.md#rawfile\"")
        );
        let mut mermaid_graph = Vec::new();
        mermaid_with_report(&mut mermaid_graph, "toy-kg-schema.md").unwrap();
        assert!(
            String::from_utf8(mermaid_graph)
                .unwrap()
                .contains("click raw \"toy-kg-schema.md#rawfile\"")
        );
    }
}
