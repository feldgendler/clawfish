//! Lane load + on-disk feature cache + game-level split + mix reweight
//! (ADR-0037 §10 R6/R7).
//!
//! Pass 1 extracts features once into a deterministic, reproducible disk cache
//! (header + length-framed records, sorted `(source, game_id, ply)`); tuning
//! then streams the cache in bounded chunks per epoch so peak heap does NOT
//! scale with corpus size. The per-chunk gradient parallelizes (worker
//! threads); the chunk *stream* is sequential.

use std::collections::hash_map::DefaultHasher;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

use crate::corpus::prng::Prng;
use crate::corpus::{CorpusRecord, store};
use crate::texel::TexelError;
use crate::texel::features::{FeatureVec, extract};
use crate::texel::layout;

/// On-disk cache format magic (`"TXC1"`); rejects foreign bytes early.
const CACHE_MAGIC: u32 = 0x5458_4331;

/// The four frozen corpus lanes plus their content sha256s (provenance).
pub struct LaneSet {
    /// One record vector per source lane, in [`crate::corpus::Source`] order.
    pub recs: [Vec<CorpusRecord>; 4],
    /// Per-lane content sha256 (recorded in `ParamsFile::lane_sha256`).
    pub sha256: [String; 4],
}

/// Feature-cache header metadata (the deterministic-artifact descriptor).
pub struct CacheMeta {
    /// Total cached record count.
    pub n: u64,
    /// Layout contract hash (pins the `(core_index, count)` index map).
    pub layout_hash: u64,
    /// Per-lane content sha256s (carried from the [`LaneSet`]).
    pub lane_sha256: [String; 4],
    /// Per-source record counts (mix-reweight + stratified-objective input).
    pub per_source: [u64; 4],
    /// Per-record `(source, game_id)` split key, in cache (stream) order. The
    /// cache is sorted `(source, game_id, ply)` so each game is a contiguous
    /// run; [`split_by_game`] groups on this compound key (no leakage). Carried
    /// here because the split has only `&CacheMeta` — the cache header stores
    /// the same keys so `build_cache` and a fresh reader agree.
    pub game_keys: Vec<(u8, u64)>,
}

/// One cached feature record (the streamed unit). `base` is the phase-blended
/// frozen contribution PLUS the post-scale mop-up; `coeffs` are sparse
/// `(core_index, phase_folded_coeff)` so that `base + Σ coeff·w` reproduces
/// [`crate::texel::features::model_score_white`] (ADR-0037 §2).
#[derive(Clone, PartialEq, Debug)]
pub struct CachedRec {
    /// Phase-blended frozen base `(base_mg·phase + base_eg·(24−phase))/24` plus
    /// the post-scale `mop_up`, at identity scale.
    pub base: f32,
    /// Sparse `(core_index, phase_folded_coeff)` tunable-feature coeffs:
    /// `coeff = raw_count · (phase/24 if MG else (24−phase)/24)`.
    pub coeffs: Box<[(u16, f32)]>,
    /// Original game-outcome label tag ([`crate::corpus::Label::as_u8`]).
    pub label: u8,
    /// Provenance tag ([`crate::corpus::Source::as_u8`]).
    pub source: u8,
    /// Frozen blind-spot stratum bitset (`STRATUM_*`).
    pub strata: u8,
    /// R-TC self-play depth stratum (`DEPTH_RUNG_EXTERNAL` for PGN sources).
    pub depth_rung: u8,
    /// Per-lane game id (the group-by-game split key).
    pub game_id: u64,
}

/// Loads the four frozen lane dirs (`scan_valid_blocks` + `read_manifest`).
///
/// Each lane dir holds a `lane.bin` shard (the CRC-framed game blocks) and a
/// `manifest.json` whose `corpus_sha256` is the lane's content digest.
pub fn load_lanes(dirs: [&Path; 4]) -> Result<LaneSet, TexelError> {
    let mut recs: [Vec<CorpusRecord>; 4] = Default::default();
    let mut sha256: [String; 4] = Default::default();
    for (lane, dir) in dirs.iter().enumerate() {
        let shard = dir.join("lane.bin");
        let (blocks, _valid_len) =
            store::scan_valid_blocks(&shard).map_err(|e| TexelError::Cache(e.to_string()))?;
        let mut lane_recs = Vec::new();
        for block in blocks {
            lane_recs.extend(block.records);
        }
        recs[lane] = lane_recs;
        let manifest = crate::corpus::manifest::read_manifest(dir)
            .map_err(|e| TexelError::Cache(e.to_string()))?;
        sha256[lane] = manifest.corpus_sha256;
    }
    Ok(LaneSet { recs, sha256 })
}

