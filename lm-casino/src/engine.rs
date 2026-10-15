//! Casino engine (doc 14): players, provably-fair sessions, both games, and
//! settlement against the house ledger.
//!
//! A spin is: place the bet (exposure-capped) → derive the outcome from the
//! player's committed server seed + client seed + nonce → compute the payout
//! with the game's math → settle the win. The engine exposes [`Casino::replay`]
//! and [`Casino::outcome_win`] so anyone can verify a spin from the revealed
//! seed, which is the product's core trust claim.

use std::collections::BTreeMap;
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::edge::EdgeSplit;
use crate::fair::{self, Fair};
use crate::ledger::{CasError, Ledger};
use crate::line::LineMath;
use crate::ways::WaysMath;
use crate::{MICROS_PER_LKYL, TARGET_RTP};

/// The two launch titles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Game {
    /// Book-style, 5×3, 10 lines.
    LuckyLode,
    /// Ways-style, 6 reels, up to 117,649 ways.
    LuckyLodeWays,
}

impl Game {
    /// Stable wire/API name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Game::LuckyLode => "lucky-lode",
            Game::LuckyLodeWays => "lucky-lode-ways",
        }
    }

    /// Parse a wire/API name.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "lucky-lode" | "line" | "lucky_lode" => Some(Game::LuckyLode),
            "lucky-lode-ways" | "ways" | "lucky_lode_ways" => Some(Game::LuckyLodeWays),
            _ => None,
        }
    }
}

/// Engine error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CasinoError {
    /// No such player (call `/api/deposit` or ensure first).
    UnknownPlayer,
    /// Ledger error (funds/exposure).
    Cas(CasError),
    /// Bad parameters (zero bet, non-divisible line bet, bad seed).
    BadBet(&'static str),
    /// The free FUN allowance is on cooldown until the given unix time.
    FunCooldown(u64),
}

impl From<CasError> for CasinoError {
    fn from(e: CasError) -> Self {
        CasinoError::Cas(e)
    }
}

impl fmt::Display for CasinoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CasinoError::UnknownPlayer => write!(f, "unknown player"),
            CasinoError::Cas(e) => write!(f, "{e:?}"),
            CasinoError::BadBet(m) => write!(f, "bad bet: {m}"),
            CasinoError::FunCooldown(t) => write!(f, "FUN claim on cooldown until {t}"),
        }
    }
}

/// A player's provably-fair session and lifetime counters.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Player {
    pub id: u64,
    /// Secret server seed (hex); revealed on rotation.
    pub server_seed: String,
    /// Published commitment = SHA-256(server seed bytes), hex.
    pub commitment: String,
    pub client_seed: String,
    pub nonce: u64,
    pub spins: u64,
    pub wagered: u64,
    pub won: u64,
    /// FUN play-money balance (free credits; never on-chain, never
    /// convertible, never withdrawable).
    #[serde(default)]
    pub fun: u64,
    /// FUN spins played.
    #[serde(default)]
    pub fun_spins: u64,
    /// FUN wagered (play money; excluded from every house accounting).
    #[serde(default)]
    pub fun_wagered: u64,
    /// FUN won.
    #[serde(default)]
    pub fun_won: u64,
    /// Lifetime FUN claimed.
    #[serde(default)]
    pub fun_claimed: u64,
    /// Unix time of the last FUN claim (0 = never claimed).
    #[serde(default)]
    pub fun_last_claim: u64,
    /// The most recent spin (for the client/verifier).
    pub last: Option<SpinRecord>,
    /// The most recent FUN spin (verifiable the same way).
    #[serde(default)]
    pub fun_last: Option<SpinRecord>,
}

/// Result of a free FUN claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FunClaim {
    /// FUN granted by this claim.
    pub granted: u64,
    /// Player's FUN balance after the claim.
    pub balance: u64,
    /// Unix time when the next claim becomes available.
    pub next_claim_at: u64,
}

