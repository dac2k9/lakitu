//! Code-index query — the lakitu side of the shared code "graph-brain"
//! (token-eff 8205bd, Track 1). Consumes the static jina index artifact
//! (`records.jsonl` + `vectors.npy` + `manifest.json` + jina ONNX) and answers
//! an NL query with the matching symbol-spans PLUS their precise 1-hop graph
//! neighbors — never whole files.
//!
//! Built bottom-up. Landed first (and the build's crux): the **precise
//! graph-expand**. A *loose* expand was measured to inject ~52 spans (≈ reading
//! the whole files) → no token savings; a *precise* 1-hop expand over real
//! call/type edges holds the budget far lower → net-positive. [`query`] wires
//! the full pipeline: embed (in-process ONNX, bit-matched to the reference
//! golden) → cosine search → precise expand → a staleness check against each
//! result's pinned source SHA. The MCP tool registration is the next step.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use ndarray::Array2;
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

/// One indexed symbol-span record — mirrors a line of the index `records.jsonl`.
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
///   (the Gate-1 pre-check: expanding all hits gave ~23 spans; the top-1/2 is
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

/// The index `manifest.json`: embedder + model/tokenizer paths (relative to the
/// artifact dir), dims/pooling for the embed guard, per-repo source SHAs, and
/// `by_symbol` (canonical `repo:path:symbol` → its window-chunk record ids, for
/// resolving an oversized symbol's full span).
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub embedder: String,
    pub onnx: String,
    pub tokenizer: String,
    pub dim: usize,
    pub pooling: String,
    pub max_len: usize,
    pub records: usize,
    pub repos: BTreeMap<String, String>,
    #[serde(default)]
    pub by_symbol: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub edges_method: String,
}

/// A loaded index: records + row-aligned jina vectors + manifest + an id→row
/// map. Vectors are raw (masked-mean, NOT unit-normalized — golden l2 ≈ 14.4),
/// so we keep per-row norms for cosine.
pub struct Artifact {
    pub records: Vec<Record>,
    pub vectors: Array2<f32>,
    pub norms: Vec<f32>,
    pub manifest: Manifest,
    pub by_id: BTreeMap<String, usize>,
}

impl Artifact {
    /// Load from a directory holding `records.jsonl` + `vectors.npy` +
    /// `manifest.json`. Guards row/dim alignment against the manifest.
    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        let manifest: Manifest =
            serde_json::from_reader(std::fs::File::open(dir.join("manifest.json"))?)?;

        let recs_txt = std::fs::read_to_string(dir.join("records.jsonl"))?;
        let mut records = Vec::with_capacity(manifest.records);
        for line in recs_txt.lines() {
            if line.trim().is_empty() {
                continue;
            }
            records.push(serde_json::from_str::<Record>(line)?);
        }

        let vectors: Array2<f32> = ndarray_npy::read_npy(dir.join("vectors.npy"))?;
        anyhow::ensure!(
            records.len() == vectors.nrows(),
            "records ({}) != vector rows ({})",
            records.len(),
            vectors.nrows()
        );
        anyhow::ensure!(
            vectors.ncols() == manifest.dim,
            "vector dim {} != manifest dim {}",
            vectors.ncols(),
            manifest.dim
        );

        let norms = vectors
            .rows()
            .into_iter()
            .map(|r| r.dot(&r).sqrt())
            .collect();
        // Record ids are NOT globally unique — cfg-duplicated (e.g. a
        // `#[cfg(unix)]` / `#[cfg(windows)]` pair) or windowed symbols share a
        // canonical `repo:path:symbol`. Keep the first occurrence; resolving a
        // hit/edge to ALL of a symbol's records (via `manifest.by_symbol`) is a
        // follow-up.
        let mut by_id: BTreeMap<String, usize> = BTreeMap::new();
        for (i, r) in records.iter().enumerate() {
            by_id.entry(r.id.clone()).or_insert(i);
        }
        Ok(Self {
            records,
            vectors,
            norms,
            manifest,
            by_id,
        })
    }

    /// Top-`k` record ids by cosine similarity to a raw jina query vector.
    pub fn search(&self, query: &[f32], k: usize) -> Vec<String> {
        let q = ndarray::ArrayView1::from(query);
        let qn = q.dot(&q).sqrt();
        let mut scored: Vec<(f32, usize)> = self
            .vectors
            .rows()
            .into_iter()
            .enumerate()
            .map(|(i, row)| {
                let denom = self.norms[i] * qn;
                let cos = if denom > 0.0 {
                    row.dot(&q) / denom
                } else {
                    0.0
                };
                (cos, i)
            })
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored
            .into_iter()
            .take(k)
            .map(|(_, i)| self.records[i].id.clone())
            .collect()
    }
}