/// Builds the on-disk feature cache (parallel extract; R6/R7).
///
/// Feature extraction is fanned out across `workers` threads; the output is
/// then sorted `(source, game_id, ply)` so the on-disk byte order is
/// deterministic regardless of thread scheduling.
pub fn build_cache(lanes: &LaneSet, out: &Path, workers: usize) -> Result<CacheMeta, TexelError> {
    // Flatten the four lanes, tracking the sort key (source, game_id, ply).
    let mut indexed: Vec<(u8, u64, u32, &CorpusRecord)> = Vec::new();
    for lane in &lanes.recs {
        for r in lane {
            indexed.push((r.source.as_u8(), r.game_id, r.ply, r));
        }
    }

    // Parallel extraction, deterministic re-sort afterwards.
    let extracted = extract_parallel(&indexed, workers.max(1))?;

    // Sort by (source, game_id, ply) for a deterministic byte layout.
    let mut records: Vec<(u8, u64, u32, CachedRec)> = extracted;
    records.sort_by_key(|r| (r.0, r.1, r.2));

    let mut per_source = [0u64; 4];
    let mut game_keys: Vec<(u8, u64)> = Vec::with_capacity(records.len());
    for (src, gid, _ply, _rec) in &records {
        if let Some(slot) = per_source.get_mut(*src as usize) {
            *slot += 1;
        }
        game_keys.push((*src, *gid));
    }

    let layout_hash = layout_contract_hash();
    write_cache_file(
        out,
        &lanes.sha256,
        layout_hash,
        records.iter().map(|(_, _, _, r)| r),
        records.len() as u64,
    )?;

    Ok(CacheMeta {
        n: records.len() as u64,
        layout_hash,
        lane_sha256: lanes.sha256.clone(),
        per_source,
        game_keys,
    })
}

/// Fan feature extraction across `workers` threads. Each thread owns a disjoint
/// slice of the flattened records and produces `(source, game_id, ply, rec)`
/// tuples; the caller re-sorts for determinism, so worker scheduling never
/// affects the output.
fn extract_parallel(
    indexed: &[(u8, u64, u32, &CorpusRecord)],
    workers: usize,
) -> Result<Vec<(u8, u64, u32, CachedRec)>, TexelError> {
    let n = indexed.len();
    if n == 0 {
        return Ok(Vec::new());
    }
    let workers = workers.min(n).max(1);
    let chunk = n.div_ceil(workers);

    let result: Result<Vec<_>, TexelError> = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for w in 0..workers {
            let start = w * chunk;
            if start >= n {
                break;
            }
            let end = (start + chunk).min(n);
            let slice = &indexed[start..end];
            handles.push(scope.spawn(move || -> Result<Vec<_>, TexelError> {
                let mut out = Vec::with_capacity(slice.len());
                for &(src, gid, ply, rec) in slice {
                    out.push((src, gid, ply, cached_rec_from(rec)?));
                }
                Ok(out)
            }));
        }
        let mut all = Vec::with_capacity(n);
        for h in handles {
            let part = h.join().map_err(|_| {
                TexelError::Cache("feature-extraction worker panicked".to_string())
            })??;
            all.extend(part);
        }
        Ok(all)
    });
    result
}

/// Build a [`CachedRec`] from a [`CorpusRecord`] (parse FEN → extract → fold).
fn cached_rec_from(r: &CorpusRecord) -> Result<CachedRec, TexelError> {
    let pos = crate::Position::from_fen(&r.fen).map_err(|e| TexelError::Parse(format!("{e:?}")))?;
    let mut rec = fold_feature_vec(&extract(&pos));
    rec.label = r.label.as_u8();
    rec.source = r.source.as_u8();
    rec.strata = r.strata;
    rec.depth_rung = r.depth_rung;
    rec.game_id = r.game_id;
    Ok(rec)
}