/// A settled spin. Contains no secret: the commitment is public, and the
/// server seed is only revealed on rotation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SpinRecord {
    pub game: Game,
    pub nonce: u64,
    pub client_seed: String,
    pub server_seed_hash: String,
    /// Reel positions (line: one stop per reel; ways: rows-per-reel).
    pub stops: Vec<u64>,
    /// Visible symbols per reel (line: one each; ways: the full reel).
    pub symbols: Vec<Vec<usize>>,
    pub bet: u64,
    pub win: u64,
    /// Scatters landed (line game only).
    pub scatters: usize,
}

/// The casino: house ledger, tuned math, and player sessions.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Casino {
    pub ledger: Ledger,
    pub line: LineMath,
    pub ways: WaysMath,
    pub players: BTreeMap<u64, Player>,
    seed_counter: u64,
    pub total_spins: u64,
    /// Casino-wide FUN (play-money) entertainment counters — never part of
    /// the house ledger, GGR or edge.
    #[serde(default)]
    pub total_fun_spins: u64,
    #[serde(default)]
    pub total_fun_wagered: u64,
    #[serde(default)]
    pub total_fun_won: u64,
}

impl Casino {
    /// New casino with a seeded bankroll and the default edge split. Both
    /// titles are tuned to [`TARGET_RTP`].
    #[must_use]
    pub fn new(bankroll_micros: u64) -> Self {
        let mut line = LineMath::default();
        line.tune_to_target(TARGET_RTP);
        let mut ways = WaysMath::default();
        ways.tune_to_target(TARGET_RTP);
        let max_win = crate::line::MAX_WIN_MULTIPLE.max(crate::ways::MAX_WIN_MULTIPLE_WAYS);
        Self {
            ledger: Ledger::new(bankroll_micros, EdgeSplit::default(), max_win),
            line,
            ways,
            players: BTreeMap::new(),
            seed_counter: 0,
            total_spins: 0,
            total_fun_spins: 0,
            total_fun_wagered: 0,
            total_fun_won: 0,
        }
    }

    /// House bankroll in micro-LKYL.
    #[must_use]
    pub fn bankroll(&self) -> u64 {
        self.ledger.bankroll()
    }

    /// Largest total bet allowed by the exposure cap.
    #[must_use]
    pub fn max_bet(&self) -> u64 {
        self.ledger.max_bet()
    }

    /// Player balance in micro-LKYL.
    #[must_use]
    pub fn balance(&self, player: u64) -> u64 {
        self.ledger.balance(player)
    }

    /// Create the player with a fresh committed seed if not present.
    pub fn ensure_player(&mut self, player: u64) -> &Player {
        if !self.players.contains_key(&player) {
            let seed = self.fresh_seed(player);
            let seed_bytes = fair::from_hex(&seed).expect("hex from to_hex");
            let commitment = fair::to_hex(&fair::commitment(&seed_bytes));
            self.players.insert(
                player,
                Player {
                    id: player,
                    server_seed: seed,
                    commitment,
                    client_seed: format!("lucky-{player}"),
                    nonce: 0,
                    spins: 0,
                    wagered: 0,
                    won: 0,
                    fun: 0,
                    fun_spins: 0,
                    fun_wagered: 0,
                    fun_won: 0,
                    fun_claimed: 0,
                    fun_last_claim: 0,
                    last: None,
                    fun_last: None,
                },
            );
        }
        self.players.get(&player).expect("just inserted")
    }

    /// Set a player's client seed (takes effect for their next spin).
    ///
    /// # Errors
    /// [`CasinoError::UnknownPlayer`].
    pub fn set_client_seed(&mut self, player: u64, seed: &str) -> Result<(), CasinoError> {
        let p = self
            .players
            .get_mut(&player)
            .ok_or(CasinoError::UnknownPlayer)?;
        p.client_seed = seed.to_string();
        Ok(())
    }

    /// Rotate the server seed: the **old seed is returned (revealed)** and a
    /// fresh committed seed starts a new session at nonce 0.
    ///
    /// # Errors
    /// [`CasinoError::UnknownPlayer`].
    pub fn rotate(&mut self, player: u64) -> Result<String, CasinoError> {
        let counter = self.next_counter();
        let fresh = self.seed_from_entropy(player, counter);
        let fresh_bytes = fair::from_hex(&fresh).expect("hex from to_hex");
        let fresh_commitment = fair::to_hex(&fair::commitment(&fresh_bytes));
        let p = self
            .players
            .get_mut(&player)
            .ok_or(CasinoError::UnknownPlayer)?;
        let revealed = std::mem::replace(&mut p.server_seed, fresh);
        p.commitment = fresh_commitment;
        p.nonce = 0;
        Ok(revealed)
    }

