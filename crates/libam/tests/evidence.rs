//! Every claim of the libam evidence ledger (`spec/libam/evidence.toml`) cites addresses that lie inside the code
//! (`.text`) or read-only data (`.rdata`) of the analysed image, `sample/amdaemon.exe`. The test is skipped when
//! the sample is not checked out (it is not redistributable).

use std::collections::HashSet;
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};

use object::read::pe::PeFile64;
use object::{Object, ObjectSection};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Ledger {
    #[serde(default)]
    claim: Vec<Claim>,
}

#[derive(Debug, Deserialize)]
struct Claim {
    id: String,
    kind: String,
    va: Vec<u64>,
    summary: String,
}

const KINDS: &[&str] = &["layout", "constant", "behaviour", "order", "protocol", "string", "state", "error"];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("libam lives in crates/ of the workspace")
        .to_path_buf()
}

/// Address ranges of `.text` and `.rdata` in the sample image.
fn cited_sections(image: &[u8]) -> Vec<(String, Range<u64>)> {
    let pe = PeFile64::parse(image).expect("sample/amdaemon.exe is a PE32+ image");
    pe.sections()
        .filter_map(|section| {
            let name = section.name().ok()?.to_owned();
            matches!(name.as_str(), ".text" | ".rdata")
                .then(|| (name, section.address()..section.address() + section.size()))
        })
        .collect()
}

#[test]
fn every_libam_claim_cites_code_or_read_only_data_of_the_sample() {
    let root = workspace_root();
    let Ok(image) = fs::read(root.join("sample/amdaemon.exe")) else {
        eprintln!("sample/amdaemon.exe is absent; skipping the evidence check");
        return;
    };
    let sections = cited_sections(&image);
    assert_eq!(sections.len(), 2, "the sample has both a .text and an .rdata section: {sections:?}");

    let text = fs::read_to_string(root.join("spec/libam/evidence.toml")).expect("spec/libam/evidence.toml exists");
    let ledger: Ledger = toml::from_str(&text).expect("a valid ledger");
    assert!(!ledger.claim.is_empty(), "the ledger holds claims");

    let mut ids = HashSet::new();
    let mut problems = Vec::new();
    for claim in &ledger.claim {
        if !ids.insert(claim.id.clone()) {
            problems.push(format!("duplicate claim id {}", claim.id));
        }
        if !KINDS.contains(&claim.kind.as_str()) {
            problems.push(format!("{} has unknown kind {:?}", claim.id, claim.kind));
        }
        if claim.va.is_empty() || claim.summary.trim().is_empty() {
            problems.push(format!("{} needs at least one address and a summary", claim.id));
        }
        for &va in &claim.va {
            if !sections.iter().any(|(_, range)| range.contains(&va)) {
                problems.push(format!("{} cites {va:#x}, outside .text and .rdata", claim.id));
            }
        }
    }
    assert!(problems.is_empty(), "{} evidence problems:\n{}", problems.len(), problems.join("\n"));
}