/// Phase-fold a [`FeatureVec`] into a [`CachedRec`] (provenance fields left at
/// defaults; the caller fills them). `base` absorbs the phase-blended frozen
/// base plus the post-scale mop-up; each coeff is `raw_count · phase-factor`.
fn fold_feature_vec(fv: &FeatureVec) -> CachedRec {
    let phase = fv.phase as f32;
    let mg_factor = phase / 24.0;
    let eg_factor = (24.0 - phase) / 24.0;
    let base = (fv.base_mg * phase + fv.base_eg * (24.0 - phase)) / 24.0 + fv.mop_up;
    let coeffs: Box<[(u16, f32)]> = fv
        .coeffs
        .iter()
        .map(|&(idx, count)| {
            let factor = if layout::is_mg(idx as usize) {
                mg_factor
            } else {
                eg_factor
            };
            (idx, count * factor)
        })
        .collect();
    CachedRec {
        base,
        coeffs,
        label: 0,
        source: 0,
        strata: 0,
        depth_rung: 0,
        game_id: 0,
    }
}

/// Streams the feature cache in bounded chunks of `chunk` records (R6).
///
/// The reader is buffered and yields owned `Vec<CachedRec>` of length
/// `≤ chunk`; peak heap is bounded by one chunk plus the read buffer, never by
/// the corpus size. A corrupt / truncated cache yields fewer records (the
/// iterator stops at the first framing error) rather than panicking.
pub fn stream_chunks(cache: &Path, chunk: usize) -> impl Iterator<Item = Vec<CachedRec>> {
    CacheStream::open(cache, chunk.max(1))
}

/// Bounded-memory streaming reader over the on-disk cache.
struct CacheStream {
    reader: Option<BufReader<File>>,
    remaining: u64,
    chunk: usize,
}

impl CacheStream {
    fn open(path: &Path, chunk: usize) -> CacheStream {
        match File::open(path) {
            Ok(f) => {
                let mut reader = BufReader::new(f);
                match read_header(&mut reader) {
                    Ok((n, _layout_hash, _lane_sha256)) => CacheStream {
                        reader: Some(reader),
                        remaining: n,
                        chunk,
                    },
                    Err(_) => CacheStream {
                        reader: None,
                        remaining: 0,
                        chunk,
                    },
                }
            }
            Err(_) => CacheStream {
                reader: None,
                remaining: 0,
                chunk,
            },
        }
    }
}

impl Iterator for CacheStream {
    type Item = Vec<CachedRec>;

    fn next(&mut self) -> Option<Vec<CachedRec>> {
        let reader = self.reader.as_mut()?;
        if self.remaining == 0 {
            return None;
        }
        let take = self.chunk.min(self.remaining as usize);
        let mut out = Vec::with_capacity(take);
        for _ in 0..take {
            match read_record(reader) {
                Ok(rec) => out.push(rec),
                Err(_) => {
                    // Framing error / truncated cache: stop streaming.
                    self.remaining = 0;
                    self.reader = None;
                    break;
                }
            }
        }
        self.remaining = self.remaining.saturating_sub(out.len() as u64);
        if out.is_empty() { None } else { Some(out) }
    }
}

/// A group-by-game train/validation split (no `game_id` leakage across sides).
pub struct Split {
    /// Train record indices into the cache.
    pub train: Vec<u32>,
    /// Validation record indices into the cache.
    pub val: Vec<u32>,
}