    /// Spin `game` for `bet` total micro-LKYL.
    ///
    /// # Errors
    /// [`CasinoError`] for unknown players, zero/oversized bets, or a line bet
    /// not divisible by the line count.
    pub fn spin(&mut self, player: u64, game: Game, bet: u64) -> Result<SpinRecord, CasinoError> {
        let (seed, client_seed, nonce) = self.spin_material(player, game, bet)?;
        self.ledger.place_bet(player, bet)?;
        let (symbols, stops, scatters, win) = self.compute(game, &seed, &client_seed, nonce, bet);
        self.ledger.settle(player, win)?;
        Ok(self.commit(
            player, false, game, bet, &seed, &client_seed, nonce, symbols, stops, scatters, win,
        ))
    }

    /// Spin `game` for `bet` FUN (free play money). Identical provably-fair
    /// derivation and math as [`Casino::spin`]; the bet and win move only the
    /// player's FUN balance — the house ledger, bankroll, GGR and edge are
    /// never touched, and FUN can never be deposited or withdrawn.
    ///
    /// # Errors
    /// [`CasinoError`] for unknown players, zero bets, a line bet not
    /// divisible by the line count, a bet over [`FUN_MAX_BET`], or an
    /// insufficient FUN balance.
    pub fn spin_fun(
        &mut self,
        player: u64,
        game: Game,
        bet: u64,
    ) -> Result<SpinRecord, CasinoError> {
        if bet > FUN_MAX_BET {
            return Err(CasinoError::BadBet("FUN bet over the cap"));
        }
        let (seed, client_seed, nonce) = self.spin_material(player, game, bet)?;
        {
            let p = self
                .players
                .get_mut(&player)
                .expect("checked by spin_material");
            if p.fun < bet {
                return Err(CasinoError::Cas(CasError::InsufficientFunds));
            }
            p.fun -= bet;
        }
        let (symbols, stops, scatters, win) = self.compute(game, &seed, &client_seed, nonce, bet);
        self.players.get_mut(&player).expect("checked").fun += win;
        Ok(self.commit(
            player, true, game, bet, &seed, &client_seed, nonce, symbols, stops, scatters, win,
        ))
    }

    /// Claim the free FUN allowance. The first claim is always granted;
    /// afterwards claims are spaced by `cooldown` seconds.
    ///
    /// # Errors
    /// [`CasinoError::UnknownPlayer`], [`CasinoError::BadBet`] on a zero
    /// grant, or [`CasinoError::FunCooldown`] (carrying the next allowed
    /// unix time) while the cooldown holds.
    pub fn claim_fun(
        &mut self,
        player: u64,
        now: u64,
        grant: u64,
        cooldown: u64,
    ) -> Result<FunClaim, CasinoError> {
        if grant == 0 {
            return Err(CasinoError::BadBet("zero FUN grant"));
        }
        let p = self
            .players
            .get_mut(&player)
            .ok_or(CasinoError::UnknownPlayer)?;
        let next_at = p.fun_last_claim.saturating_add(cooldown);
        if p.fun_last_claim != 0 && now < next_at {
            return Err(CasinoError::FunCooldown(next_at));
        }
        p.fun += grant;
        p.fun_claimed += grant;
        p.fun_last_claim = now;
        Ok(FunClaim {
            granted: grant,
            balance: p.fun,
            next_claim_at: now.saturating_add(cooldown),
        })
    }

    /// The player's FUN balance (play money).
    #[must_use]
    pub fn fun_balance(&self, player: u64) -> u64 {
        self.players.get(&player).map(|p| p.fun).unwrap_or(0)
    }

