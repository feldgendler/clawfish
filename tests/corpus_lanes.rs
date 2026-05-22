//! M6.H2 cross-lane independence tests.
//!
//! Each corpus lane (`SelfPlayOnBook`, `SelfPlayOffBook`, `Ccrl`,
//! `LichessOpen`) is an INDEPENDENT generator writing its own `lane.bin` in its
//! own dir. Per-lane FEN dedup is first-seen-WITHIN-the-lane; there is NO
//! cross-lane dedup. The same FEN may therefore appear in up to four lanes,
//! possibly with conflicting labels — explicitly accepted (the M6.I mixer
//! weighs lanes). These tests pin that contract by routing the same FEN through
//! two independent `LaneCommitter`s (two lane dirs) and asserting both retain it
//! with their own labels. See `docs/plans/m6.h2-corpus-lanes.md` §0/§1.1.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use clawfish::corpus::pipeline::LaneCommitter;
use clawfish::corpus::store::scan_valid_blocks;
use clawfish::corpus::{CorpusRecord, DEPTH_RUNG_EXTERNAL, Label, Source};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("clawfish-corpus-lanes-{tag}-{pid}-{n}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }
    fn lane(&self) -> PathBuf {
        self.0.join("lane.bin")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn rec(source: Source, label: Label, game_id: u64, ply: u32, fen: &str) -> CorpusRecord {
    CorpusRecord {
        fen: fen.to_string(),
        label,
        source,
        game_id,
        ply,
        depth_rung: DEPTH_RUNG_EXTERNAL,
        strata: 0,
    }
}

/// All records on a lane's disk, in block order.
fn lane_records(lane: &std::path::Path) -> Vec<CorpusRecord> {
    let (blocks, _) = scan_valid_blocks(lane).expect("scan");
    blocks.into_iter().flat_map(|b| b.records).collect()
}

const SHARED_FEN: &str = "r1bqkb1r/pppp1ppp/2n2n2/4p3/2B1P3/5N2/PPPP1PPP/RNBQK2R w KQkq - 4 4";

#[test]
fn same_fen_admitted_in_multiple_lanes_with_conflicting_labels() {
    // The SAME FEN is committed to two independent lanes (a self-play lane and a
    // fetch lane) with CONFLICTING labels. Per-lane dedup is within-lane only, so
    // BOTH lanes retain it — and each keeps its own label. This is the accepted
    // conflicting-label-across-lanes contract (the M6.I mixer's problem).
    let sp = TempDir::new("selfplay");
    let fetch = TempDir::new("fetch");

    // Self-play lane: the FEN labeled WhiteWin.
    let mut sp_committer = LaneCommitter::from_parts(HashSet::new(), 0, 7, None);
    let o_sp = sp_committer
        .commit_game(
            &sp.lane(),
            0,
            vec![rec(
                Source::SelfPlayOffBook,
                Label::WhiteWin,
                0,
                8,
                SHARED_FEN,
            )],
        )
        .unwrap();
    assert_eq!(o_sp.usable_committed, 1);

    // Fetch lane (independent dir + independent committer): the SAME FEN labeled
    // BlackWin (the conflict).
    let mut fetch_committer = LaneCommitter::from_parts(HashSet::new(), 0, 7, None);
    let o_fetch = fetch_committer
        .commit_game(
            &fetch.lane(),
            0,
            vec![rec(Source::LichessOpen, Label::BlackWin, 0, 8, SHARED_FEN)],
        )
        .unwrap();
    assert_eq!(
        o_fetch.usable_committed, 1,
        "the fetch lane retains the FEN despite the self-play lane already holding it \
         (no cross-lane dedup)"
    );

    let sp_recs = lane_records(&sp.lane());
    let fetch_recs = lane_records(&fetch.lane());
    assert_eq!(sp_recs.len(), 1, "self-play lane retains the FEN");
    assert_eq!(fetch_recs.len(), 1, "fetch lane retains the same FEN");
    assert_eq!(sp_recs[0].fen, fetch_recs[0].fen, "same FEN in both lanes");
    assert_eq!(sp_recs[0].label, Label::WhiteWin);
    assert_eq!(fetch_recs[0].label, Label::BlackWin);
    assert_ne!(
        sp_recs[0].label, fetch_recs[0].label,
        "the conflicting labels are BOTH preserved (M6.I mixer reconciles)"
    );
    assert_eq!(sp_recs[0].source, Source::SelfPlayOffBook);
    assert_eq!(fetch_recs[0].source, Source::LichessOpen);
}

#[test]
fn no_cross_lane_dedup() {
    // A committer is dedup-aware only of its OWN lane's `fen_set`. Committing the
    // same multiset of FENs through two independent committers (two lanes) yields
    // the SAME record count in each — the second is not dropped as a "duplicate"
    // of the first, because dedup never crosses lanes.
    let lane_a = TempDir::new("lane-a");
    let lane_b = TempDir::new("lane-b");

    let fens: Vec<String> = (0..6).map(|i| format!("cross-fen-{i} w - - 0 1")).collect();
    let recs_a: Vec<CorpusRecord> = fens
        .iter()
        .enumerate()
        .map(|(p, f)| rec(Source::Ccrl, Label::Draw, 0, p as u32, f))
        .collect();
    let recs_b = recs_a.clone();

    let mut ca = LaneCommitter::from_parts(HashSet::new(), 0, 0, None);
    let mut cb = LaneCommitter::from_parts(HashSet::new(), 0, 0, None);
    let oa = ca.commit_game(&lane_a.lane(), 0, recs_a).unwrap();
    let ob = cb.commit_game(&lane_b.lane(), 0, recs_b).unwrap();

    assert_eq!(oa.usable_committed, 6);
    assert_eq!(
        ob.usable_committed, 6,
        "lane B keeps all 6 FENs — none cross-lane-deduped against lane A"
    );
    assert_eq!(lane_records(&lane_a.lane()).len(), 6);
    assert_eq!(lane_records(&lane_b.lane()).len(), 6);
}
