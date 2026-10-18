//! Season simulation: the full pool-side loop across modules.
//!
//! A 30-day season with a phone loyalist (daily overnight shares), a
//! weekend desktop warrior (sporadic, high-rate), a comeback miner (15-day
//! gap then return), and a farm. Exercises streak accrual + reset, comeback
//! boost, overnight weighting, league classification, jackpot conservation,
//! league-pool drain, and audit replay — the whole doc 03 §3.3–§3.4 +
//! doc 06 §6.3 path in one deterministic story.

use lm_pool::league::DeviceClass;
use lm_pool::ledger::{Ledger, ShareRec, PPLNS_WINDOW_SECS};

const D: u64 = 86_400;

fn phone_share(day: u64) -> ShareRec {
    // ~5 MH/s phone: 5e6 H/s × 86400 s of nominal credit per daily share.
    ShareRec { miner: "m1".to_string(), ts: day * D + 3_600, work: 5_000_000 * D, overnight: true }
}

#[test]
fn season_rewards_loyalty_over_raw_rate() {
    let mut l = Ledger::new();
    // Miner 1: phone loyalist, every day 1..=30, overnight, modest rate.
    for day in 1..=30u64 {
        l.submit(phone_share(day));
    }
    // Miner 2: desktop warrior, 6 weekend days, 10x the per-share work.
    for day in [6, 7, 13, 14, 20, 21] {
        l.submit(ShareRec { miner: "m2".to_string(), ts: day * D + 3_600, work: 50_000_000 * D, overnight: false });
    }
    // Miner 3: comeback — days 1..2, silent 15 days, returns day 18..20.
    l.submit(ShareRec { miner: "m3".to_string(), ts: D, work: 5_000_000 * D, overnight: false });
    l.submit(ShareRec { miner: "m3".to_string(), ts: 2 * D, work: 5_000_000 * D, overnight: false });
    for day in 18..=20u64 {
        l.submit(ShareRec { miner: "m3".to_string(), ts: day * D, work: 5_000_000 * D, overnight: false });
    }

    let now = 30 * D + 7_200;
    // 3-day PPLNS window holds only the loyalist's recent shares; the
    // warrior (last active day 21) and the comeback miner (last day 20)
    // have nothing in window — retention data, not payout data, for them.
    let agg = l.aggregate(PPLNS_WINDOW_SECS, now);
    assert_eq!(agg.len(), 1);

    // Loyalist: 30-day streak capped at +300‰, phone class.
    let loyal = &agg[0];
    assert_eq!((loyal.miner.clone(), loyal.streak_days, loyal.streak_per_mille), ("m1".to_string(), 30, 300));
    assert_eq!(loyal.class, DeviceClass::Phone);
    // 3 window shares × 5e6·D × 1.05 overnight × 1.30 streak.
    let expect_loyal = 3 * 5_000_000 * D * 105 / 100 * 130 / 100;
    assert_eq!(loyal.eff_work, expect_loyal);

    // Season-wide view: lapsed streaks read 0 (gaps far past 48 h).
    let wide = l.aggregate(30 * D, now);
    assert_eq!(wide.len(), 3);
    assert_eq!(wide[1].streak_days, 0); // warrior, silent since day 21
    assert_eq!(wide[2].streak_days, 0); // comeback miner, silent since day 20
    // The comeback miner's post-return shares still carry the +100‰ boost:
    // days 1–2 plain (2 × 4.32e11), days 18–20 boosted (3 × 4.32e11 × 1.1).
    assert_eq!(wide[2].eff_work, 2 * 5_000_000 * D + 3 * 5_000_000 * D * 110 / 100);

    // Jackpot over a season-long window: all three share the bonus, total conserved.
    let pays = l.jackpot_payout(9_000_000, 30 * D, now);
    assert_eq!(pays.iter().map(|(_, p)| p).sum::<u64>(), 9_000_000);
    assert_eq!(pays.len(), 3);
}