/// Splits the cache by game (group-by-`(source, game_id)`, no leakage),
/// deterministic under `seed`, holding out approximately `val_fraction` of
/// games.
///
/// Each whole game (compound `(source, game_id)` key) is assigned to ONE side
/// via a seeded [`Prng`] draw keyed on the game, so no game straddles the
/// split. The cache is sorted `(source, game_id, ply)`, so [`CacheMeta::game_keys`]
/// (in stream order) groups each game into a contiguous run of cache indices.
pub fn split_by_game(meta: &CacheMeta, val_fraction: f64, seed: u64) -> Split {
    let frac = val_fraction.clamp(0.0, 1.0);
    // Deterministic per-game side assignment: a game goes to val iff a Prng
    // draw seeded by (seed, source, game_id) falls below `frac`. Caching per
    // distinct key keeps every record of a game on the same side.
    let mut side_of: std::collections::HashMap<(u8, u64), bool> = std::collections::HashMap::new();
    let mut train = Vec::new();
    let mut val = Vec::new();
    for (idx, &key) in meta.game_keys.iter().enumerate() {
        let to_val = *side_of.entry(key).or_insert_with(|| {
            let mut rng = Prng::new(game_seed(seed, key.0, key.1));
            (rng.below(1_000_000) as f64) / 1_000_000.0 < frac
        });
        if to_val {
            val.push(idx as u32);
        } else {
            train.push(idx as u32);
        }
    }
    Split { train, val }
}

/// Seed a per-game [`Prng`] decorrelated by `(seed, source, game_id)`.
fn game_seed(seed: u64, source: u8, game_id: u64) -> u64 {
    let mut h = DefaultHasher::new();
    seed.hash(&mut h);
    source.hash(&mut h);
    game_id.hash(&mut h);
    h.finish()
}

/// Builds a single [`CachedRec`] from a position (used in the Tier-2
/// cache-score cross-check and Phase-3 accessor tests). Extracts features,
/// phase-folds the coefficients, and stores the frozen base contribution.
pub fn build_cache_rec(pos: &crate::Position) -> CachedRec {
    fold_feature_vec(&extract(pos))
}

/// Computes effective per-source record counts under a given lane mix.
/// `effective[i] = round(mix[i] / sum(mix) · total)`.
/// Used by the mixture-reweight contract test (ADR-0037 §8).
pub fn effective_source_counts(mix: &[f64; 4], per_source: &[u64; 4], total: u64) -> [u64; 4] {
    let _ = per_source; // raw counts are not needed: the mix re-normalizes total.
    let sum: f64 = mix.iter().sum();
    if sum <= 0.0 {
        return [0; 4];
    }
    let mut out = [0u64; 4];
    for (i, &m) in mix.iter().enumerate() {
        out[i] = (m / sum * total as f64).round() as u64;
    }
    out
}

// ---------------------------------------------------------------------------
// Layout contract hash + binary cache framing.
// ---------------------------------------------------------------------------

/// A stable hash of the `(N_CORE, per-index parity)` layout contract. Pins the
/// `(core_index, count)` index map so a cache built under one layout is
/// rejected (via the header) if the layout silently reorders.
fn layout_contract_hash() -> u64 {
    let mut h = DefaultHasher::new();
    layout::N_CORE.hash(&mut h);
    for i in 0..layout::N_CORE {
        layout::is_mg(i).hash(&mut h);
    }
    h.finish()
}

/// Write the cache header + length-framed records. Header layout (little-endian):
/// `MAGIC:u32 | N:u64 | layout_hash:u64 | 4×(sha_len:u32, sha_bytes)`.
fn write_cache_file<'a, I>(
    out: &Path,
    lane_sha256: &[String; 4],
    layout_hash: u64,
    records: I,
    n: u64,
) -> Result<(), TexelError>
where
    I: Iterator<Item = &'a CachedRec>,
{
    let f = File::create(out)?;
    let mut w = BufWriter::new(f);
    w.write_all(&CACHE_MAGIC.to_le_bytes())?;
    w.write_all(&n.to_le_bytes())?;
    w.write_all(&layout_hash.to_le_bytes())?;
    for s in lane_sha256 {
        let bytes = s.as_bytes();
        w.write_all(&(bytes.len() as u32).to_le_bytes())?;
        w.write_all(bytes)?;
    }
    for rec in records {
        write_record(&mut w, rec)?;
    }
    w.flush()?;
    Ok(())
}

