//! Code-index query — the lakitu side of the shared code "graph-brain"
//! (token-eff 8205bd, Track 1). Consumes dr-mario's static jina index artifact
//! (`records.jsonl` + `vectors.npy` + `manifest.json` + jina ONNX) and answers
//! an NL query with the matching symbol-spans PLUS their precise 1-hop graph
//! neighbors — never whole files.
//!
//! Built bottom-up. Landing first (and the build's crux): the **precise
//! graph-expand**. A *loose* expand was measured to inject ~52 spans (≈ reading
//! the whole files) → no token savings; a *precise* 1-hop expand over real
//! call/type edges holds the budget far lower → net-positive. The embed
//! (ort/ONNX, bit-matched to the golden) + cosine search + the MCP tool wire in
//! once the artifact ships.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

/// 1-hop edges of a record. Targets are record **ids** (O(1) resolve), split by
/// kind so each can be capped/weighted, and internal-only (an edge with no
/// record — std / 3rd-party — simply doesn't resolve and is dropped).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Edges {
    #[serde(default)]
    pub callees: Vec<String>,
    #[serde(default)]
    pub type_refs: Vec<String>,
}

/// One indexed symbol-span record — mirrors a line of dr-mario's `records.jsonl`.
#[derive(Debug, Clone, Deserialize)]
pub struct Record {
    pub id: String,
    pub repo: String,
    pub path: String,
    pub symbol: String,
    pub kind: String,
    /// `[start, end]`, 1-based inclusive.
    pub line_span: [u32; 2],
    pub header: String,
    /// The symbol's source (or window chunk) — what gets injected into context
    /// in place of a whole-file read.
    pub body: String,
    #[serde(default)]
    pub edges: Edges,
}

/// A span returned to the caller — a hit or an expanded neighbor, never a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub repo: String,
    pub path: String,
    pub symbol: String,
    pub line_span: [u32; 2],
    pub header: String,
    /// The symbol's source — injected in place of a whole-file read.
    pub body: String,
    /// `true` = a direct retrieval hit; `false` = pulled in by graph-expand.
    pub hit: bool,
}

fn span_of(r: &Record, hit: bool) -> Span {
    Span {
        repo: r.repo.clone(),
        path: r.path.clone(),
        symbol: r.symbol.clone(),
        line_span: r.line_span,
        header: r.header.clone(),
        body: r.body.clone(),
        hit,
    }
}