    /// Validate the bet and derive the session material for a spin.
    fn spin_material(
        &self,
        player: u64,
        game: Game,
        bet: u64,
    ) -> Result<([u8; fair::HASH_LEN], String, u64), CasinoError> {
        let p = self
            .players
            .get(&player)
            .ok_or(CasinoError::UnknownPlayer)?;
        if bet == 0 {
            return Err(CasinoError::BadBet("zero bet"));
        }
        if game == Game::LuckyLode && !bet.is_multiple_of(self.line.lines) {
            return Err(CasinoError::BadBet("bet must be a multiple of lines"));
        }
        let seed_vec =
            fair::from_hex(&p.server_seed).map_err(|_| CasinoError::BadBet("bad seed"))?;
        if seed_vec.len() != fair::HASH_LEN {
            return Err(CasinoError::BadBet("seed length"));
        }
        let mut seed = [0u8; fair::HASH_LEN];
        seed.copy_from_slice(&seed_vec);
        Ok((seed, p.client_seed.clone(), p.nonce))
    }

    /// Derive the visible outcome + payout for one (seed, client seed, nonce).
    fn compute(
        &self,
        game: Game,
        seed: &[u8; fair::HASH_LEN],
        client_seed: &str,
        nonce: u64,
        bet: u64,
    ) -> (Vec<Vec<usize>>, Vec<u64>, usize, u64) {
        let mut f = Fair::new(*seed, client_seed.as_bytes(), nonce);
        match game {
            Game::LuckyLode => {
                let counts = self.line.reel_strip_lengths();
                let stops = f.next_reel_stops(&counts);
                let flat: Vec<usize> = stops
                    .iter()
                    .enumerate()
                    .map(|(r, s)| self.line.symbol_at(r, *s))
                    .collect();
                let out = self.line.evaluate(&flat);
                let win = self.line.payout_micros(&flat, bet / self.line.lines);
                let symbols: Vec<Vec<usize>> = flat.iter().map(|s| vec![*s]).collect();
                (symbols, stops, out.scatters, win)
            }
            Game::LuckyLodeWays => {
                let counts = self.ways.reel_symbol_counts();
                let mut reels: Vec<Vec<usize>> = Vec::with_capacity(counts.len());
                let mut stops: Vec<u64> = Vec::with_capacity(counts.len());
                for c in &counts {
                    let rows = 2 + f.next_index(6);
                    stops.push(rows);
                    let mut reel = Vec::with_capacity(rows as usize);
                    for _ in 0..rows {
                        reel.push(f.next_index(*c) as usize);
                    }
                    reels.push(reel);
                }
                let win = self.ways.payout_micros(&reels, bet);
                (reels, stops, 0, win)
            }
        }
    }

    /// Record a settled spin: advance the shared RNG cursor and the
    /// per-mode counters, and return the public record.
    #[allow(clippy::too_many_arguments)]
    fn commit(
        &mut self,
        player: u64,
        fun: bool,
        game: Game,
        bet: u64,
        seed: &[u8; fair::HASH_LEN],
        client_seed: &str,
        nonce: u64,
        symbols: Vec<Vec<usize>>,
        stops: Vec<u64>,
        scatters: usize,
        win: u64,
    ) -> SpinRecord {
        let record = SpinRecord {
            game,
            nonce,
            client_seed: client_seed.to_string(),
            server_seed_hash: fair::to_hex(&fair::commitment(seed)),
            stops,
            symbols,
            bet,
            win,
            scatters,
        };
        let p = self.players.get_mut(&player).expect("checked above");
        // Real and FUN spins share one nonce cursor so every (seed, nonce)
        // pair stays unique across both modes.
        p.nonce += 1;
        if fun {
            p.fun_spins += 1;
            p.fun_wagered += bet;
            p.fun_won += win;
            p.fun_last = Some(record.clone());
            self.total_fun_spins += 1;
            self.total_fun_wagered += bet;
            self.total_fun_won += win;
        } else {
            p.spins += 1;
            p.wagered += bet;
            p.won += win;
            p.last = Some(record.clone());
            self.total_spins += 1;
        }
        record
    }

    /// The player's public session state.
    ///
    /// # Errors
    /// [`CasinoError::UnknownPlayer`].
    pub fn player(&self, player: u64) -> Result<&Player, CasinoError> {
        self.players.get(&player).ok_or(CasinoError::UnknownPlayer)
    }