#[test]
fn comeback_boost_lands_in_window() {
    let mut l = Ledger::new();
    // Miner 7: one share day 1, returns day 20 (19 idle days), mines 20..22.
    l.submit(ShareRec { miner: "m7".to_string(), ts: D, work: 1000, overnight: false });
    for day in 20..=22u64 {
        l.submit(ShareRec { miner: "m7".to_string(), ts: day * D, work: 1000, overnight: false });
    }
    // Miner 8: same raw work, no gap (days 20..22 only, first share day 20).
    for day in 20..=22u64 {
        l.submit(ShareRec { miner: "m8".to_string(), ts: day * D, work: 1000, overnight: false });
    }
    let now = 22 * D + 100;
    let agg = l.aggregate(30 * D, now);
    // Miner 7: day-1 share plain (1000) + three post-return shares at +100‰
    // (3 × 1100), all × 3-day streak 1.03 → 4300 × 1.03 = 4429.
    // Miner 8: same recent raw, no gap, no boost → 3000 × 1.03 = 3090.
    assert_eq!(agg[0].eff_work, 4429);
    assert_eq!(agg[1].eff_work, 3090);
    // Narrow to the return window: identical recent raw (3000 each), yet the
    // returner still earns more (3399 vs 3090) — the boost isolated.
    let recent = l.aggregate(3 * D, now);
    assert_eq!(recent[0].eff_work, 3399);
    assert_eq!(recent[1].eff_work, 3090);
}

#[test]
fn leaderboard_ranks_and_flags_farm() {
    let mut l = Ledger::new();
    for day in 28..=30u64 {
        l.submit(phone_share(day)); // miner 1, eff ≈ 3 × 5e6·D × 1.05 × 1.03
        l.submit(ShareRec { miner: "m2".to_string(), ts: day * D, work: 100, overnight: false });
        l.submit(ShareRec {
            miner: "m9".to_string(),
            ts: day * D,
            work: 600_000_000 * D, // farm rate, 3 straight days
            overnight: false,
        });
    }
    let now = 30 * D + 7_200;
    let board = l.leaderboard(PPLNS_WINDOW_SECS, now);
    assert_eq!(board.len(), 3);
    assert_eq!(board[0].miner, "m9");
    assert_eq!(board[0].class, DeviceClass::Farm);
    assert_eq!(board[1].miner, "m1");
    assert_eq!(board[1].class, DeviceClass::Phone);
    assert_eq!(board[2].miner, "m2");
    // Effective work strictly descends down the board.
    assert!(board[0].eff_work > board[1].eff_work);
    assert!(board[1].eff_work > board[2].eff_work);

    // League pool: farm excluded, phone + small fry split it all.
    l.credit_league(1_000_000);
    let pays = l.split_league_pool(PPLNS_WINDOW_SECS, now);
    // Miner 1 holds ~99.9999% of eligible work: takes all but dust math —
    // miner 2's 309 units earn largest-remainder nothing extra here.
    assert_eq!(pays.iter().map(|(_, p)| p).sum::<u64>(), 1_000_000);
    assert!(pays.iter().all(|(m, _)| m != "m9"));
    assert_eq!(pays[0], ("m1".to_string(), 1_000_000));
    assert_eq!(l.league_balance(), 0);
}

#[test]
fn audit_replay_recomputes_identically() {
    // Rebuild the same ledger twice (different insertion order) and confirm
    // every query agrees — pool payouts must be replayable from the log.
    let mut a = Ledger::new();
    let mut b = Ledger::new();
    let recs = vec![
        phone_share(28),
        ShareRec { miner: "m2".to_string(), ts: 29 * D, work: 700, overnight: true },
        phone_share(29),
        ShareRec { miner: "m2".to_string(), ts: 30 * D, work: 700, overnight: false },
        phone_share(30),
    ];
    for r in &recs {
        a.submit(r.clone());
    }
    for r in recs.iter().rev() {
        b.submit(r.clone());
    }
    let now = 30 * D + 7_200;
    assert_eq!(a.aggregate(PPLNS_WINDOW_SECS, now), b.aggregate(PPLNS_WINDOW_SECS, now));
    assert_eq!(
        a.jackpot_payout(500_000, PPLNS_WINDOW_SECS, now),
        b.jackpot_payout(500_000, PPLNS_WINDOW_SECS, now)
    );
    assert_eq!(a.leaderboard(PPLNS_WINDOW_SECS, now), b.leaderboard(PPLNS_WINDOW_SECS, now));
}
