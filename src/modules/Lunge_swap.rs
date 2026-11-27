/// Lunge Swap — attribute-swapping macro for spears.
///
/// Mechanic (MC-28289 / attribute-swapping exploit):
///   The server evaluates the attack-speed threshold from the item held *before*
///   a slot change, but grants the Lunge effect from the item held *after* the
///   change.  Swapping away from a fast carrier (empty slot = 5-tick recharge)
///   to the spear in the same tick fires a Lunge jab at the carrier's rate
///   instead of the spear's normal 13–23-tick rate.
///
/// Sequence each key-press:
///   1. Switch to the spear slot.
///   2. Send the stab packet (`ServerboundPlayerAction { action: Stab, … }`).
///   3. Swing the main hand animation.
///   4. Reset only the attack ticker (not the swap ticker) so the carrier's
///      charge reads against the carrier's delay next time.
///   5. After one tick, switch back to the carrier slot.
///
/// The module is registered under `Movement / Blatant`.

use std::sync::Mutex;

use azalea::Client;

use super::registry::{Id, lunge_swap as setting, options};
use super::{Edge, Phase, store};
use crate::client::tracking::game_time;
use crate::items::kinetic;
use crate::session::{Gamemode, SharedMutex};

// ── Carrier-slot options ─────────────────────────────────────────────────────

options! {
    pub enum CarrierSlot {
        /// Keep whatever you were holding; use the empty-slot fast path only
        /// when the spear is already in hand.
        Empty = "Empty slot",
        One   = "1",
        Two   = "2",
        Three = "3",
        Four  = "4",
        Five  = "5",
        Six   = "6",
        Seven = "7",
        Eight = "8",
        Nine  = "9",
    }
}

options! {
    pub enum SpearSlot {
        Auto  = "Auto",
        One   = "1",
        Two   = "2",
        Three = "3",
        Four  = "4",
        Five  = "5",
        Six   = "6",
        Seven = "7",
        Eight = "8",
        Nine  = "9",
    }
}

// ── Module-level edge + state ─────────────────────────────────────────────────

static EDGE: Edge = Edge::new(Id::LungeSwap);

static RUN: Mutex<Run> = Mutex::new(Run::IDLE);

struct Run {
    /// Tick on which the last stab was sent; used to enforce the 1-tick
    /// settle delay before swapping back to the carrier.
    stab_at: Option<i64>,
    /// Hotbar slot we swapped away from (carrier); restore after 1 tick.
    restore: Option<u8>,
    /// Spear slot we switched to for the stab.
    spear: Option<u8>,
}

impl Run {
    const IDLE: Run = Run {
        stab_at: None,
        restore: None,
        spear: None,
    };
}

// ── Public surface ────────────────────────────────────────────────────────────

pub fn enabled() -> bool {
    store().enabled(Id::LungeSwap)
}