    /// Recompute a spin's visible symbols from a revealed server seed — the
    /// player-side verifier.
    ///
    /// # Errors
    /// `Err` on a malformed seed.
    pub fn replay(
        &self,
        game: Game,
        server_seed_hex: &str,
        client_seed: &str,
        nonce: u64,
    ) -> Result<Vec<Vec<usize>>, String> {
        let v = fair::from_hex(server_seed_hex)?;
        if v.len() != fair::HASH_LEN {
            return Err("seed must be 32 bytes".to_string());
        }
        let mut seed = [0u8; fair::HASH_LEN];
        seed.copy_from_slice(&v);
        let mut f = Fair::new(seed, client_seed.as_bytes(), nonce);
        Ok(match game {
            Game::LuckyLode => {
                let counts = self.line.reel_strip_lengths();
                let stops = f.next_reel_stops(&counts);
                stops
                    .iter()
                    .enumerate()
                    .map(|(r, s)| vec![self.line.symbol_at(r, *s)])
                    .collect()
            }
            Game::LuckyLodeWays => {
                let counts = self.ways.reel_symbol_counts();
                let mut reels = Vec::with_capacity(counts.len());
                for c in &counts {
                    let rows = 2 + f.next_index(6);
                    let mut reel = Vec::with_capacity(rows as usize);
                    for _ in 0..rows {
                        reel.push(f.next_index(*c) as usize);
                    }
                    reels.push(reel);
                }
                reels
            }
        })
    }

    /// Recompute a spin's win (micro-LKYL) from its visible symbols.
    #[must_use]
    pub fn outcome_win(&self, game: Game, symbols: &[Vec<usize>], bet: u64) -> u64 {
        match game {
            Game::LuckyLode => {
                let flat: Vec<usize> = symbols.iter().map(|v| v[0]).collect();
                self.line.payout_micros(&flat, bet / self.line.lines)
            }
            Game::LuckyLodeWays => self.ways.payout_micros(symbols, bet),
        }
    }

    fn next_counter(&mut self) -> u64 {
        self.seed_counter += 1;
        self.seed_counter
    }

    fn fresh_seed(&mut self, player: u64) -> String {
        let counter = self.next_counter();
        self.seed_from_entropy(player, counter)
    }

    /// Testnet-grade seed generation: SHA-256 over a counter, wall-clock nanos
    /// and the player id. Replace with a CSPRNG at mainnet (doc 14 §14.8).
    fn seed_from_entropy(&self, player: u64, counter: u64) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let mut msg = Vec::with_capacity(48);
        msg.extend_from_slice(b"lm-casino-seed-v1");
        msg.extend_from_slice(&player.to_le_bytes());
        msg.extend_from_slice(&counter.to_le_bytes());
        msg.extend_from_slice(&nanos.to_le_bytes());
        fair::to_hex(&fair::sha256(&msg))
    }
}

/// One LKYL expressed in micro-LKYL (re-export for callers).
pub const LKYL: u64 = MICROS_PER_LKYL;

/// Hard cap on a single FUN bet (play money). Generous for play but keeps
/// the payout arithmetic (max ×8000) far inside u64.
pub const FUN_MAX_BET: u64 = 10_000 * MICROS_PER_LKYL;

#[cfg(test)]
mod tests {
    use super::*;

    fn casino() -> Casino {
        Casino::new(1_000 * LKYL)
    }

    #[test]
    fn spin_settles_and_conserves() {
        let mut c = casino();
        c.ensure_player(1);
        c.ledger.deposit(1, 10 * LKYL).unwrap();
        let rec = c.spin(1, Game::LuckyLode, 10_000).unwrap();
        assert_eq!(rec.bet, 10_000);
        assert_eq!(rec.game, Game::LuckyLode);
        assert_eq!(rec.symbols.len(), 5);
        assert_eq!(c.balance(1), 10 * LKYL - 10_000 + rec.win);
        assert_eq!(c.player(1).unwrap().spins, 1);
        assert!(c.ledger.is_conserving());
    }