impl Manifest {
    /// A staleness warning if `repo`'s pinned index SHA no longer matches its
    /// current HEAD (`current_sha`), else `None`. Handles a short/long SHA
    /// mismatch either direction (a prefix match still counts as fresh).
    ///
    /// The index is a full-rebuild snapshot with no incremental update path —
    /// this is the cheap half of the freshness requirement a stale index needs
    /// (surface staleness as a visible risk rather than silently trusting it).
    /// The other half — actually re-running the extractor on a schedule or a
    /// commit hook — is index-build-side follow-up, not this query path.
    pub fn staleness(&self, repo: &str, current_sha: &str) -> Option<String> {
        let pinned = self.repos.get(repo)?;
        let fresh = pinned == current_sha
            || pinned.starts_with(current_sha)
            || current_sha.starts_with(pinned.as_str());
        if fresh {
            None
        } else {
            Some(format!(
                "index for {repo} is pinned at {pinned}, current HEAD is {current_sha} — results may be stale"
            ))
        }
    }
}

/// In-process jina embedder — turns a raw NL query into the same 768-dim
/// vector space the index was built in. The pipeline is bit-matched to the
/// Python reference (`gate1_embed_reference.py`): tokenize (truncate 512) →
/// ONNX run → masked-mean pool over the sequence (NOT the CLS token, no L2
/// norm). A pooling/truncation mismatch here would silently break cosine
/// similarity against the index — `embed_matches_golden` (gated test) guards it.
pub struct Embedder {
    session: ort::session::Session,
    tokenizer: tokenizers::Tokenizer,
}

impl Embedder {
    pub fn load(onnx_path: &Path, tokenizer_path: &Path) -> anyhow::Result<Self> {
        let mut tokenizer = tokenizers::Tokenizer::from_file(tokenizer_path)
            .map_err(|e| anyhow::anyhow!("load tokenizer {}: {e}", tokenizer_path.display()))?;
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: 512,
                ..Default::default()
            }))
            .map_err(|e| anyhow::anyhow!("set truncation: {e}"))?;

        let session = ort::session::Session::builder()?.commit_from_file(onnx_path)?;
        Ok(Self { session, tokenizer })
    }

    /// Embed a raw NL query (no code-normalizer) into a 768-dim vector.
    pub fn embed(&mut self, query: &str) -> anyhow::Result<Vec<f32>> {
        let encoding = self
            .tokenizer
            .encode(query, true)
            .map_err(|e| anyhow::anyhow!("tokenize: {e}"))?;
        let ids: Vec<i64> = encoding.get_ids().iter().map(|&x| x as i64).collect();
        let mask: Vec<i64> = encoding
            .get_attention_mask()
            .iter()
            .map(|&x| x as i64)
            .collect();
        let seq_len = ids.len();
        anyhow::ensure!(seq_len > 0, "empty tokenization for query");

        let input_ids = ort::value::Tensor::from_array((vec![1i64, seq_len as i64], ids))?;
        let attention_mask =
            ort::value::Tensor::from_array((vec![1i64, seq_len as i64], mask.clone()))?;

        let outputs = self.session.run(ort::inputs![
            "input_ids" => input_ids,
            "attention_mask" => attention_mask,
        ])?;
        let (shape, hidden) = outputs["last_hidden_state"].try_extract_tensor::<f32>()?;
        anyhow::ensure!(
            shape.len() == 3 && shape[1] as usize == seq_len,
            "unexpected last_hidden_state shape {shape:?} for seq_len {seq_len}"
        );
        let dim = shape[2] as usize;

        // Masked mean-pool over the sequence axis — NOT the CLS token, no L2
        // norm (cosine normalizes at compare time).
        let mut pooled = vec![0f32; dim];
        let mut mask_sum = 0f32;
        for t in 0..seq_len {
            let m = mask[t] as f32;
            mask_sum += m;
            let row = &hidden[t * dim..(t + 1) * dim];
            for (d, v) in row.iter().enumerate() {
                pooled[d] += v * m;
            }
        }
        anyhow::ensure!(mask_sum > 0.0, "empty attention mask for query");
        for v in &mut pooled {
            *v /= mask_sum;
        }
        Ok(pooled)
    }
}