/// Called every tick by `play::interaction::handle_interaction`.
/// Returns `true` if a stab was sent this tick (so callers can gate other
/// attack paths — mirrors the bool returns in `auto_mine::tick` etc.).
pub fn tick(bot: &Client, shared: &SharedMutex) -> bool {
    let s = store();
    match EDGE.poll(&s) {
        Phase::Off => return false,
        Phase::Stopped => {
            // Restore the carrier slot if we were mid-swap when disabled.
            let mut run = RUN.lock().unwrap();
            if let (Some(restore), Some(spear)) = (run.restore, run.spear) {
                if bot.selected_hotbar_slot().is_ok_and(|slot| slot == spear) {
                    let _ = bot.set_selected_hotbar_slot(restore);
                }
            }
            *run = Run::IDLE;
            return false;
        }
        Phase::Started => *RUN.lock().unwrap() = Run::IDLE,
        Phase::Running => {}
    }

    let now = game_time();
    let mut run = RUN.lock().unwrap();

    // ── Settle: swap back to carrier one tick after the stab ─────────────────
    if let (Some(stabbed_at), Some(restore)) = (run.stab_at, run.restore) {
        if now > stabbed_at {
            // Confirm we are still on the spear before restoring.
            if let Some(spear) = run.spear {
                if bot.selected_hotbar_slot().is_ok_and(|s| s == spear) {
                    let _ = bot.set_selected_hotbar_slot(restore);
                }
            }
            run.stab_at = None;
            run.restore = None;
            run.spear = None;
        }
        // Still waiting for the settle tick; don't stab again.
        return false;
    }

    // ── Gate checks ──────────────────────────────────────────────────────────
    let (triggered, spear_choice, carrier_choice, food, attack_strength, attack_delay) = {
        let st = shared.lock().unwrap();
        if !st.in_world
            || st.screen_open
            || st.session.dead
            || st.session.gamemode == Gamemode::Spectator
        {
            return false;
        }
        // The key-trigger flag is consumed here; it is set by the GUI keybind handler.
        let triggered = st.session.lunge_swap_triggered;
        (
            triggered,
            s.choice(setting::SPEAR),
            s.choice(setting::CARRIER),
            st.session.food,
            st.session.attack_strength,
            st.session.attack_delay,
        )
    };

    if !triggered {
        return false;
    }
    // Clear the flag so it only fires once per press.
    shared.lock().unwrap().session.lunge_swap_triggered = false;

    // Lunge requires food > 6 (3 hunger bars) — same gate the server enforces.
    if food <= 6 {
        crate::log_debug!("lunge_swap", "skipped: food {food} ≤ 6");
        return false;
    }

    // Attack bar must be ready. `attack_strength` is the carrier's cooldown
    // fraction; 1.0 = fully charged.  We allow 0.95+ to match the server's
    // threshold (same as AutoMace's default).
    let threshold = s.num(setting::COOLDOWN) / 100.0;
    if attack_strength < threshold {
        crate::log_debug!(
            "lunge_swap",
            "skipped: cooldown {:.2} < {:.2}",
            attack_strength,
            threshold
        );
        return false;
    }

    // ── Find spear slot ───────────────────────────────────────────────────────
    let hotbar = {
        let st = shared.lock().unwrap();
        st.session.hotbar.clone()
    };
    let current_slot = match bot.selected_hotbar_slot() {
        Ok(s) => s,
        Err(_) => return false,
    };

    let spear_slot = find_spear(&hotbar, current_slot, spear_choice);
    let Some(spear_slot) = spear_slot else {
        crate::log_debug!("lunge_swap", "skipped: no spear in hotbar");
        return false;
    };

    // ── Resolve carrier slot ──────────────────────────────────────────────────
    // "Empty slot" uses a hotbar slot with nothing in it, which recharges in
    // 5 ticks — the fastest possible attack speed.
    let carrier_slot = resolve_carrier(&hotbar, current_slot, spear_slot, carrier_choice);
    let Some(carrier_slot) = carrier_slot else {
        crate::log_debug!("lunge_swap", "skipped: no carrier slot found");
        return false;
    };

    // ── Execute: switch → spear, stab, record restore ────────────────────────
    let restore = if current_slot != spear_slot {
        let _ = bot.set_selected_hotbar_slot(spear_slot);
        Some(carrier_slot)
    } else {
        // Already holding the spear; carrier IS the restore target.
        Some(carrier_slot)
    };

    // Fire the stab packet.  `play::spear::stab` sends ServerboundPlayerAction
    // { action: Stab, … }, resets the attack-strength ticker, and plays the
    // local arm swing.
    let sound = hotbar
        .get(spear_slot as usize)
        .and_then(|st| st.as_itemstackdata())
        .and_then(|data| kinetic::kinetic(data))
        .and_then(|k| k.use_sound);
    crate::play::spear::stab(bot, shared, sound);

    run.stab_at = Some(now);
    run.restore = restore;
    run.spear = Some(spear_slot);

    crate::log_debug!(
        "lunge_swap",
        "stab: spear={spear_slot} carrier={carrier_slot} strength={attack_strength:.2} delay={attack_delay:.1}"
    );

    true
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Return the first hotbar slot that holds a spear or trident, respecting
/// the slot-choice setting.
fn find_spear(
    hotbar: &[crate::session::SlotStack],
    selected: u8,
    choice: SpearSlot,
) -> Option<u8> {
    let is_spear = |i: u8| -> bool {
        hotbar
            .get(i as usize)
            .is_some_and(|st| !st.is_empty() && is_spear_item(st.item))
    };
    match choice {
        SpearSlot::Auto if is_spear(selected) => Some(selected),
        SpearSlot::Auto => (0u8..9).find(|&i| is_spear(i)),
        fixed => {
            let i = fixed as u8 - 1; // enum values 1–9 → indices 0–8
            is_spear(i).then_some(i)
        }
    }
}

/// Return the carrier slot.  When `carrier_choice` is `Empty`, we prefer a
/// genuinely empty slot; otherwise we fall back to the fixed slot.
fn resolve_carrier(
    hotbar: &[crate::session::SlotStack],
    current: u8,
    spear: u8,
    choice: CarrierSlot,
) -> Option<u8> {
    match choice {
        CarrierSlot::Empty => {
            // Prefer a slot that is currently empty (fastest recharge).
            let empty = (0u8..9).find(|&i| {
                i != spear
                    && hotbar
                        .get(i as usize)
                        .is_some_and(|st| st.is_empty())
            });
            // Fall back to current slot even if it has an item — still better
            // than the spear's native delay.
            empty.or(Some(current).filter(|&c| c != spear))
        }
        fixed => {
            let i = fixed as u8 - 1;
            (i != spear).then_some(i)
        }
    }
}

/// True for every spear tier and the trident.
fn is_spear_item(item: &str) -> bool {
    matches!(
        item,
        "wooden_spear"
            | "stone_spear"
            | "copper_spear"
            | "iron_spear"
            | "golden_spear"
            | "diamond_spear"
            | "netherite_spear"
            | "trident"
    )
}