    #[test]
    fn replay_reproduces_spin_and_win() {
        let mut c = casino();
        c.ensure_player(1);
        c.ledger.deposit(1, 10 * LKYL).unwrap();
        let rec = c.spin(1, Game::LuckyLode, 20_000).unwrap();
        let seed = c.player(1).unwrap().server_seed.clone();
        let symbols = c
            .replay(Game::LuckyLode, &seed, &rec.client_seed, rec.nonce)
            .unwrap();
        assert_eq!(symbols, rec.symbols);
        assert_eq!(c.outcome_win(Game::LuckyLode, &symbols, rec.bet), rec.win);
    }

    #[test]
    fn ways_spin_replays_too() {
        let mut c = casino();
        c.ensure_player(2);
        c.ledger.deposit(2, 10 * LKYL).unwrap();
        let rec = c.spin(2, Game::LuckyLodeWays, 25_000).unwrap();
        assert_eq!(rec.symbols.len(), 6);
        let seed = c.player(2).unwrap().server_seed.clone();
        let symbols = c
            .replay(Game::LuckyLodeWays, &seed, &rec.client_seed, rec.nonce)
            .unwrap();
        assert_eq!(symbols, rec.symbols);
        assert_eq!(
            c.outcome_win(Game::LuckyLodeWays, &symbols, rec.bet),
            rec.win
        );
    }

    #[test]
    fn rotate_reveals_a_verifiable_old_seed_and_resets_nonce() {
        let mut c = casino();
        c.ensure_player(3);
        c.ledger.deposit(3, LKYL).unwrap();
        let old_commitment = c.player(3).unwrap().commitment.clone();
        let rec = c.spin(3, Game::LuckyLode, 10_000).unwrap();
        let revealed = c.rotate(3).unwrap();
        // The revealed seed matches the commitment that was published.
        let bytes = fair::from_hex(&revealed).unwrap();
        assert_eq!(fair::to_hex(&fair::commitment(&bytes)), old_commitment);
        // And it reproduces the spin.
        let symbols = c
            .replay(Game::LuckyLode, &revealed, &rec.client_seed, rec.nonce)
            .unwrap();
        assert_eq!(symbols, rec.symbols);
        assert_eq!(c.player(3).unwrap().nonce, 0);
    }

    #[test]
    fn rejects_bad_bets_and_unknown_players() {
        let mut c = casino();
        assert_eq!(
            c.spin(9, Game::LuckyLode, 10_000).unwrap_err(),
            CasinoError::UnknownPlayer
        );
        c.ensure_player(1);
        c.ledger.deposit(1, 10 * LKYL).unwrap();
        assert_eq!(
            c.spin(1, Game::LuckyLode, 0).unwrap_err(),
            CasinoError::BadBet("zero bet")
        );
        // 10 lines: a non-multiple bet is refused.
        assert_eq!(
            c.spin(1, Game::LuckyLode, 9_999).unwrap_err(),
            CasinoError::BadBet("bet must be a multiple of lines")
        );
        // Over the exposure cap.
        c.ensure_player(4);
        c.ledger.deposit(4, 1_000 * LKYL).unwrap();
        assert_eq!(
            c.spin(4, Game::LuckyLodeWays, c.max_bet() + 1).unwrap_err(),
            CasinoError::Cas(CasError::HouseCannotCover)
        );
    }

    #[test]
    fn client_seed_changes_outcomes() {
        let mut c = casino();
        c.ensure_player(1);
        c.ledger.deposit(1, 10 * LKYL).unwrap();
        c.set_client_seed(1, "seed-a").unwrap();
        let a = c.spin(1, Game::LuckyLode, 10_000).unwrap();
        let seed = c.player(1).unwrap().server_seed.clone();
        c.set_client_seed(1, "seed-b").unwrap();
        let b = c.replay(Game::LuckyLode, &seed, "seed-b", a.nonce).unwrap();
        assert_ne!(a.symbols, b);
    }

    #[test]
    fn game_names_round_trip() {
        for g in [Game::LuckyLode, Game::LuckyLodeWays] {
            assert_eq!(Game::parse(g.as_str()), Some(g));
        }
        assert_eq!(Game::parse("nope"), None);
    }

