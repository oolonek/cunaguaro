# cunaguaro

Cunaguaro connects organism names and possible chemicals in a research dataset to Wikidata records. It records uncertainty rather than guessing, and a matching record does not prove a chemical is present in a sample.

The flock helps turn mass-spectrometry samples and measurements into traceable evidence for possible chemical matches, with an optional local knowledge-graph export. [Start with the flock overview](https://github.com/oolonek/isla-gabriola-flock/blob/main/README.md) to see how the crates work together before following a technical guide.

The name refers to the cunaguaro, *Leopardus pardalis* ([Wikidata Q33261](https://www.wikidata.org/wiki/Q33261); [Catalogue of Life 3T78D](https://www.catalogueoflife.org/data/taxon/3T78D)), the ocelot.

[![Ocelot perched in a tree at Zoo Ave, Costa Rica](https://commons.wikimedia.org/wiki/Special:FilePath/Leopardus%20pardalis.jpg?width=500)](https://commons.wikimedia.org/wiki/File:Leopardus_pardalis.jpg)

Photo: [Hans Hillewaert](https://commons.wikimedia.org/wiki/File:Leopardus_pardalis.jpg), [CC BY-SA 3.0](https://creativecommons.org/licenses/by-sa/3.0/).

## In the flock

Peregrino stores sample records and their organism names; Hocofaisan supplies scored **candidate hits**. Cunaguaro checks these names and possible chemicals against Wikidata. It can sketch how they might fit a knowledge graph; Peregrino produces the graph itself.

## Start here

After the overview, see [Mato's toy dataset walkthrough](https://github.com/oolonek/mato/blob/master/TECHNICAL.md#example-download-the-zenodo-toy-dataset-and-export-a-kg) for a small example of Cunaguaro's place in the flock. The [technical guide](TECHNICAL.md#shared-taxon-index-and-reconciliation) explains how to prepare its shared Wikidata records and inspect matches.

## Learn more

The [technical guide](TECHNICAL.md) covers shared indexes, conservative reconciliation, chemical matching, the conceptual mapping report, and graph commands. For specialist context, see [chemical identifier semantics](WIKIDATA.md), the [current toy mapping report](toy-kg-schema.md), and [ontology research](ONTOLOGY-RESEARCH.md). The older `toy-schema.md` is obsolete.

For steps tied to one workstation's installed files and services, consult the ignored local guide `LOCAL.md` if it is available.

The code is available under either the [MIT license](LICENSE-MIT) or the [Apache 2.0 license](LICENSE-APACHE).

*Naming note: For new species names, we take inspiration from [colonizer](https://crates.io/crates/colonizer)'s `inspire` mode, which suggests names from the Catalogue of Life, rather than inventing each crate name ourselves.*