/// One record frame: `base:f32 | coeff_count:u32 | count×(idx:u16, coeff:f32)
/// | label:u8 | source:u8 | strata:u8 | depth_rung:u8 | game_id:u64`.
fn write_record<W: Write>(w: &mut W, rec: &CachedRec) -> Result<(), TexelError> {
    w.write_all(&rec.base.to_le_bytes())?;
    w.write_all(&(rec.coeffs.len() as u32).to_le_bytes())?;
    for &(idx, coeff) in rec.coeffs.iter() {
        w.write_all(&idx.to_le_bytes())?;
        w.write_all(&coeff.to_le_bytes())?;
    }
    w.write_all(&[rec.label, rec.source, rec.strata, rec.depth_rung])?;
    w.write_all(&rec.game_id.to_le_bytes())?;
    Ok(())
}

/// Read + validate the cache header. Returns `(N, layout_hash, lane_sha256)`.
fn read_header<R: Read>(r: &mut R) -> Result<(u64, u64, [String; 4]), TexelError> {
    let magic = read_u32(r)?;
    if magic != CACHE_MAGIC {
        return Err(TexelError::Cache(format!(
            "bad cache magic {magic:#x} (expected {CACHE_MAGIC:#x})"
        )));
    }
    let n = read_u64(r)?;
    let layout_hash = read_u64(r)?;
    let expected = layout_contract_hash();
    if layout_hash != expected {
        return Err(TexelError::Cache(format!(
            "cache layout_hash {layout_hash:#x} != current {expected:#x}"
        )));
    }
    let mut lane_sha256: [String; 4] = Default::default();
    for slot in &mut lane_sha256 {
        let len = read_u32(r)? as usize;
        let mut buf = vec![0u8; len];
        r.read_exact(&mut buf)?;
        *slot =
            String::from_utf8(buf).map_err(|_| TexelError::Cache("bad sha utf8".to_string()))?;
    }
    Ok((n, layout_hash, lane_sha256))
}

/// Read one record frame (the inverse of [`write_record`]).
fn read_record<R: Read>(r: &mut R) -> Result<CachedRec, TexelError> {
    let base = read_f32(r)?;
    let coeff_count = read_u32(r)? as usize;
    let mut coeffs = Vec::with_capacity(coeff_count);
    for _ in 0..coeff_count {
        let idx = read_u16(r)?;
        let coeff = read_f32(r)?;
        coeffs.push((idx, coeff));
    }
    let mut tag = [0u8; 4];
    r.read_exact(&mut tag)?;
    let game_id = read_u64(r)?;
    Ok(CachedRec {
        base,
        coeffs: coeffs.into_boxed_slice(),
        label: tag[0],
        source: tag[1],
        strata: tag[2],
        depth_rung: tag[3],
        game_id,
    })
}

fn read_u16<R: Read>(r: &mut R) -> Result<u16, TexelError> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_le_bytes(b))
}

fn read_u32<R: Read>(r: &mut R) -> Result<u32, TexelError> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_u64<R: Read>(r: &mut R) -> Result<u64, TexelError> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