    #[test]
    fn fun_claim_grants_and_cools_down() {
        let mut c = casino();
        c.ensure_player(1);
        let grant = 1_000 * LKYL;
        let fc = c.claim_fun(1, 1_000, grant, 3_600).unwrap();
        assert_eq!(fc.granted, grant);
        assert_eq!(fc.balance, grant);
        assert_eq!(fc.next_claim_at, 4_600);
        assert_eq!(c.fun_balance(1), grant);
        // Inside the cooldown: refused, carrying the next allowed time.
        assert_eq!(
            c.claim_fun(1, 1_001, grant, 3_600),
            Err(CasinoError::FunCooldown(4_600))
        );
        // At the boundary it is allowed again.
        assert!(c.claim_fun(1, 4_600, grant, 3_600).is_ok());
        assert_eq!(c.fun_balance(1), 2 * grant);
        assert_eq!(c.player(1).unwrap().fun_claimed, 2 * grant);
        // Unknown player / zero grant.
        assert_eq!(
            c.claim_fun(9, 5_000, grant, 3_600),
            Err(CasinoError::UnknownPlayer)
        );
        assert_eq!(
            c.claim_fun(1, 9_999, 0, 3_600),
            Err(CasinoError::BadBet("zero FUN grant"))
        );
    }

    #[test]
    fn fun_spin_is_isolated_from_the_house_ledger() {
        let mut c = casino();
        c.ensure_player(1);
        c.claim_fun(1, 0, 1_000 * LKYL, 3_600).unwrap();
        let bankroll_before = c.bankroll();
        let custody_before = c.ledger.custody();
        let rec = c.spin_fun(1, Game::LuckyLode, 10 * LKYL).unwrap();
        // FUN moved; the real ledger did not.
        assert_eq!(c.fun_balance(1), 1_000 * LKYL - 10 * LKYL + rec.win);
        assert_eq!(c.balance(1), 0);
        assert_eq!(c.bankroll(), bankroll_before);
        assert_eq!(c.ledger.custody(), custody_before);
        assert_eq!(c.ledger.net_ggr(), 0);
        assert!(c.ledger.is_conserving());
        assert_eq!((c.total_spins, c.total_fun_spins), (0, 1));
        let p = c.player(1).unwrap();
        assert_eq!((p.spins, p.fun_spins), (0, 1));
        assert_eq!(p.fun_wagered, 10 * LKYL);
        assert_eq!(p.fun_won, rec.win);
        assert!(p.fun_last.is_some());
        // Line bets must divide by the line count, FUN or not.
        assert_eq!(
            c.spin_fun(1, Game::LuckyLode, 9_999).unwrap_err(),
            CasinoError::BadBet("bet must be a multiple of lines")
        );
        // Over the cap / over the balance.
        assert_eq!(
            c.spin_fun(1, Game::LuckyLode, FUN_MAX_BET + 10).unwrap_err(),
            CasinoError::BadBet("FUN bet over the cap")
        );
        assert_eq!(
            c.spin_fun(1, Game::LuckyLode, 5_000 * LKYL).unwrap_err(),
            CasinoError::Cas(CasError::InsufficientFunds)
        );
    }

    #[test]
    fn fun_and_real_spins_share_the_nonce_cursor_and_verify() {
        let mut c = casino();
        c.ensure_player(1);
        c.ledger.deposit(1, 10 * LKYL).unwrap();
        c.claim_fun(1, 0, 1_000 * LKYL, 3_600).unwrap();
        let a = c.spin(1, Game::LuckyLode, 10_000).unwrap();
        let b = c.spin_fun(1, Game::LuckyLodeWays, 10 * LKYL).unwrap();
        let d = c.spin(1, Game::LuckyLode, 10_000).unwrap();
        assert_eq!([a.nonce, b.nonce, d.nonce], [0, 1, 2]);
        // Both modes replay from the revealed seed.
        let seed = c.player(1).unwrap().server_seed.clone();
        for rec in [&a, &b, &d] {
            let symbols = c
                .replay(rec.game, &seed, &rec.client_seed, rec.nonce)
                .unwrap();
            assert_eq!(symbols, rec.symbols);
            assert_eq!(c.outcome_win(rec.game, &symbols, rec.bet), rec.win);
        }
    }
}