/// Precise 1-hop graph-expand. Returns the ranked `hit_ids` as spans (hits
/// first, in rank order), then the 1-hop neighbors (callees, then type-refs) of
/// only the top `expand_top_n` hits, deduped. Three knobs hold the token budget
/// that keeps the index net-positive — the measured lever that turns a
/// *marginal* net-positive into a clean one:
/// * `expand_top_n` — expand neighbors of only the top-N primary hits, not all
///   (dr-mario's pre-check: expanding all hits gave ~23 spans; the top-1/2 is
///   what shrinks that toward the ~5–15 budget).
/// * `per_hit_neighbor_cap` — bound each edge-kind per expanded hit.
/// * `max_spans` — hard cap on the total returned.
///
/// Edge ids that don't resolve in `by_id` are skipped (internal-only).
pub fn expand(
    records: &[Record],
    by_id: &BTreeMap<String, usize>,
    hit_ids: &[String],
    max_spans: usize,
    per_hit_neighbor_cap: usize,
    expand_top_n: usize,
) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    // Every hit is returned, in rank order.
    for id in hit_ids {
        if let Some(&idx) = by_id.get(id) {
            if seen.insert(id.clone()) {
                out.push(span_of(&records[idx], true));
            }
        }
    }

    // Only the top-N hits get their 1-hop neighbors (callees, then type-refs)
    // pulled — capped per hit — which is what holds the span budget.
    'hits: for id in hit_ids.iter().take(expand_top_n) {
        let Some(&idx) = by_id.get(id) else { continue };
        let e = &records[idx].edges;
        let neighbors = e
            .callees
            .iter()
            .take(per_hit_neighbor_cap)
            .chain(e.type_refs.iter().take(per_hit_neighbor_cap));
        for nid in neighbors {
            if out.len() >= max_spans {
                break 'hits;
            }
            if let Some(&nidx) = by_id.get(nid) {
                if seen.insert(nid.clone()) {
                    out.push(span_of(&records[nidx], false));
                }
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: &str, callees: &[&str], type_refs: &[&str]) -> Record {
        Record {
            id: id.to_string(),
            repo: "acme/x".to_string(),
            path: format!("src/{id}.rs"),
            symbol: id.to_string(),
            kind: "fn".to_string(),
            line_span: [1, 9],
            header: format!("fn {id}()"),
            body: format!("fn {id}() {{ /* ... */ }}"),
            edges: Edges {
                callees: callees.iter().map(|s| s.to_string()).collect(),
                type_refs: type_refs.iter().map(|s| s.to_string()).collect(),
            },
        }
    }

    fn index(records: &[Record]) -> BTreeMap<String, usize> {
        records
            .iter()
            .enumerate()
            .map(|(i, r)| (r.id.clone(), i))
            .collect()
    }

    #[test]
    fn hits_first_then_neighbors_deduped() {
        let records = vec![
            rec("a", &["b"], &["t"]),
            rec("b", &["a"], &[]), // back-edge to the hit → must dedupe, not re-add
            rec("t", &[], &[]),
            rec("z", &[], &[]),
        ];
        let by = index(&records);
        let out = expand(&records, &by, &["a".to_string()], 15, 5, 2);
        let ids: Vec<_> = out.iter().map(|s| s.symbol.as_str()).collect();
        assert_eq!(ids, ["a", "b", "t"]); // hit, then its callee + type-ref; z untouched
        assert!(out[0].hit && !out[1].hit && !out[2].hit);
    }

    #[test]
    fn respects_max_spans_and_per_hit_cap() {
        let records = vec![
            rec("h", &["c1", "c2", "c3", "c4"], &["r1", "r2"]),
            rec("c1", &[], &[]),
            rec("c2", &[], &[]),
            rec("c3", &[], &[]),
            rec("c4", &[], &[]),
            rec("r1", &[], &[]),
            rec("r2", &[], &[]),
        ];
        let by = index(&records);
        // per-hit cap 2 → ≤2 callees + ≤2 type-refs; max_spans 4 → hit + 3 neighbors.
        let out = expand(&records, &by, &["h".to_string()], 4, 2, 2);
        let ids: Vec<_> = out.iter().map(|s| s.symbol.as_str()).collect();
        assert_eq!(ids, ["h", "c1", "c2", "r1"]); // c3/c4 capped out; r2 cut by max_spans
    }

    #[test]
    fn expand_top_n_limits_which_hits_expand() {
        let records = vec![
            rec("a", &["x"], &[]),
            rec("b", &["y"], &[]),
            rec("x", &[], &[]),
            rec("y", &[], &[]),
        ];
        let by = index(&records);
        // Two hits; expand_top_n=1 → only the top hit (a) expands → x in, y out.
        let out = expand(&records, &by, &["a".to_string(), "b".to_string()], 15, 5, 1);
        let ids: Vec<_> = out.iter().map(|s| s.symbol.as_str()).collect();
        assert_eq!(ids, ["a", "b", "x"]); // both hits returned; only top-1 expanded
        assert!(out[0].hit && out[1].hit && !out[2].hit);
    }

    #[test]
    fn unresolvable_edges_are_dropped() {
        // Edges to external/missing ids (no record) must not appear (internal-only).
        let records = vec![rec("a", &["std::vec::Vec", "missing"], &["External"])];
        let by = index(&records);
        let out = expand(&records, &by, &["a".to_string()], 15, 5, 2);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].symbol, "a");
    }
}