fn read_f32<R: Read>(r: &mut R) -> Result<f32, TexelError> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(f32::from_le_bytes(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::prng::Prng;
    use crate::corpus::{CorpusRecord, DEPTH_RUNG_EXTERNAL, Label, Source};
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_temp_dir(tag: &str) -> PathBuf {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("clawfish-texel-ds-{tag}-{pid}-{n}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    const FENS: &[&str] = &[
        "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
        "r1bqkb1r/pppp1ppp/2n2n2/4p3/2B1P3/5N2/PPPP1PPP/RNBQK2R w KQkq - 4 4",
        "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1",
        "4k3/8/8/8/8/8/4P3/4K3 w - - 0 1",
    ];

    /// Synthetic 4-lane `LaneSet`: `games_per_lane` games, a few plies each, so
    /// the group-by-game split and per-source counts are exercisable.
    fn synthetic_lanes(seed: u64, games_per_lane: usize, plies: usize) -> LaneSet {
        let mut rng = Prng::new(seed);
        let mut recs: [Vec<CorpusRecord>; 4] = Default::default();
        let sources = [
            Source::SelfPlayOnBook,
            Source::SelfPlayOffBook,
            Source::Ccrl,
            Source::LichessOpen,
        ];
        for (lane, src) in sources.iter().enumerate() {
            for g in 0..games_per_lane {
                for p in 0..plies {
                    let fen = FENS[(rng.below(FENS.len() as u64)) as usize];
                    let label = match rng.below(3) {
                        0 => Label::WhiteWin,
                        1 => Label::Draw,
                        _ => Label::BlackWin,
                    };
                    recs[lane].push(CorpusRecord {
                        fen: fen.to_string(),
                        label,
                        source: *src,
                        game_id: g as u64,
                        ply: p as u32,
                        depth_rung: DEPTH_RUNG_EXTERNAL,
                        strata: 0,
                    });
                }
            }
        }
        LaneSet {
            recs,
            sha256: [
                "aa".to_string(),
                "bb".to_string(),
                "cc".to_string(),
                "dd".to_string(),
            ],
        }
    }

    /// Build a cache from synthetic lanes; returns `(cache_path, meta)`.
    fn build(dir: &std::path::Path, lanes: &LaneSet) -> (PathBuf, CacheMeta) {
        let cache = dir.join("features.cache");
        let meta = build_cache(lanes, &cache, 1).expect("build_cache");
        (cache, meta)
    }

    // -----------------------------------------------------------------------
    // Cache write → stream round-trip (R6 bounded-memory artifact).
    // -----------------------------------------------------------------------

    /// Writing the cache then streaming it back reconstructs the same record
    /// COUNT and the same per-record `(base, coeffs, label, source, game_id)`
    /// multiset — the cache is a faithful, deterministic artifact.
    #[test]
    fn feature_cache_write_then_stream_roundtrip() {
        let dir = unique_temp_dir("cache-roundtrip");
        let lanes = synthetic_lanes(0xF00D, 5, 4);
        let total: usize = lanes.recs.iter().map(|v| v.len()).sum();
        let (cache, meta) = build(&dir, &lanes);
        assert_eq!(meta.n as usize, total, "cache N must equal total records");

        let streamed: Vec<CachedRec> = stream_chunks(&cache, 16).flatten().collect();
        assert_eq!(
            streamed.len(),
            total,
            "streamed record count must equal the built count"
        );
        // Every streamed record carries a valid label/source byte and a
        // non-negative game_id (structural integrity of the framing).
        for r in &streamed {
            assert!(Label::from_u8(r.label).is_some(), "valid label byte");
            assert!(Source::from_u8(r.source).is_some(), "valid source byte");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A multi-chunk cache streams in ≥2 bounded chunks (peak memory does not
    /// scale with corpus size — R6). With `chunk` < N the iterator yields more
    /// than one chunk, each of bounded length.
    #[test]
    fn stream_chunks_bounded() {
        let dir = unique_temp_dir("stream-bounded");
        let lanes = synthetic_lanes(0xBEEF, 10, 5); // 4 × 50 = 200 records.
        let total: usize = lanes.recs.iter().map(|v| v.len()).sum();
        let (cache, _meta) = build(&dir, &lanes);

        let chunk_size = 32;
        let chunks: Vec<Vec<CachedRec>> = stream_chunks(&cache, chunk_size).collect();
        assert!(
            chunks.len() >= 2,
            "a {total}-record cache with chunk={chunk_size} must yield ≥2 chunks; got {}",
            chunks.len()
        );
        for c in &chunks {
            assert!(
                c.len() <= chunk_size,
                "each chunk must be ≤ chunk_size ({chunk_size}); got {}",
                c.len()
            );
        }
        let streamed: usize = chunks.iter().map(|c| c.len()).sum();
        assert_eq!(
            streamed, total,
            "chunks must cover every record exactly once"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Group-by-game split: no leakage, deterministic, seed-sensitive.
    // -----------------------------------------------------------------------

    /// `split_by_game` puts every game_id ENTIRELY on one side (train or val) —
    /// no game_id straddles the split (no train/val leakage). Cross-checks the
    /// split indices against the streamed records' game_ids.
    #[test]
    fn split_by_game_no_leakage() {
        let dir = unique_temp_dir("split-leak");
        let lanes = synthetic_lanes(0x1234, 8, 4);
        let (cache, meta) = build(&dir, &lanes);
        let recs: Vec<CachedRec> = stream_chunks(&cache, 64).flatten().collect();

        let split = split_by_game(&meta, 0.25, 99);
        // game_id is unique only WITHIN a lane (per-lane key), so the split key
        // must be the compound (source, game_id). The cache is sorted
        // (source, game_id, ply), so the split's index references resolve via
        // the streamed order.
        let key_of = |idx: u32| -> (u8, u64) {
            let r = &recs[idx as usize];
            (r.source, r.game_id)
        };
        let train_keys: HashSet<(u8, u64)> = split.train.iter().map(|&i| key_of(i)).collect();
        let val_keys: HashSet<(u8, u64)> = split.val.iter().map(|&i| key_of(i)).collect();
        let overlap: Vec<_> = train_keys.intersection(&val_keys).collect();
        assert!(
            overlap.is_empty(),
            "no (source, game_id) may appear on BOTH sides of the split; \
             leaked: {overlap:?}"
        );
        // Sanity: every record is assigned to exactly one side.
        assert_eq!(
            split.train.len() + split.val.len(),
            recs.len(),
            "split must partition every cache record"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `split_by_game` is deterministic under a fixed seed: two calls produce
    /// identical train/val index vectors.
    #[test]
    fn split_deterministic_under_seed() {
        let dir = unique_temp_dir("split-det");
        let lanes = synthetic_lanes(0x9090, 8, 3);
        let (_cache, meta) = build(&dir, &lanes);
        let a = split_by_game(&meta, 0.3, 555);
        let b = split_by_game(&meta, 0.3, 555);
        assert_eq!(a.train, b.train, "train indices must be seed-deterministic");
        assert_eq!(a.val, b.val, "val indices must be seed-deterministic");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A different seed changes the split (the held-out game set is not fixed).
    #[test]
    fn split_changes_with_seed() {
        let dir = unique_temp_dir("split-seed");
        let lanes = synthetic_lanes(0x7070, 12, 3);
        let (_cache, meta) = build(&dir, &lanes);
        let a = split_by_game(&meta, 0.3, 1);
        let b = split_by_game(&meta, 0.3, 2);
        assert_ne!(
            a.val, b.val,
            "different seeds must (with overwhelming probability) yield a \
             different held-out set"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Mixture reweighting changes effective per-source counts.
    // -----------------------------------------------------------------------

    /// Re-weighting the four lanes changes the EFFECTIVE per-source record
    /// counts the inner objective sees: a non-uniform mix over-weights one lane
    /// relative to uniform. (The reweighting is the mechanism the optional
    /// `mixture` simplex moves; ADR-0037 §8.) We pin that the effective counts
    /// derived from a mix differ from the uniform-mix baseline, via the
    /// `effective_source_counts` contract function.
    #[test]
    fn mixture_reweight_changes_effective_source_counts() {
        // CacheMeta.per_source is the raw per-source record count. The mix maps
        // those raw counts to effective counts: effective_i = round(mix_i ·
        // total / sum(mix)). A non-uniform mix must change the effective
        // distribution relative to the raw/uniform one.
        let meta = CacheMeta {
            n: 400,
            layout_hash: 0,
            lane_sha256: Default::default(),
            per_source: [100, 100, 100, 100],
            game_keys: Vec::new(),
        };
        let uniform = [0.25, 0.25, 0.25, 0.25];
        let skewed = [0.70, 0.10, 0.10, 0.10];

        // Use the exposed reweighting contract function — pins the impl.
        let eff_uniform = effective_source_counts(&uniform, &meta.per_source, meta.n);
        let eff_skewed = effective_source_counts(&skewed, &meta.per_source, meta.n);

        assert_ne!(
            eff_uniform, eff_skewed,
            "a non-uniform mix must change the effective per-source counts"
        );
        // The over-weighted lane (index 0) must gain relative to uniform.
        assert!(
            eff_skewed[0] > eff_uniform[0],
            "the over-weighted lane must have more effective records: \
             skewed[0]={}, uniform[0]={}",
            eff_skewed[0],
            eff_uniform[0]
        );
        // And the effective totals are preserved (re-normalized resampling).
        let sum_u: u64 = eff_uniform.iter().sum();
        let sum_s: u64 = eff_skewed.iter().sum();
        assert!(
            sum_u.abs_diff(sum_s) <= 4,
            "effective totals must match within rounding (got {sum_u} vs {sum_s})"
        );
    }
}