/// One query-tool result: the ranked hit + its precise graph-expand neighbors,
/// plus any staleness warnings for the repos those spans came from.
#[derive(Debug)]
pub struct QueryResult {
    pub spans: Vec<Span>,
    pub staleness_warnings: Vec<String>,
}

/// Tunable knobs for [`query`] — the levers the Gate-1 net-positive check
/// measured: `expand_top_n` (expand only the top-N primary hits, the lever
/// that shrinks a marginal net into a clean one), `per_hit_neighbor_cap`, and
/// `max_spans` (the hard total budget).
#[derive(Debug, Clone, Copy)]
pub struct QueryOpts {
    pub k: usize,
    pub max_spans: usize,
    pub per_hit_neighbor_cap: usize,
    pub expand_top_n: usize,
}

impl Default for QueryOpts {
    /// k=5 (the Gate-1 target); expand only the top-1 hit, ≤4 neighbors/kind,
    /// ≤15 spans total — the settings the net-positive pre-check validated.
    fn default() -> Self {
        Self {
            k: 5,
            max_spans: 15,
            per_hit_neighbor_cap: 4,
            expand_top_n: 1,
        }
    }
}

/// The full pipeline an MCP query tool would call: embed the NL query in the
/// index's own vector space, retrieve top-k, precisely expand the top hits'
/// 1-hop graph neighbors (capped per `opts`), and flag any result whose repo
/// has drifted past its pinned index SHA. `current_shas` (repo → current
/// HEAD) is caller-supplied — this module has no opinion on where a repo's
/// checkout lives on disk.
pub fn query(
    artifact: &Artifact,
    embedder: &mut Embedder,
    text: &str,
    opts: QueryOpts,
    current_shas: &BTreeMap<String, String>,
) -> anyhow::Result<QueryResult> {
    let qv = embedder.embed(text)?;
    let hit_ids = artifact.search(&qv, opts.k);
    let spans = expand(
        &artifact.records,
        &artifact.by_id,
        &hit_ids,
        opts.max_spans,
        opts.per_hit_neighbor_cap,
        opts.expand_top_n,
    );

    let mut staleness_warnings = Vec::new();
    let mut checked: BTreeSet<&str> = BTreeSet::new();
    for s in &spans {
        if checked.insert(s.repo.as_str()) {
            if let Some(sha) = current_shas.get(&s.repo) {
                if let Some(w) = artifact.manifest.staleness(&s.repo, sha) {
                    staleness_warnings.push(w);
                }
            }
        }
    }
    Ok(QueryResult {
        spans,
        staleness_warnings,
    })
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

    /// Integration test against the real artifact — skipped unless
    /// `LAKITU_TEST_ARTIFACT` points at a built artifact dir (so CI / other
    /// machines don't fail). Validates load + row-alignment + cosine via
    /// self-retrieval (a record's own vector must rank itself top-1).
    #[test]
    fn loads_real_artifact_when_present() {
        let Ok(dir) = std::env::var("LAKITU_TEST_ARTIFACT") else {
            eprintln!("skip: set LAKITU_TEST_ARTIFACT to a built artifact dir");
            return;
        };
        let art = Artifact::load(Path::new(&dir)).expect("load artifact");
        assert_eq!(art.records.len(), art.manifest.records);
        assert_eq!(art.vectors.ncols(), art.manifest.dim);
        // ids aren't globally unique (cfg-dup / windowed symbols share a
        // canonical id), so by_id may hold fewer entries than records.
        assert!(art.by_id.len() <= art.records.len());

        let probe = art.records.len() / 2;
        let qv: Vec<f32> = art.vectors.row(probe).to_vec();
        let top = art.search(&qv, 1);
        assert_eq!(
            top[0], art.records[probe].id,
            "a vector must retrieve itself"
        );
    }

    #[derive(Deserialize)]
    struct Golden {
        query: String,
        vector: Vec<f32>,
        l2_norm: f32,
    }

    /// Loads the jina ONNX model + tokenizer and confirms our Rust/ort
    /// pipeline matches the Python reference's golden vector — the guard
    /// against a silent pooling/truncation mismatch. Skipped unless
    /// `LAKITU_TEST_MODELS` (dir with model.onnx + tokenizer.json) and
    /// `LAKITU_TEST_GOLDEN` (the golden json) are set.
    #[test]
    fn embed_matches_golden() {
        let (Ok(models_dir), Ok(golden_path)) = (
            std::env::var("LAKITU_TEST_MODELS"),
            std::env::var("LAKITU_TEST_GOLDEN"),
        ) else {
            eprintln!("skip: set LAKITU_TEST_MODELS + LAKITU_TEST_GOLDEN to check embed parity");
            return;
        };
        let golden: Golden =
            serde_json::from_reader(std::fs::File::open(&golden_path).expect("open golden"))
                .expect("parse golden");

        let mut emb = Embedder::load(
            &Path::new(&models_dir).join("model.onnx"),
            &Path::new(&models_dir).join("tokenizer.json"),
        )
        .expect("load embedder");
        let v = emb.embed(&golden.query).expect("embed golden query");

        assert_eq!(v.len(), golden.vector.len(), "dim mismatch vs golden");
        let l2 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (l2 - golden.l2_norm).abs() < 1e-2,
            "l2 norm {l2} vs golden {}",
            golden.l2_norm
        );
        let dot: f32 = v.iter().zip(&golden.vector).map(|(a, b)| a * b).sum();
        let cos = dot / (l2 * golden.l2_norm);
        assert!(cos > 0.9999, "cosine to golden = {cos}, expected ~1.0");
    }

    /// End-to-end demo: embed a real NL query, search the real artifact,
    /// expand, print the returned spans. Skipped unless the artifact + model
    /// env vars are all set. This is the "does the prototype actually work"
    /// check — run with `--nocapture` to see the retrieved spans.
    #[test]
    fn end_to_end_demo_query() {
        let (Ok(art_dir), Ok(models_dir)) = (
            std::env::var("LAKITU_TEST_ARTIFACT"),
            std::env::var("LAKITU_TEST_MODELS"),
        ) else {
            eprintln!("skip: set LAKITU_TEST_ARTIFACT + LAKITU_TEST_MODELS for the e2e demo");
            return;
        };
        let art = Artifact::load(Path::new(&art_dir)).expect("load artifact");
        let mut emb = Embedder::load(
            &Path::new(&models_dir).join("model.onnx"),
            &Path::new(&models_dir).join("tokenizer.json"),
        )
        .expect("load embedder");

        let q = "how does the reconcile loop decide a shared task's next state";
        let result =
            query(&art, &mut emb, q, QueryOpts::default(), &BTreeMap::new()).expect("query");
        assert!(!result.spans.is_empty(), "expected at least one span back");

        eprintln!("query: {q}");
        for s in &result.spans {
            eprintln!(
                "  [{}] {}:{} {} L{}-{}",
                if s.hit { "hit" } else { "exp" },
                s.repo,
                s.path,
                s.symbol,
                s.line_span[0],
                s.line_span[1]
            );
        }
    }

    #[derive(Deserialize)]
    struct LabeledQuery {
        query: String,
    }

    /// Real net-positive measurement: for each query in the canonical labeled
    /// set, run the actual shipped pipeline and compare what it returns
    /// against the whole-file baseline it replaces — for the SAME files the
    /// tool's own answer lives in (a like-for-like comparison, not a recall
    /// measurement). Requires local checkouts of the indexed repos to read
    /// real file sizes (`LAKITU_TEST_REPO_ROOTS`, "repo=path,repo=path,...").
    /// Skipped unless every env var is set.
    #[test]
    fn net_check_vs_whole_file() {
        let (Ok(art_dir), Ok(models_dir), Ok(labeled_path), Ok(roots_spec)) = (
            std::env::var("LAKITU_TEST_ARTIFACT"),
            std::env::var("LAKITU_TEST_MODELS"),
            std::env::var("LAKITU_TEST_LABELED_SET"),
            std::env::var("LAKITU_TEST_REPO_ROOTS"),
        ) else {
            eprintln!(
                "skip: set LAKITU_TEST_ARTIFACT + LAKITU_TEST_MODELS + LAKITU_TEST_LABELED_SET \
                 + LAKITU_TEST_REPO_ROOTS (\"repo=path,repo=path\") for the net-check"
            );
            return;
        };
        let roots: BTreeMap<String, String> = roots_spec
            .split(',')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

        let art = Artifact::load(Path::new(&art_dir)).expect("load artifact");
        let mut emb = Embedder::load(
            &Path::new(&models_dir).join("model.onnx"),
            &Path::new(&models_dir).join("tokenizer.json"),
        )
        .expect("load embedder");

        let queries: Vec<String> = std::fs::read_to_string(&labeled_path)
            .expect("read labeled set")
            .lines()
            .filter_map(|l| serde_json::from_str::<LabeledQuery>(l).ok())
            .map(|q| q.query)
            .collect();
        assert!(
            queries.len() >= 10,
            "expected the real ~40-query labeled set, got {}",
            queries.len()
        );

        // Cache whole-file byte sizes so a file shared across queries/spans is
        // only stat'd once.
        let mut file_size_cache: BTreeMap<(String, String), u64> = BTreeMap::new();
        let mut file_size = |repo: &str, path: &str| -> Option<u64> {
            let key = (repo.to_string(), path.to_string());
            if let Some(&s) = file_size_cache.get(&key) {
                return Some(s);
            }
            let root = roots.get(repo)?;
            let size = std::fs::metadata(Path::new(root).join(path)).ok()?.len();
            file_size_cache.insert(key, size);
            Some(size)
        };

        let (mut total_index_bytes, mut total_whole_file_bytes, mut worse_count) =
            (0u64, 0u64, 0usize);
        let mut scored = 0usize;
        let mut ratio_sum = 0.0; // per-query mean, unweighted — a byte-heavy file
        let (mut ratio_min, mut ratio_max) = (f64::MAX, f64::MIN); // shouldn't dominate the story
        for q in &queries {
            let Ok(result) = query(&art, &mut emb, q, QueryOpts::default(), &BTreeMap::new())
            else {
                continue;
            };
            if result.spans.is_empty() {
                continue;
            }
            let index_bytes: u64 = result.spans.iter().map(|s| s.body.len() as u64).sum();
            let mut touched: BTreeSet<(String, String)> = BTreeSet::new();
            let mut whole_file_bytes = 0u64;
            let mut resolvable = true;
            for s in &result.spans {
                if touched.insert((s.repo.clone(), s.path.clone())) {
                    match file_size(&s.repo, &s.path) {
                        Some(sz) => whole_file_bytes += sz,
                        None => resolvable = false,
                    }
                }
            }
            if !resolvable {
                continue; // repo not in LAKITU_TEST_REPO_ROOTS — skip, don't guess
            }
            scored += 1;
            total_index_bytes += index_bytes;
            total_whole_file_bytes += whole_file_bytes;
            let ratio = 100.0 * index_bytes as f64 / whole_file_bytes as f64;
            ratio_sum += ratio;
            ratio_min = ratio_min.min(ratio);
            ratio_max = ratio_max.max(ratio);
            eprintln!("  {index_bytes:>6}B / {whole_file_bytes:>7}B = {ratio:>5.1}%  \"{q}\"");
            if index_bytes >= whole_file_bytes {
                worse_count += 1;
                eprintln!(
                    "  ⚠ WORSE than whole-file: \"{q}\" — index {index_bytes}B >= whole-file {whole_file_bytes}B"
                );
            }
        }

        assert!(
            scored > 0,
            "no query could be scored — check LAKITU_TEST_REPO_ROOTS"
        );
        let pct_of_whole_file = 100.0 * total_index_bytes as f64 / total_whole_file_bytes as f64;
        let ratio_mean = ratio_sum / scored as f64;
        eprintln!(
            "net-check: {scored} queries scored, {worse_count} worse-than-whole-file\n\
             total index bytes:      {total_index_bytes}\n\
             total whole-file bytes: {total_whole_file_bytes}\n\
             byte-weighted:  {pct_of_whole_file:.1}% of whole-file cost (~{:.0}% saved)\n\
             per-query mean: {ratio_mean:.1}% (range {ratio_min:.1}%–{ratio_max:.1}%)\n\
             rough token estimate (÷4): {} vs {}",
            100.0 - pct_of_whole_file,
            total_index_bytes / 4,
            total_whole_file_bytes / 4,
        );
    }
}
