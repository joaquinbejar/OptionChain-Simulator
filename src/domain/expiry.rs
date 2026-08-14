//! Rolling expiration planner for v2 simulations.
//!
//! This module turns a versioned [`ExpirationSchedule`] and one simulated
//! timestamp into the inventory of expirations that are alive at that instant.
//! It is the source of truth for keeping 0DTE, weekly, monthly and yearly /
//! LEAPS contracts continuously available across a long simulation, and it is
//! specified by ADR 0001 (`doc/adr/0001-v2-rolling-simulation-contract.md`).
//!
//! It is deliberately a **pure function of its inputs**: no I/O, no RNG draws,
//! no wall-clock reads. Two consequences matter for the reproducibility
//! contract. The planner cannot perturb the seeded price/volatility stream,
//! because it never touches it; and adding, removing or reordering rules can
//! never change that stream either. For a **fixed IANA time-zone database**, a
//! given `(schedule, simulated_at)` pair always yields the same inventory, on
//! any machine, in any locale, at any point in the process's life.
//!
//! That qualifier is not pedantry. `expires_at` is a function of the bundled
//! tz rules, and `chrono-tz` ships tzdb updates in *patch* releases, so two
//! builds of the same commit made months apart can embed different data —
//! theoretical for `America/New_York`, real for zones such as `Africa/Cairo`
//! or `Asia/Jerusalem`, which change DST rules on weeks of notice. The tzdb
//! version is therefore a replay input like the seed and the calendar version:
//! [`tzdb_version`] exposes it so #44 can persist and echo it.
//!
//! The two rules that carry the most weight:
//!
//! - **Cutoff.** An expiration is expired when `expires_at <= simulated_at`.
//!   The comparison is on the absolute UTC instant and uses `<=`, so a 17:00
//!   local expiry is gone at exactly 17:00, and its replacement is present in
//!   the same result.
//! - **Overlap.** Counts are evaluated *per rule*; only afterwards are
//!   coincident physical expirations deduplicated, carrying the union of the
//!   contributing rule ids as labels. A rule is never starved because another
//!   rule already claimed the same date.
//!
//! # Visibility
//!
//! Every type here is `pub(crate)` and stays inside the private `domain`
//! module. It is deliberately **not** part of the published crate's API: doing
//! so would put `chrono_tz::Tz`, `chrono::Weekday` and `chrono::NaiveTime` into
//! the public surface, and a `chrono-tz` major bump would then be a breaking
//! change for downstream consumers such as IronCondor. Issue #44 owns the
//! public representation — primitive-typed schedule fields on the v2 session
//! parameters — and converts into these types at the one boundary that already
//! does f64 → typed conversion.
//!
//! # Validation
//!
//! The invariants are established in one place and cannot be bypassed. Fields
//! are private, the constructors validate, and `Deserialize` is routed through
//! `#[serde(try_from = ...)]` so a schedule loaded from Redis is checked
//! exactly like one built from a request. Without that, a stored
//! `target_count` of `1e11` would reach the projection loop.

// The planner is the bottom of the v0.2.0 stack and has no in-tree caller yet:
// issue #44 persists an `ExpirationSchedule` in the v2 session parameters and
// #46 calls `RollingPlanner::active_at` to build each snapshot. Landing it with
// its own tests first is what keeps every PR in the stack independently sound.
//
// `expect` rather than `allow` on purpose: the moment #44 or #46 wires a caller
// in, the expectation goes unfulfilled and CI's `-D warnings` turns it into a
// hard error, so removing this is enforced by the compiler rather than promised
// in a commit message.
#![expect(
    dead_code,
    reason = "no in-tree caller until #44 persists the schedule and #46 calls active_at"
)]

use crate::utils::ChainError;
use chrono::{DateTime, Datelike, Days, NaiveDate, NaiveTime, TimeZone, Utc, Weekday};
use chrono_tz::{GapInfo, Tz};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;

/// Maximum number of rules one schedule may declare.
///
/// A plain constant for now; issue #48 turns this and the two bounds below into
/// validated `OCS_MAX_*` environment knobs alongside the other request limits
/// in `api::rest::limits`.
pub(crate) const MAX_SCHEDULE_RULES: usize = 16;

/// Maximum `target_count` a single rule may request.
///
/// A plain constant for now; issue #48 turns it into an `OCS_MAX_*` knob.
pub(crate) const MAX_TARGET_COUNT: usize = 256;

/// Maximum number of expirations one snapshot may carry.
///
/// Validated against the **pre-deduplication** sum of every rule's
/// `target_count`, which is the tight upper bound on how many chains a
/// snapshot can hold. Checking the upper bound at construction keeps the
/// rejection deterministic: it does not depend on which dates happen to
/// coincide at a particular simulated instant.
///
/// A plain constant for now; issue #48 turns it into an `OCS_MAX_*` knob.
pub(crate) const MAX_EXPIRATIONS_PER_SNAPSHOT: usize = 512;

/// Maximum length of a `rule_id`.
///
/// Rule ids are echoed as labels on every chain of every step and are joined
/// into a single CSV column on export, so they are bounded in both length and
/// charset (see [`validate_rule_id`]).
pub(crate) const MAX_RULE_ID_LEN: usize = 64;

/// Upper bound on how many candidate days a daily or weekly rule may scan
/// before the projection is considered pathological.
///
/// A weekly rule naming one weekday needs seven days per expiration; the
/// factor of eight plus a constant leaves room for weekends and for the
/// starting partial week without ever letting the scan run unbounded.
const DAY_SCAN_SLACK: usize = 32;

/// Upper bound on how many candidate periods a monthly or yearly rule may scan.
const PERIOD_SCAN_SLACK: usize = 2;

/// The month a `yearly` rule expires in when the request omits it — December,
/// as ADR 0001 §4.1 specifies.
const DEFAULT_YEARLY_MONTH: u32 = 12;

/// Seconds in a day, as a `Decimal`, for the fractional days-to-expiration
/// conversion.
const SECONDS_PER_DAY: Decimal = Decimal::from_parts(86_400, 0, 0, false, 0);

/// The version of the IANA time-zone database this binary resolves local
/// expiration times against, e.g. `"2025b"`.
///
/// This is a **replay input**. Every `expires_at` the planner produces is a
/// function of these rules, and `chrono-tz` ships tzdb updates in patch
/// releases, so a simulation replayed against a different database can differ
/// for any zone whose DST rules changed in between. Issue #44 persists and
/// echoes this alongside the effective seed and the calendar version, so a
/// client can tell whether a replay is comparing like with like.
#[must_use]
pub(crate) fn tzdb_version() -> &'static str {
    chrono_tz::IANA_TZDB_VERSION
}

/// The versioned calendar policy a schedule is evaluated under.
///
/// The version is part of the stored simulation and part of the replay input
/// set, so a future policy can be added without silently changing the tape of
/// a simulation created under an older one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum CalendarVersion {
    /// Weekends are the only ineligible days; no exchange-holiday data is
    /// consulted. See [`CalendarVersion::eligible_date`].
    #[serde(rename = "weekdays_v1")]
    WeekdaysV1,
}

impl CalendarVersion {
    /// The holiday-adjustment hook.
    ///
    /// Returns the date an expiration should land on, or `None` when the
    /// candidate carries no expiration at all and the caller should move to the
    /// next candidate.
    ///
    /// [`CalendarVersion::WeekdaysV1`] performs no adjustment: it rejects
    /// Saturday and Sunday and accepts every other date, because it bundles no
    /// exchange-holiday database. A future `weekdays_v2` can roll a holiday
    /// expiry back to the previous eligible weekday here without touching any
    /// simulation stored under `weekdays_v1`. Note that such a hook can map two
    /// candidates onto the same date, which is why per-rule projection counts
    /// **distinct instants** (see [`RollingPlanner::project_rule`]).
    #[must_use]
    pub(crate) fn eligible_date(self, date: NaiveDate) -> Option<NaiveDate> {
        match self {
            CalendarVersion::WeekdaysV1 => match date.weekday() {
                Weekday::Sat | Weekday::Sun => None,
                _ => Some(date),
            },
        }
    }

    /// The stable wire name of this calendar version.
    #[must_use]
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            CalendarVersion::WeekdaysV1 => "weekdays_v1",
        }
    }
}

/// What a rule expires on, independent of how many expirations it keeps.
///
/// Serialised **internally tagged** under `kind` with `snake_case` variant
/// names, and flattened into [`ExpiryRule`], so a stored rule reads exactly as
/// ADR 0001 §14.2 shows it:
/// `{"rule_id": "monthlies", "kind": "monthly", "target_count": 12, "weekday": "Fri"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ExpiryRuleKind {
    /// Every eligible weekday expires — the rolling 0DTE.
    Daily,
    /// Each named weekday expires. The set is non-empty and contains no
    /// weekend day; [`ExpirationSchedule::validate`] enforces both.
    Weekly {
        /// The weekdays that carry an expiration, deduplicated and ordered
        /// Monday-first by [`ExpiryRuleKind::weekly`].
        weekdays: Vec<Weekday>,
    },
    /// The **last** occurrence of `weekday` in each calendar month.
    Monthly {
        /// The weekday whose last occurrence in the month expires.
        weekday: Weekday,
    },
    /// The last occurrence of `weekday` in `month` of each year — the
    /// LEAPS-style rule.
    Yearly {
        /// The weekday whose last occurrence in `month` expires.
        weekday: Weekday,
        /// The month, `1..=12`.
        month: u32,
    },
}

impl ExpiryRuleKind {
    /// Builds an [`ExpiryRuleKind::Weekly`] with its weekday set normalised —
    /// deduplicated and ordered Monday-first — so that two schedules that name
    /// the same days in a different order are the same schedule.
    ///
    /// Normalising here rather than at validation time matters for replay: the
    /// normalised form is what is persisted and echoed back to the client.
    ///
    /// This is infallible. An empty or weekend-carrying set is rejected by
    /// [`ExpirationSchedule::validate`], which knows the owning rule id and can
    /// therefore name the offending field.
    #[must_use]
    pub(crate) fn weekly(weekdays: impl IntoIterator<Item = Weekday>) -> Self {
        // `chrono::Weekday` is not `Ord`, so deduplicate and order through the
        // Monday-based index, which round-trips losslessly.
        let ordered: BTreeSet<u8> = weekdays
            .into_iter()
            .filter_map(|day| u8::try_from(day.num_days_from_monday()).ok())
            .collect();
        let weekdays = ordered
            .into_iter()
            .filter_map(|index| Weekday::try_from(index).ok())
            .collect();
        ExpiryRuleKind::Weekly { weekdays }
    }

    /// Builds an [`ExpiryRuleKind::Yearly`] in the default month, so the
    /// constructor and the `Deserialize` path agree on what "yearly without a
    /// month" means.
    #[must_use]
    pub(crate) fn yearly(weekday: Weekday) -> Self {
        ExpiryRuleKind::Yearly {
            weekday,
            month: DEFAULT_YEARLY_MONTH,
        }
    }

    /// The wire tag this kind serialises under.
    ///
    /// Nothing calls this at runtime — it exists so that adding a variant to
    /// [`ExpiryRuleKind`] fails to compile until [`ExpiryRuleKindTag`] gains
    /// the matching variant. Without it the two lists drift, and a kind that
    /// serialises fine becomes a stored schedule that cannot be read back.
    #[must_use]
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "compile-time exhaustiveness link")
    )]
    fn tag(&self) -> ExpiryRuleKindTag {
        match self {
            ExpiryRuleKind::Daily => ExpiryRuleKindTag::Daily,
            ExpiryRuleKind::Weekly { .. } => ExpiryRuleKindTag::Weekly,
            ExpiryRuleKind::Monthly { .. } => ExpiryRuleKindTag::Monthly,
            ExpiryRuleKind::Yearly { .. } => ExpiryRuleKindTag::Yearly,
        }
    }

    /// A short, stable description of the rule kind, used in validation
    /// messages.
    #[must_use]
    fn kind_name(&self) -> &'static str {
        match self {
            ExpiryRuleKind::Daily => "daily",
            ExpiryRuleKind::Weekly { .. } => "weekly",
            ExpiryRuleKind::Monthly { .. } => "monthly",
            ExpiryRuleKind::Yearly { .. } => "yearly",
        }
    }
}

/// One expiration rule: what it expires on, and how many non-expired
/// expirations it keeps available at every step.
///
/// Fields are private and the constructor validates, so an out-of-range
/// `target_count` or a malformed `rule_id` is unrepresentable. `Deserialize`
/// goes through the same constructor (see [`ExpiryRuleWire`]), which is what
/// makes a schedule loaded from the session store as safe as one built from a
/// request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ExpiryRuleWire")]
pub(crate) struct ExpiryRule {
    rule_id: String,
    #[serde(flatten)]
    kind: ExpiryRuleKind,
    target_count: NonZeroUsize,
}

/// Which rule kind a wire rule names, before its kind-specific fields are
/// checked.
///
/// Kept in step with [`ExpiryRuleKind`] by [`ExpiryRuleKind::tag`], whose
/// exhaustive match is what breaks the build if one list gains a variant and
/// the other does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ExpiryRuleKindTag {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

/// The deserialization shape of [`ExpiryRule`], routed through its validating
/// constructor by `#[serde(try_from = ...)]`.
///
/// The kind-specific fields are listed flat and optional rather than reached
/// through `#[serde(flatten)]` on [`ExpiryRuleKind`], because serde does not
/// support `deny_unknown_fields` alongside `flatten` — and that rejection is
/// exactly what ADR 0001 §4.1 specifies. Deserialising through this shape also
/// makes a field that belongs to *another* kind, such as `weekday` on a `daily`
/// rule, an error naming the field instead of a silently ignored key.
///
/// [`ExpiryRule`] still *serialises* through the flattened, internally-tagged
/// enum, so the stored shape is untouched. What this type accepts moves in two
/// directions: it is narrower for a stray or foreign field, and wider for a
/// `yearly` rule that omits `month`, which now takes the
/// [`DEFAULT_YEARLY_MONTH`] the ADR specifies instead of failing on a missing
/// field. An explicit `null` for a foreign field is still accepted and ignored,
/// which is what serde's own `Option` handling does.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpiryRuleWire {
    rule_id: String,
    kind: ExpiryRuleKindTag,
    target_count: NonZeroUsize,
    weekdays: Option<Vec<Weekday>>,
    weekday: Option<Weekday>,
    month: Option<u32>,
}

impl TryFrom<ExpiryRuleWire> for ExpiryRule {
    type Error = ChainError;

    fn try_from(wire: ExpiryRuleWire) -> Result<Self, Self::Error> {
        // Before anything interpolates the id into an error field that a
        // handler reflects back to the caller.
        validate_rule_id(&wire.rule_id)?;
        let kind = wire.kind_from_parts()?;
        ExpiryRule::from_parts(wire.rule_id, kind, wire.target_count)
    }
}

impl ExpiryRuleWire {
    /// Builds the kind, requiring the fields it owns and rejecting the ones it
    /// does not.
    fn kind_from_parts(&self) -> Result<ExpiryRuleKind, ChainError> {
        let field = |name: &str| format!("schedules.{}.{name}", self.rule_id);

        match self.kind {
            ExpiryRuleKindTag::Daily => {
                self.reject(&[])?;
                Ok(ExpiryRuleKind::Daily)
            }
            ExpiryRuleKindTag::Weekly => {
                self.reject(&["weekdays"])?;
                let weekdays = self
                    .weekdays
                    .clone()
                    .ok_or_else(|| ChainError::Validation {
                        field: field("weekdays"),
                        reason: "is required for a weekly rule".to_string(),
                    })?;
                Ok(ExpiryRuleKind::weekly(weekdays))
            }
            ExpiryRuleKindTag::Monthly => {
                self.reject(&["weekday"])?;
                Ok(ExpiryRuleKind::Monthly {
                    weekday: self.require_weekday()?,
                })
            }
            ExpiryRuleKindTag::Yearly => {
                self.reject(&["weekday", "month"])?;
                Ok(ExpiryRuleKind::Yearly {
                    weekday: self.require_weekday()?,
                    month: self.month.unwrap_or(DEFAULT_YEARLY_MONTH),
                })
            }
        }
    }

    /// Fails when a kind-specific field outside `allowed` is present.
    ///
    /// A stray key is schema drift, and a schedule is persisted and replayed —
    /// accepting it silently would let a stored simulation mean something
    /// different from what its author wrote. The check is an allow-list over an
    /// exhaustive destructure rather than a lookup by name, so a new
    /// kind-specific field on this type is a compile error here instead of a
    /// field that silently reports itself absent.
    fn reject(&self, allowed: &[&str]) -> Result<(), ChainError> {
        let Self {
            rule_id: _,
            kind: _,
            target_count: _,
            weekdays,
            weekday,
            month,
        } = self;

        let present = [
            ("weekdays", weekdays.is_some()),
            ("weekday", weekday.is_some()),
            ("month", month.is_some()),
        ];

        for (name, is_present) in present {
            if is_present && !allowed.contains(&name) {
                return Err(ChainError::Validation {
                    field: format!("schedules.{}.{name}", self.rule_id),
                    reason: "does not belong to this rule kind".to_string(),
                });
            }
        }
        Ok(())
    }

    /// Requires the `weekday` a monthly or yearly rule expires on.
    fn require_weekday(&self) -> Result<Weekday, ChainError> {
        self.weekday.ok_or_else(|| ChainError::Validation {
            field: format!("schedules.{}.weekday", self.rule_id),
            reason: "is required for this rule kind".to_string(),
        })
    }
}

impl ExpiryRule {
    /// Builds a rule, rejecting a malformed `rule_id` and a `target_count` of
    /// zero or above [`MAX_TARGET_COUNT`].
    ///
    /// A rule keeping zero expirations is not a rule, and the type reflects
    /// that: `target_count` is a [`NonZeroUsize`] internally, so the invalid
    /// state is unrepresentable past this constructor.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Validation`] naming
    /// `schedules.<rule_id>.target_count` when the count is zero or exceeds
    /// [`MAX_TARGET_COUNT`], or `schedules.rule_id` when the id is empty, too
    /// long, or carries a character outside `[A-Za-z0-9_-]`.
    pub(crate) fn new(
        rule_id: impl Into<String>,
        kind: ExpiryRuleKind,
        target_count: usize,
    ) -> Result<Self, ChainError> {
        let rule_id = rule_id.into();
        let target_count =
            NonZeroUsize::new(target_count).ok_or_else(|| ChainError::Validation {
                field: format!("schedules.{rule_id}.target_count"),
                reason: "must be at least 1".to_string(),
            })?;
        Self::from_parts(rule_id, kind, target_count)
    }

    /// The shared validating constructor behind [`ExpiryRule::new`] and the
    /// `Deserialize` path.
    fn from_parts(
        rule_id: String,
        kind: ExpiryRuleKind,
        target_count: NonZeroUsize,
    ) -> Result<Self, ChainError> {
        // Normalise here rather than only in `ExpiryRuleKind::weekly`, so the
        // `Deserialize` path converges on the same form: the normalised set is
        // what gets persisted and echoed (ADR 0001 §8), and a schedule loaded
        // from the store must not keep duplicates or an arbitrary order.
        let kind = match kind {
            ExpiryRuleKind::Weekly { weekdays } => ExpiryRuleKind::weekly(weekdays),
            other => other,
        };

        validate_rule_id(&rule_id)?;
        if target_count.get() > MAX_TARGET_COUNT {
            return Err(ChainError::Validation {
                field: format!("schedules.{rule_id}.target_count"),
                reason: format!(
                    "must not exceed {MAX_TARGET_COUNT}, got {}",
                    target_count.get()
                ),
            });
        }
        validate_rule_kind(&rule_id, &kind)?;

        Ok(Self {
            rule_id,
            kind,
            target_count,
        })
    }

    /// The rule's stable identifier, which becomes its label on every chain it
    /// produces.
    #[must_use]
    pub(crate) fn rule_id(&self) -> &str {
        &self.rule_id
    }

    /// What the rule expires on.
    #[must_use]
    pub(crate) fn kind(&self) -> &ExpiryRuleKind {
        &self.kind
    }

    /// How many non-expired expirations the rule keeps available, evaluated
    /// per rule and before coincident expirations are deduplicated.
    #[must_use]
    pub(crate) fn target_count(&self) -> NonZeroUsize {
        self.target_count
    }
}

/// A complete, versioned expiration schedule: the calendar policy, the zone and
/// local time every rule expires at, and the rules themselves.
///
/// Fields are private and both the constructor and the `Deserialize` path run
/// [`ExpirationSchedule::validate`], so a schedule that exists is a schedule
/// the planner can evaluate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ExpirationScheduleWire")]
pub(crate) struct ExpirationSchedule {
    calendar: CalendarVersion,
    timezone: Tz,
    expiration_time: NaiveTime,
    rules: Vec<ExpiryRule>,
}

/// The deserialization shape of [`ExpirationSchedule`], routed through its
/// validating constructor by `#[serde(try_from = ...)]`.
///
/// Nothing is flattened here, so the rejection ADR 0001 §4.1 specifies is the
/// serde attribute itself.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpirationScheduleWire {
    calendar: CalendarVersion,
    timezone: Tz,
    expiration_time: NaiveTime,
    rules: Vec<ExpiryRule>,
}

impl TryFrom<ExpirationScheduleWire> for ExpirationSchedule {
    type Error = ChainError;

    fn try_from(wire: ExpirationScheduleWire) -> Result<Self, Self::Error> {
        ExpirationSchedule::new(
            wire.calendar,
            wire.timezone,
            wire.expiration_time,
            wire.rules,
        )
    }
}

impl ExpirationSchedule {
    /// Builds a validated, normalised schedule.
    ///
    /// Normalisation orders the rules by `rule_id`, so two requests that list
    /// the same rules in a different order produce the same stored schedule —
    /// which matters because the normalised schedule is a replay input.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Validation`] for the first problem found; see
    /// [`ExpirationSchedule::validate`] for the full list.
    pub(crate) fn new(
        calendar: CalendarVersion,
        timezone: Tz,
        expiration_time: NaiveTime,
        rules: Vec<ExpiryRule>,
    ) -> Result<Self, ChainError> {
        let mut schedule = Self {
            calendar,
            timezone,
            expiration_time,
            rules,
        };
        schedule.validate()?;
        schedule.rules.sort_by(|a, b| a.rule_id.cmp(&b.rule_id));
        Ok(schedule)
    }

    /// The calendar policy the rules are evaluated under.
    #[must_use]
    pub(crate) fn calendar(&self) -> CalendarVersion {
        self.calendar
    }

    /// The IANA zone `expiration_time` is expressed in.
    #[must_use]
    pub(crate) fn timezone(&self) -> Tz {
        self.timezone
    }

    /// The **local** time of day at which every rule's expirations expire.
    #[must_use]
    pub(crate) fn expiration_time(&self) -> NaiveTime {
        self.expiration_time
    }

    /// The rules, ordered by `rule_id`.
    #[must_use]
    pub(crate) fn rules(&self) -> &[ExpiryRule] {
        &self.rules
    }

    /// Rejects every schedule the planner cannot evaluate unambiguously.
    ///
    /// Per-rule invariants — the `rule_id` charset and length, the
    /// `target_count` bound, and the kind-specific fields — are already
    /// established by [`ExpiryRule::from_parts`]; this checks what only the
    /// whole schedule knows.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Validation`] naming the offending field when:
    ///
    /// - the rule list is empty, or longer than [`MAX_SCHEDULE_RULES`];
    /// - a `rule_id` is duplicated;
    /// - the pre-deduplication sum of `target_count` exceeds
    ///   [`MAX_EXPIRATIONS_PER_SNAPSHOT`], or overflows.
    pub(crate) fn validate(&self) -> Result<(), ChainError> {
        if self.rules.is_empty() {
            return Err(ChainError::Validation {
                field: "schedules".to_string(),
                reason: "must declare at least one expiration rule".to_string(),
            });
        }
        if self.rules.len() > MAX_SCHEDULE_RULES {
            return Err(ChainError::Validation {
                field: "schedules".to_string(),
                reason: format!(
                    "must not exceed {MAX_SCHEDULE_RULES} rules, got {}",
                    self.rules.len()
                ),
            });
        }

        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut projected: usize = 0;

        for rule in &self.rules {
            if !seen.insert(rule.rule_id.as_str()) {
                return Err(ChainError::Validation {
                    field: format!("schedules.{}.rule_id", rule.rule_id),
                    reason: "must be unique within the schedule".to_string(),
                });
            }

            projected = projected
                .checked_add(rule.target_count.get())
                .ok_or_else(|| ChainError::Validation {
                    field: "schedules".to_string(),
                    reason: "total expiration count overflows".to_string(),
                })?;
        }

        if projected > MAX_EXPIRATIONS_PER_SNAPSHOT {
            return Err(ChainError::Validation {
                field: "schedules".to_string(),
                reason: format!(
                    "total expiration count must not exceed {MAX_EXPIRATIONS_PER_SNAPSHOT}, got {projected}"
                ),
            });
        }

        Ok(())
    }
}

/// Rejects a `rule_id` that is empty, too long, or carries a character outside
/// `[A-Za-z0-9_-]`.
///
/// The charset is deliberately narrow. Rule ids ride along as labels on every
/// chain of every step, and the CSV export joins them into a single column with
/// `|`, so a separator or a quote inside an id would corrupt that column.
/// Constraining it here — before anything is persisted or echoed — is cheaper
/// than turning previously-accepted schedules into `400`s later.
fn validate_rule_id(rule_id: &str) -> Result<(), ChainError> {
    if rule_id.is_empty() {
        return Err(ChainError::Validation {
            field: "schedules.rule_id".to_string(),
            reason: "must not be empty".to_string(),
        });
    }
    if rule_id.len() > MAX_RULE_ID_LEN {
        return Err(ChainError::Validation {
            field: "schedules.rule_id".to_string(),
            reason: format!(
                "must not exceed {MAX_RULE_ID_LEN} characters, got {}",
                rule_id.len()
            ),
        });
    }
    if let Some(bad) = rule_id
        .chars()
        .find(|c| !c.is_ascii_alphanumeric() && *c != '_' && *c != '-')
    {
        return Err(ChainError::Validation {
            field: "schedules.rule_id".to_string(),
            reason: format!("must contain only [A-Za-z0-9_-], found {bad:?}"),
        });
    }
    Ok(())
}

/// Validates the parts of a rule that depend on its kind.
fn validate_rule_kind(rule_id: &str, kind: &ExpiryRuleKind) -> Result<(), ChainError> {
    let field = format!("schedules.{rule_id}.{}", kind.kind_name());

    match kind {
        ExpiryRuleKind::Daily => Ok(()),
        ExpiryRuleKind::Weekly { weekdays } => {
            if weekdays.is_empty() {
                return Err(ChainError::Validation {
                    field: format!("{field}.weekdays"),
                    reason: "must name at least one weekday".to_string(),
                });
            }
            for weekday in weekdays {
                reject_weekend(&format!("{field}.weekdays"), *weekday)?;
            }
            Ok(())
        }
        ExpiryRuleKind::Monthly { weekday } => {
            reject_weekend(&format!("{field}.weekday"), *weekday)
        }
        ExpiryRuleKind::Yearly { weekday, month } => {
            reject_weekend(&format!("{field}.weekday"), *weekday)?;
            if !(1..=12).contains(month) {
                return Err(ChainError::Validation {
                    field: format!("{field}.month"),
                    reason: format!("must be between 1 and 12, got {month}"),
                });
            }
            Ok(())
        }
    }
}

/// Rejects a weekend weekday, which `weekdays_v1` can never expire on.
///
/// Naming Saturday explicitly is a request the service cannot honour, so it is
/// an error rather than a rule that silently produces nothing.
#[cold]
fn reject_weekend(field: &str, weekday: Weekday) -> Result<(), ChainError> {
    match weekday {
        Weekday::Sat | Weekday::Sun => Err(ChainError::Validation {
            field: field.to_string(),
            reason: format!("{weekday} is never an eligible expiration day under weekdays_v1"),
        }),
        _ => Ok(()),
    }
}

/// One physical expiration that is alive at a given simulated instant, with
/// every rule that asked for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActiveExpiry {
    /// The absolute expiration instant, in UTC.
    pub(crate) expires_at: DateTime<Utc>,
    /// The ids of every rule this expiration satisfies, sorted. A date claimed
    /// by both a weekly and a monthly rule appears once, with both labels.
    pub(crate) labels: Vec<String>,
}

impl ActiveExpiry {
    /// The fractional days remaining until this expiration at `simulated_at`.
    ///
    /// Lives here, next to `expires_at`, because ADR 0001 §7 requires the
    /// snapshot's `days_to_expiration` to be computed from the *same*
    /// `(simulated_at, expires_at)` pair the planner used. Keeping the
    /// conversion beside the pair is what makes that checkable instead of
    /// merely intended.
    ///
    /// The result is strictly positive for any expiration [`RollingPlanner`]
    /// returns, since the planner only ever emits instants after
    /// `simulated_at`.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Internal`] when the interval does not fit in a
    /// `Decimal`, or when `simulated_at` is at or after `expires_at` — which
    /// cannot happen for a planner-produced expiry and therefore signals that
    /// the caller paired an expiry with the wrong instant.
    pub(crate) fn days_to_expiration(
        &self,
        simulated_at: DateTime<Utc>,
    ) -> Result<Decimal, ChainError> {
        let seconds = self
            .expires_at
            .signed_duration_since(simulated_at)
            .num_seconds();
        if seconds <= 0 {
            return Err(ChainError::Internal(format!(
                "expiration {} is not after the simulated instant {simulated_at}",
                self.expires_at
            )));
        }

        Decimal::from(seconds)
            .checked_div(SECONDS_PER_DAY)
            .ok_or_else(|| {
                ChainError::Internal(format!(
                    "days to expiration for {} does not fit in a decimal",
                    self.expires_at
                ))
            })
    }
}

/// Evaluates an [`ExpirationSchedule`] against a simulated instant.
///
/// Borrows the schedule rather than owning it, so a caller that keeps the
/// schedule in its session parameters can build a planner per step without
/// cloning the rules.
///
/// Pure: constructing a planner performs no I/O and evaluating it draws no
/// randomness and reads no clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RollingPlanner<'a> {
    schedule: &'a ExpirationSchedule,
}

impl<'a> RollingPlanner<'a> {
    /// Builds a planner over an already-validated schedule.
    #[must_use]
    pub(crate) fn new(schedule: &'a ExpirationSchedule) -> Self {
        Self { schedule }
    }

    /// The schedule this planner evaluates.
    #[must_use]
    pub(crate) fn schedule(&self) -> &ExpirationSchedule {
        self.schedule
    }

    /// Returns every expiration alive at `simulated_at`, in chronological
    /// order.
    ///
    /// Each rule is first projected to exactly its `target_count` expirations
    /// strictly after `simulated_at`; the union is then deduplicated by
    /// instant, with the labels of the contributing rules merged. So a rule
    /// whose expiration coincides with another rule's still counts it, and the
    /// snapshot prices it once.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Validation`] naming the offending rule when a
    /// projection cannot complete: date arithmetic that would overflow, a local
    /// expiration time that cannot be resolved to an instant even after DST gap
    /// handling, or a scan that exceeds its bound.
    pub(crate) fn active_at(
        &self,
        simulated_at: DateTime<Utc>,
    ) -> Result<Vec<ActiveExpiry>, ChainError> {
        let mut merged: BTreeMap<DateTime<Utc>, BTreeSet<String>> = BTreeMap::new();

        for rule in &self.schedule.rules {
            for expires_at in self.project_rule(rule, simulated_at)? {
                merged
                    .entry(expires_at)
                    .or_default()
                    .insert(rule.rule_id.clone());
            }
        }

        Ok(merged
            .into_iter()
            .map(|(expires_at, labels)| ActiveExpiry {
                expires_at,
                labels: labels.into_iter().collect(),
            })
            .collect())
    }

    /// Projects one rule to exactly `target_count` expirations strictly after
    /// `simulated_at`.
    ///
    /// The accumulator is a set, so the count is a count of **distinct
    /// instants**. That matters for a future calendar whose holiday hook maps
    /// two candidates onto one adjusted date: counting the candidates instead
    /// would satisfy the rule here and then silently lose one expiry to the
    /// deduplication in [`RollingPlanner::active_at`], breaking the invariant
    /// that no step is ever short.
    fn project_rule(
        &self,
        rule: &ExpiryRule,
        simulated_at: DateTime<Utc>,
    ) -> Result<BTreeSet<DateTime<Utc>>, ChainError> {
        let wanted = rule.target_count.get();
        let start = simulated_at
            .with_timezone(&self.schedule.timezone)
            .date_naive();
        let mut found: BTreeSet<DateTime<Utc>> = BTreeSet::new();

        match &rule.kind {
            ExpiryRuleKind::Daily => {
                self.scan_days(rule, start, simulated_at, wanted, &mut found, |_| true)?;
            }
            ExpiryRuleKind::Weekly { weekdays } => {
                // `chrono::Weekday` is not `Ord`, and the set holds at most
                // five entries, so a linear scan over the normalised slice is
                // both simpler and cheaper than any keyed collection.
                self.scan_days(rule, start, simulated_at, wanted, &mut found, |date| {
                    weekdays.contains(&date.weekday())
                })?;
            }
            ExpiryRuleKind::Monthly { weekday } => {
                self.scan_periods(rule, simulated_at, wanted, &mut found, |index| {
                    let (year, month) = add_months(start.year(), start.month(), index)
                        .map_err(|reason| projection_error(rule, reason))?;
                    last_weekday_of_month(rule, year, month, *weekday).map(Some)
                })?;
            }
            ExpiryRuleKind::Yearly { weekday, month } => {
                self.scan_periods(rule, simulated_at, wanted, &mut found, |index| {
                    let year = add_years(start.year(), index)
                        .map_err(|reason| projection_error(rule, reason))?;
                    last_weekday_of_month(rule, year, *month, *weekday).map(Some)
                })?;
            }
        }

        if found.len() < wanted {
            return Err(projection_error(
                rule,
                format!("could only project {} of {wanted} expirations", found.len()),
            ));
        }
        Ok(found)
    }

    /// Walks forward one local date at a time, collecting the expirations of
    /// every eligible date the predicate accepts.
    ///
    /// The scan is bounded: a rule asking for `n` expirations may inspect at
    /// most `8n + 32` days, which comfortably covers a weekly rule naming a
    /// single weekday while keeping a pathological schedule from looping.
    fn scan_days<F>(
        &self,
        rule: &ExpiryRule,
        start: NaiveDate,
        simulated_at: DateTime<Utc>,
        wanted: usize,
        found: &mut BTreeSet<DateTime<Utc>>,
        accepts: F,
    ) -> Result<(), ChainError>
    where
        F: Fn(NaiveDate) -> bool,
    {
        let budget = wanted
            .checked_mul(8)
            .and_then(|days| days.checked_add(DAY_SCAN_SLACK))
            .ok_or_else(|| projection_error(rule, "day scan budget overflows".to_string()))?;

        let mut date = start;
        for _ in 0..budget {
            if accepts(date)
                && let Some(eligible) = self.schedule.calendar.eligible_date(date)
            {
                let expires_at = self.instant_for(rule, eligible)?;
                if expires_at > simulated_at {
                    found.insert(expires_at);
                }
            }

            // Check completion BEFORE advancing, so a rule whose last
            // expiration falls on the maximum representable date is not failed
            // by an increment it no longer needs.
            if found.len() >= wanted {
                return Ok(());
            }

            date = date
                .checked_add_days(Days::new(1))
                .ok_or_else(|| projection_error(rule, "date arithmetic overflows".to_string()))?;
        }

        Ok(())
    }

    /// Walks forward one period (month or year) at a time, collecting the
    /// expiration each period yields.
    ///
    /// The scan is bounded at `n + 2` periods: at most one leading period is
    /// already past at `simulated_at`, and the extra slot leaves room for that
    /// without letting the loop run on.
    fn scan_periods<F>(
        &self,
        rule: &ExpiryRule,
        simulated_at: DateTime<Utc>,
        wanted: usize,
        found: &mut BTreeSet<DateTime<Utc>>,
        date_for: F,
    ) -> Result<(), ChainError>
    where
        F: Fn(u32) -> Result<Option<NaiveDate>, ChainError>,
    {
        let budget = wanted
            .checked_add(PERIOD_SCAN_SLACK)
            .ok_or_else(|| projection_error(rule, "period scan budget overflows".to_string()))?;
        let budget = u32::try_from(budget)
            .map_err(|_| projection_error(rule, "period scan budget overflows".to_string()))?;

        for index in 0..budget {
            if found.len() >= wanted {
                return Ok(());
            }

            let Some(date) = date_for(index)? else {
                continue;
            };
            let Some(eligible) = self.schedule.calendar.eligible_date(date) else {
                continue;
            };

            let expires_at = self.instant_for(rule, eligible)?;
            if expires_at > simulated_at {
                found.insert(expires_at);
            }
        }

        Ok(())
    }

    /// Resolves a local expiration date to its absolute UTC instant.
    ///
    /// The two irregular DST cases are resolved explicitly, because the replay
    /// guarantee needs the same local time to always map to the same instant:
    ///
    /// - a **fold**, where the local time occurs twice, takes the **earlier**
    ///   instant;
    /// - a **gap**, where the local time never occurs, takes the first instant
    ///   after the gap, read from [`GapInfo`]. Reading it from `GapInfo` rather
    ///   than adding a fixed hour is what makes this correct for the
    ///   thirty-minute transitions used by zones such as
    ///   `Australia/Lord_Howe`.
    fn instant_for(&self, rule: &ExpiryRule, date: NaiveDate) -> Result<DateTime<Utc>, ChainError> {
        let local = date.and_time(self.schedule.expiration_time);

        if let Some(resolved) = self
            .schedule
            .timezone
            .from_local_datetime(&local)
            .earliest()
        {
            return Ok(resolved.with_timezone(&Utc));
        }

        GapInfo::new(&local, &self.schedule.timezone)
            .and_then(|gap| gap.end)
            .map(|end| end.with_timezone(&Utc))
            .ok_or_else(|| {
                projection_error(
                    rule,
                    format!(
                        "local time {local} does not exist in {} and no end of the transition gap is known",
                        self.schedule.timezone.name()
                    ),
                )
            })
    }
}

/// Builds the error a failed projection reports, naming the rule that failed.
#[cold]
fn projection_error(rule: &ExpiryRule, reason: String) -> ChainError {
    ChainError::Validation {
        field: format!("schedules.{}", rule.rule_id),
        reason,
    }
}

/// Advances `(year, month)` by `offset` months, with checked arithmetic.
///
/// Returns the failure reason rather than a `ChainError`, so the caller can
/// attach the rule that was being projected.
fn add_months(year: i32, month: u32, offset: u32) -> Result<(i32, u32), String> {
    let zero_based = month.checked_sub(1).ok_or("month underflows")?;
    let total = zero_based
        .checked_add(offset)
        .ok_or("month arithmetic overflows")?;

    let years = i32::try_from(total / 12).map_err(|_| "year arithmetic overflows")?;
    let year = year.checked_add(years).ok_or("year arithmetic overflows")?;
    let month = (total % 12)
        .checked_add(1)
        .ok_or("month arithmetic overflows")?;

    Ok((year, month))
}

/// Advances `year` by `offset` years, with checked arithmetic.
///
/// Returns the failure reason rather than a `ChainError`, so the caller can
/// attach the rule that was being projected.
fn add_years(year: i32, offset: u32) -> Result<i32, String> {
    let offset = i32::try_from(offset).map_err(|_| "year arithmetic overflows")?;
    year.checked_add(offset)
        .ok_or_else(|| "year arithmetic overflows".to_string())
}

/// The date of the last `weekday` in the given month.
///
/// Walks back from the last day of the month to the most recent occurrence of
/// `weekday`, so "last Friday of February 2028" lands on the 25th of a
/// twenty-nine-day month without any special-casing.
fn last_weekday_of_month(
    rule: &ExpiryRule,
    year: i32,
    month: u32,
    weekday: Weekday,
) -> Result<NaiveDate, ChainError> {
    let (next_year, next_month) =
        add_months(year, month, 1).map_err(|reason| projection_error(rule, reason))?;
    let first_of_next = NaiveDate::from_ymd_opt(next_year, next_month, 1).ok_or_else(|| {
        projection_error(
            rule,
            format!("{next_year}-{next_month:02} is outside the representable date range"),
        )
    })?;
    let last_of_month = first_of_next
        .checked_sub_days(Days::new(1))
        .ok_or_else(|| projection_error(rule, "date arithmetic underflows".to_string()))?;

    // Days to step back from the month's last day to the wanted weekday, in
    // `0..=6`. Both operands are Monday-based indices, so adding seven before
    // the remainder keeps the subtraction non-negative without a cast.
    let back =
        (last_of_month.weekday().num_days_from_monday() + 7 - weekday.num_days_from_monday()) % 7;

    last_of_month
        .checked_sub_days(Days::new(u64::from(back)))
        .ok_or_else(|| projection_error(rule, "date arithmetic underflows".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Africa::Cairo;
    use chrono_tz::America::New_York;
    use chrono_tz::Australia::Lord_Howe;
    use chrono_tz::Europe::Madrid;
    use rust_decimal_macros::dec;

    /// 17:00, the local expiration time used by the reference configuration.
    fn at_1700() -> NaiveTime {
        match NaiveTime::from_hms_opt(17, 0, 0) {
            Some(time) => time,
            None => panic!("17:00:00 must be a valid time"),
        }
    }

    /// Builds a UTC instant from its parts, for terse test setup.
    fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        match Utc
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
        {
            Some(instant) => instant,
            None => panic!(
                "{year}-{month:02}-{day:02} {hour:02}:{minute:02} must be a valid UTC instant"
            ),
        }
    }

    /// Builds a date, failing the test rather than panicking deep in a helper.
    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        match NaiveDate::from_ymd_opt(year, month, day) {
            Some(date) => date,
            None => panic!("{year}-{month:02}-{day:02} must be a valid date"),
        }
    }

    /// Builds a rule, failing the test on an invalid literal.
    fn rule(id: &str, kind: ExpiryRuleKind, count: usize) -> ExpiryRule {
        match ExpiryRule::new(id, kind, count) {
            Ok(rule) => rule,
            Err(error) => panic!("test rule must be valid: {error}"),
        }
    }

    /// Builds a New York schedule expiring at 17:00 local.
    fn ny_schedule(rules: Vec<ExpiryRule>) -> ExpirationSchedule {
        match ExpirationSchedule::new(CalendarVersion::WeekdaysV1, New_York, at_1700(), rules) {
            Ok(schedule) => schedule,
            Err(error) => panic!("test schedule must be valid: {error}"),
        }
    }

    /// The full reference configuration from ADR 0001 §14.
    fn reference_schedule() -> ExpirationSchedule {
        ny_schedule(vec![
            rule("zero_dte", ExpiryRuleKind::Daily, 1),
            rule(
                "weeklies",
                ExpiryRuleKind::weekly([Weekday::Mon, Weekday::Wed, Weekday::Fri]),
                3,
            ),
            rule(
                "monthlies",
                ExpiryRuleKind::Monthly {
                    weekday: Weekday::Fri,
                },
                12,
            ),
        ])
    }

    fn active(schedule: &ExpirationSchedule, at: DateTime<Utc>) -> Vec<ActiveExpiry> {
        match RollingPlanner::new(schedule).active_at(at) {
            Ok(expiries) => expiries,
            Err(error) => panic!("planner must project: {error}"),
        }
    }

    /// Resolves one local date through the planner's DST handling.
    fn instant_of(schedule: &ExpirationSchedule, day: NaiveDate) -> DateTime<Utc> {
        let planner = RollingPlanner::new(schedule);
        let probe = rule("probe", ExpiryRuleKind::Daily, 1);
        match planner.instant_for(&probe, day) {
            Ok(instant) => instant,
            Err(error) => panic!("local time must resolve: {error}"),
        }
    }

    /// Builds a single-rule schedule in `zone` expiring at `time` local.
    fn schedule_in(zone: Tz, time: NaiveTime) -> ExpirationSchedule {
        match ExpirationSchedule::new(
            CalendarVersion::WeekdaysV1,
            zone,
            time,
            vec![rule("probe", ExpiryRuleKind::Daily, 1)],
        ) {
            Ok(schedule) => schedule,
            Err(error) => panic!("test schedule must be valid: {error}"),
        }
    }

    // ---- cutoff boundary -------------------------------------------------

    /// One minute before the cutoff the same-day 0DTE is still present, and it
    /// is the nearest expiration.
    #[test]
    fn test_daily_before_cutoff_keeps_same_day_expiry() {
        let schedule = ny_schedule(vec![rule("zero_dte", ExpiryRuleKind::Daily, 1)]);

        // Monday 2026-01-05 16:59 New York = 21:59 UTC (EST).
        let expiries = active(&schedule, utc(2026, 1, 5, 21, 59));

        assert_eq!(expiries.len(), 1);
        assert_eq!(expiries[0].expires_at, utc(2026, 1, 5, 22, 0));
        assert_eq!(expiries[0].labels, vec!["zero_dte".to_string()]);
    }

    /// At exactly the cutoff the expiry is gone and its replacement is present
    /// in the same result — `expires_at <= simulated_at` is expired.
    #[test]
    fn test_daily_at_cutoff_rolls_to_next_eligible_day() {
        let schedule = ny_schedule(vec![rule("zero_dte", ExpiryRuleKind::Daily, 1)]);

        let expiries = active(&schedule, utc(2026, 1, 5, 22, 0));

        assert_eq!(expiries.len(), 1);
        // Tuesday 2026-01-06 17:00 New York.
        assert_eq!(expiries[0].expires_at, utc(2026, 1, 6, 22, 0));
    }

    /// One second after the cutoff behaves exactly like the cutoff itself.
    #[test]
    fn test_daily_after_cutoff_matches_cutoff_behaviour() {
        let schedule = ny_schedule(vec![rule("zero_dte", ExpiryRuleKind::Daily, 1)]);

        let at_cutoff = active(&schedule, utc(2026, 1, 5, 22, 0));
        let after_cutoff = active(
            &schedule,
            utc(2026, 1, 5, 22, 0) + chrono::Duration::seconds(1),
        );

        assert_eq!(at_cutoff, after_cutoff);
    }

    // ---- weekends and rolling -------------------------------------------

    /// After Friday's cutoff the 0DTE rolls to Monday: `weekdays_v1` has no
    /// eligible weekend day.
    #[test]
    fn test_daily_rolls_over_the_weekend_to_monday() {
        let schedule = ny_schedule(vec![rule("zero_dte", ExpiryRuleKind::Daily, 1)]);

        // Friday 2026-01-09 17:00 New York, at the cutoff.
        let expiries = active(&schedule, utc(2026, 1, 9, 22, 0));

        assert_eq!(expiries.len(), 1);
        // Monday 2026-01-12, not Saturday the 10th.
        assert_eq!(expiries[0].expires_at, utc(2026, 1, 12, 22, 0));
    }

    /// A daily rule keeping five expirations spans a weekend without ever
    /// producing a Saturday or a Sunday.
    #[test]
    fn test_daily_never_emits_a_weekend_expiry() {
        let schedule = ny_schedule(vec![rule("zero_dte", ExpiryRuleKind::Daily, 5)]);

        let expiries = active(&schedule, utc(2026, 1, 8, 12, 0));

        assert_eq!(expiries.len(), 5);
        for expiry in &expiries {
            let weekday = expiry.expires_at.with_timezone(&New_York).weekday();
            assert!(
                !matches!(weekday, Weekday::Sat | Weekday::Sun),
                "unexpected weekend expiry {}",
                expiry.expires_at
            );
        }
    }

    // ---- weekly ----------------------------------------------------------

    /// A Monday/Wednesday/Friday rule keeping three always returns the next
    /// three rule expirations, in order.
    #[test]
    fn test_weekly_returns_the_next_three_rule_expirations() {
        let schedule = ny_schedule(vec![rule(
            "weeklies",
            ExpiryRuleKind::weekly([Weekday::Mon, Weekday::Wed, Weekday::Fri]),
            3,
        )]);

        // Monday 2026-01-05 09:30 New York.
        let expiries = active(&schedule, utc(2026, 1, 5, 14, 30));

        let dates: Vec<DateTime<Utc>> = expiries.iter().map(|e| e.expires_at).collect();
        assert_eq!(
            dates,
            vec![
                utc(2026, 1, 5, 22, 0), // Mon
                utc(2026, 1, 7, 22, 0), // Wed
                utc(2026, 1, 9, 22, 0), // Fri
            ]
        );
    }

    /// Crossing Monday's cutoff replenishes the weekly inventory to three in
    /// the same result — there is no step with only two.
    #[test]
    fn test_weekly_replenishes_in_the_same_result() {
        let schedule = ny_schedule(vec![rule(
            "weeklies",
            ExpiryRuleKind::weekly([Weekday::Mon, Weekday::Wed, Weekday::Fri]),
            3,
        )]);

        let expiries = active(&schedule, utc(2026, 1, 5, 22, 0));

        let dates: Vec<DateTime<Utc>> = expiries.iter().map(|e| e.expires_at).collect();
        assert_eq!(
            dates,
            vec![
                utc(2026, 1, 7, 22, 0),  // Wed
                utc(2026, 1, 9, 22, 0),  // Fri
                utc(2026, 1, 12, 22, 0), // next Mon
            ]
        );
    }

    /// A weekly rule naming a single weekday still reaches a large count
    /// within its scan budget.
    #[test]
    fn test_weekly_single_weekday_reaches_a_large_count() {
        let schedule = ny_schedule(vec![rule(
            "weeklies",
            ExpiryRuleKind::weekly([Weekday::Fri]),
            52,
        )]);

        let expiries = active(&schedule, utc(2026, 1, 5, 14, 30));

        assert_eq!(expiries.len(), 52);
        for expiry in &expiries {
            assert_eq!(
                expiry.expires_at.with_timezone(&New_York).weekday(),
                Weekday::Fri
            );
        }
    }

    /// The weekday set is normalised — deduplicated and Monday-first — so the
    /// order a client submits cannot change the schedule.
    #[test]
    fn test_weekly_weekday_set_is_normalised() {
        let kind = ExpiryRuleKind::weekly([Weekday::Fri, Weekday::Mon, Weekday::Fri]);

        match kind {
            ExpiryRuleKind::Weekly { weekdays } => {
                assert_eq!(weekdays, vec![Weekday::Mon, Weekday::Fri]);
            }
            other => panic!("expected a weekly rule, got {other:?}"),
        }
    }

    // ---- monthly ---------------------------------------------------------

    /// Twelve last-Friday monthlies stay twelve, and start where ADR 0001 §14.3
    /// says they do.
    #[test]
    fn test_monthly_last_friday_projects_twelve() {
        let schedule = ny_schedule(vec![rule(
            "monthlies",
            ExpiryRuleKind::Monthly {
                weekday: Weekday::Fri,
            },
            12,
        )]);

        let expiries = active(&schedule, utc(2026, 1, 5, 14, 30));

        assert_eq!(expiries.len(), 12);
        assert_eq!(expiries[0].expires_at, utc(2026, 1, 30, 22, 0));
        assert_eq!(expiries[1].expires_at, utc(2026, 2, 27, 22, 0));
        // Late March is EDT, so the same 17:00 local is an hour earlier in UTC.
        assert_eq!(expiries[2].expires_at, utc(2026, 3, 27, 21, 0));
        assert_eq!(expiries[11].expires_at, utc(2026, 12, 25, 22, 0));
    }

    /// The count survives the month boundary: once January's monthly expires,
    /// the twelfth reaches into the next year.
    #[test]
    fn test_monthly_count_survives_month_and_year_boundaries() {
        let schedule = ny_schedule(vec![rule(
            "monthlies",
            ExpiryRuleKind::Monthly {
                weekday: Weekday::Fri,
            },
            12,
        )]);

        let expiries = active(&schedule, utc(2026, 1, 30, 22, 0));

        assert_eq!(expiries.len(), 12);
        assert_eq!(expiries[0].expires_at, utc(2026, 2, 27, 22, 0));
        assert_eq!(expiries[11].expires_at, utc(2027, 1, 29, 22, 0));
    }

    /// A twenty-nine-day February resolves its last Friday like any other
    /// month.
    #[test]
    fn test_monthly_handles_a_leap_year_february() {
        let schedule = ny_schedule(vec![rule(
            "monthlies",
            ExpiryRuleKind::Monthly {
                weekday: Weekday::Fri,
            },
            1,
        )]);

        let expiries = active(&schedule, utc(2028, 2, 1, 14, 30));

        assert_eq!(expiries.len(), 1);
        assert_eq!(expiries[0].expires_at, utc(2028, 2, 25, 22, 0));
    }

    /// The projection is stable across every month of a year: a monthly rule
    /// asked at any point keeps exactly its count.
    #[test]
    fn test_monthly_count_is_stable_across_a_whole_year() {
        let schedule = ny_schedule(vec![rule(
            "monthlies",
            ExpiryRuleKind::Monthly {
                weekday: Weekday::Fri,
            },
            12,
        )]);

        for month in 1..=12 {
            let expiries = active(&schedule, utc(2026, month, 15, 12, 0));
            assert_eq!(expiries.len(), 12, "month {month} lost inventory");
        }
    }

    // ---- yearly / LEAPS --------------------------------------------------

    /// A yearly rule reaches at least a year out, which is what makes
    /// LEAPS-style scenarios expressible.
    #[test]
    fn test_yearly_reaches_beyond_one_year() {
        let schedule = ny_schedule(vec![rule(
            "leaps",
            ExpiryRuleKind::Yearly {
                weekday: Weekday::Fri,
                month: 12,
            },
            2,
        )]);

        let simulated_at = utc(2026, 1, 5, 14, 30);
        let expiries = active(&schedule, simulated_at);

        assert_eq!(expiries.len(), 2);
        assert_eq!(expiries[0].expires_at, utc(2026, 12, 25, 22, 0));
        assert_eq!(expiries[1].expires_at, utc(2027, 12, 31, 22, 0));
        assert!(expiries[1].expires_at - simulated_at > chrono::Duration::days(365));
    }

    /// A yearly rule whose month has already passed this year starts from next
    /// year rather than returning nothing.
    #[test]
    fn test_yearly_skips_a_month_already_past() {
        let schedule = ny_schedule(vec![rule(
            "leaps",
            ExpiryRuleKind::Yearly {
                weekday: Weekday::Fri,
                month: 1,
            },
            1,
        )]);

        let expiries = active(&schedule, utc(2026, 6, 1, 12, 0));

        assert_eq!(expiries.len(), 1);
        assert_eq!(expiries[0].expires_at, utc(2027, 1, 29, 22, 0));
    }

    // ---- overlap and deduplication --------------------------------------

    /// A date claimed by two rules appears once, carrying both labels — and
    /// each rule still counted it towards its own target.
    #[test]
    fn test_coincident_expiry_is_deduplicated_and_carries_both_labels() {
        let schedule = reference_schedule();

        let expiries = active(&schedule, utc(2026, 1, 5, 14, 30));

        // 1 + 3 + 12 = 16 rule slots, but Monday's 0DTE and the first weekly
        // are the same physical expiration, so 15 chains.
        assert_eq!(expiries.len(), 15);

        let first = &expiries[0];
        assert_eq!(first.expires_at, utc(2026, 1, 5, 22, 0));
        assert_eq!(
            first.labels,
            vec!["weeklies".to_string(), "zero_dte".to_string()]
        );
    }

    /// Deduplication does not starve a rule: each rule still has its full
    /// count once the shared expirations are attributed back.
    #[test]
    fn test_every_rule_keeps_its_full_count_after_deduplication() {
        let schedule = reference_schedule();

        let expiries = active(&schedule, utc(2026, 1, 5, 14, 30));

        for (rule_id, expected) in [("zero_dte", 1), ("weeklies", 3), ("monthlies", 12)] {
            let count = expiries
                .iter()
                .filter(|expiry| expiry.labels.iter().any(|label| label == rule_id))
                .count();
            assert_eq!(count, expected, "rule {rule_id} lost inventory to dedup");
        }
    }

    /// Results are chronologically ordered and free of duplicate instants.
    #[test]
    fn test_results_are_chronological_and_unique() {
        let schedule = reference_schedule();

        let expiries = active(&schedule, utc(2026, 1, 5, 14, 30));

        assert!(!expiries.is_empty());
        for pair in expiries.windows(2) {
            assert!(
                pair[0].expires_at < pair[1].expires_at,
                "results must be strictly increasing"
            );
        }
    }

    // ---- days to expiration ---------------------------------------------

    /// The fractional days-to-expiration is computed from the same pair the
    /// planner used: seven and a half hours is 0.3125 days.
    #[test]
    fn test_days_to_expiration_is_fractional_and_exact() {
        let schedule = ny_schedule(vec![rule("zero_dte", ExpiryRuleKind::Daily, 1)]);
        let simulated_at = utc(2026, 1, 6, 14, 30);

        let expiries = active(&schedule, simulated_at);
        assert_eq!(expiries.len(), 1);

        match expiries[0].days_to_expiration(simulated_at) {
            Ok(days) => assert_eq!(days, dec!(0.3125)),
            Err(error) => panic!("must compute days to expiration: {error}"),
        }
    }

    /// Every expiration the planner returns is strictly in the future, so its
    /// days-to-expiration is strictly positive.
    #[test]
    fn test_days_to_expiration_is_positive_for_every_active_expiry() {
        let schedule = reference_schedule();
        let simulated_at = utc(2026, 1, 5, 14, 30);

        for expiry in active(&schedule, simulated_at) {
            match expiry.days_to_expiration(simulated_at) {
                Ok(days) => assert!(days > Decimal::ZERO, "non-positive DTE for {expiry:?}"),
                Err(error) => panic!("must compute days to expiration: {error}"),
            }
        }
    }

    /// Pairing an expiry with an instant at or after it is a typed error, not a
    /// negative DTE that would silently reach the pricing model.
    #[test]
    fn test_days_to_expiration_rejects_a_non_future_instant() {
        let expiry = ActiveExpiry {
            expires_at: utc(2026, 1, 5, 22, 0),
            labels: vec!["zero_dte".to_string()],
        };

        match expiry.days_to_expiration(utc(2026, 1, 5, 22, 0)) {
            Err(ChainError::Internal(reason)) => assert!(reason.contains("not after")),
            other => panic!("expected an internal error, got {other:?}"),
        }
    }

    // ---- DST -------------------------------------------------------------

    /// A local expiration time inside a spring-forward gap resolves to the
    /// first instant after the gap, not to a time that never happened.
    ///
    /// Madrid springs forward on 2026-03-29 at 02:00 local, so 02:30 does not
    /// exist that day. 02:00 local CET is 01:00 UTC and the clock jumps
    /// straight to 03:00 CEST, so the first instant after the gap is 01:00 UTC.
    #[test]
    fn test_dst_gap_resolves_to_the_first_instant_after_the_gap() {
        let time = match NaiveTime::from_hms_opt(2, 30, 0) {
            Some(time) => time,
            None => panic!("02:30:00 must be a valid time"),
        };
        let schedule = schedule_in(Madrid, time);

        assert_eq!(
            instant_of(&schedule, date(2026, 3, 29)),
            utc(2026, 3, 29, 1, 0)
        );
    }

    /// A local expiration time inside an autumn fold occurs twice; the earlier
    /// instant is chosen, deterministically.
    ///
    /// Madrid falls back on 2026-10-25 at 03:00 local, so 02:30 occurs once at
    /// 00:30 UTC (CEST) and again at 01:30 UTC (CET).
    #[test]
    fn test_dst_fold_resolves_to_the_earlier_instant() {
        let time = match NaiveTime::from_hms_opt(2, 30, 0) {
            Some(time) => time,
            None => panic!("02:30:00 must be a valid time"),
        };
        let schedule = schedule_in(Madrid, time);

        assert_eq!(
            instant_of(&schedule, date(2026, 10, 25)),
            utc(2026, 10, 25, 0, 30)
        );
    }

    /// The gap rule is shift-agnostic: Lord Howe Island jumps thirty minutes,
    /// and reading the end of the gap handles it without any special case.
    ///
    /// Lord Howe springs forward on 2026-10-04 at 02:00 local by thirty
    /// minutes, so 02:15 does not exist; the gap ends at 02:30 local, which is
    /// 15:30 UTC on the previous day.
    #[test]
    fn test_dst_gap_handles_a_thirty_minute_transition() {
        let time = match NaiveTime::from_hms_opt(2, 15, 0) {
            Some(time) => time,
            None => panic!("02:15:00 must be a valid time"),
        };
        let schedule = schedule_in(Lord_Howe, time);

        assert_eq!(
            instant_of(&schedule, date(2026, 10, 4)),
            utc(2026, 10, 3, 15, 30)
        );
    }

    /// The gap branch is reachable through the public path, not only through
    /// the private helper.
    ///
    /// Most northern-hemisphere transitions land on a Sunday, which
    /// `weekdays_v1` rejects before the local time is ever resolved. Cairo does
    /// not: it springs forward on **Friday** 2026-04-24 at 00:00 local, so a
    /// 00:30 expiration that day does not exist and `active_at` itself has to
    /// resolve the gap. The gap ends at 01:00 local EEST, which is
    /// 2026-04-23T22:00:00Z.
    #[test]
    fn test_dst_gap_is_resolved_through_the_public_projection_path() {
        let time = match NaiveTime::from_hms_opt(0, 30, 0) {
            Some(time) => time,
            None => panic!("00:30:00 must be a valid time"),
        };
        let schedule = schedule_in(Cairo, time);

        let expiries = active(&schedule, utc(2026, 4, 23, 12, 0));

        assert_eq!(expiries.len(), 1);
        assert_eq!(expiries[0].expires_at, utc(2026, 4, 23, 22, 0));
        assert_eq!(
            expiries[0].expires_at.with_timezone(&Cairo).weekday(),
            Weekday::Fri
        );
    }

    /// Crossing a DST boundary moves the absolute instant but never the local
    /// expiration time.
    #[test]
    fn test_local_expiration_time_is_stable_across_a_dst_boundary() {
        let schedule = ny_schedule(vec![rule(
            "monthlies",
            ExpiryRuleKind::Monthly {
                weekday: Weekday::Fri,
            },
            4,
        )]);

        let expiries = active(&schedule, utc(2026, 1, 5, 14, 30));

        assert_eq!(expiries.len(), 4);
        for expiry in &expiries {
            let local = expiry.expires_at.with_timezone(&New_York);
            assert_eq!(local.time(), at_1700(), "local expiration time drifted");
        }
    }

    // ---- determinism -----------------------------------------------------

    /// The same inputs produce the same output, call after call.
    #[test]
    fn test_projection_is_stable_across_repeated_calls() {
        let schedule = reference_schedule();
        let at = utc(2026, 1, 5, 14, 30);

        assert_eq!(active(&schedule, at), active(&schedule, at));
    }

    /// Reordering the rules cannot change the result: the schedule is
    /// normalised and the output is keyed by instant.
    #[test]
    fn test_rule_order_does_not_change_the_result() {
        let forwards = reference_schedule();
        let backwards = ny_schedule(vec![
            rule(
                "monthlies",
                ExpiryRuleKind::Monthly {
                    weekday: Weekday::Fri,
                },
                12,
            ),
            rule(
                "weeklies",
                ExpiryRuleKind::weekly([Weekday::Fri, Weekday::Wed, Weekday::Mon]),
                3,
            ),
            rule("zero_dte", ExpiryRuleKind::Daily, 1),
        ]);

        let at = utc(2026, 1, 5, 14, 30);
        assert_eq!(active(&forwards, at), active(&backwards, at));
    }

    /// The result depends only on the simulated instant, not on the zone the
    /// caller happens to express it in.
    #[test]
    fn test_result_depends_only_on_the_absolute_instant() {
        let schedule = reference_schedule();
        let as_utc = utc(2026, 1, 5, 14, 30);
        let same_instant_elsewhere = as_utc.with_timezone(&Madrid).with_timezone(&Utc);

        assert_eq!(
            active(&schedule, as_utc),
            active(&schedule, same_instant_elsewhere)
        );
    }

    // ---- validation ------------------------------------------------------

    /// A zero `target_count` is rejected at construction, naming the field.
    #[test]
    fn test_zero_target_count_is_rejected() {
        match ExpiryRule::new("zero_dte", ExpiryRuleKind::Daily, 0) {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "schedules.zero_dte.target_count");
                assert!(reason.contains("at least 1"));
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// A `target_count` above the cap is rejected.
    #[test]
    fn test_excessive_target_count_is_rejected() {
        match ExpiryRule::new("zero_dte", ExpiryRuleKind::Daily, MAX_TARGET_COUNT + 1) {
            Err(ChainError::Validation { field, .. }) => {
                assert_eq!(field, "schedules.zero_dte.target_count");
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// Duplicate rule ids are rejected: labels would otherwise be ambiguous.
    #[test]
    fn test_duplicate_rule_id_is_rejected() {
        match ExpirationSchedule::new(
            CalendarVersion::WeekdaysV1,
            New_York,
            at_1700(),
            vec![
                rule("same", ExpiryRuleKind::Daily, 1),
                rule("same", ExpiryRuleKind::weekly([Weekday::Mon]), 1),
            ],
        ) {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "schedules.same.rule_id");
                assert!(reason.contains("unique"));
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// An empty rule id is rejected.
    #[test]
    fn test_empty_rule_id_is_rejected() {
        match ExpiryRule::new("", ExpiryRuleKind::Daily, 1) {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "schedules.rule_id");
                assert!(reason.contains("empty"));
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// A rule id longer than the cap is rejected.
    #[test]
    fn test_overlong_rule_id_is_rejected() {
        let long = "a".repeat(MAX_RULE_ID_LEN + 1);

        match ExpiryRule::new(long, ExpiryRuleKind::Daily, 1) {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "schedules.rule_id");
                assert!(reason.contains("characters"));
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// A rule id carrying the CSV label separator is rejected, so the export
    /// column cannot be corrupted by a schedule.
    #[test]
    fn test_rule_id_with_a_label_separator_is_rejected() {
        match ExpiryRule::new("weekly|monthly", ExpiryRuleKind::Daily, 1) {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "schedules.rule_id");
                assert!(reason.contains("[A-Za-z0-9_-]"));
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// An empty schedule is rejected rather than producing empty snapshots.
    #[test]
    fn test_empty_schedule_is_rejected() {
        match ExpirationSchedule::new(CalendarVersion::WeekdaysV1, New_York, at_1700(), vec![]) {
            Err(ChainError::Validation { field, .. }) => assert_eq!(field, "schedules"),
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// More rules than the cap is rejected.
    #[test]
    fn test_too_many_rules_is_rejected() {
        let rules = (0..=MAX_SCHEDULE_RULES)
            .map(|index| rule(&format!("rule_{index}"), ExpiryRuleKind::Daily, 1))
            .collect();

        match ExpirationSchedule::new(CalendarVersion::WeekdaysV1, New_York, at_1700(), rules) {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "schedules");
                assert!(reason.contains("rules"));
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// A total inventory above the per-snapshot cap is rejected at
    /// construction, using the pre-deduplication sum.
    #[test]
    fn test_excessive_total_inventory_is_rejected() {
        let rules = (0..4)
            .map(|index| {
                rule(
                    &format!("rule_{index}"),
                    ExpiryRuleKind::Daily,
                    MAX_TARGET_COUNT,
                )
            })
            .collect();

        match ExpirationSchedule::new(CalendarVersion::WeekdaysV1, New_York, at_1700(), rules) {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "schedules");
                assert!(reason.contains("total expiration count"));
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// A weekly rule naming no weekday is rejected.
    #[test]
    fn test_weekly_without_weekdays_is_rejected() {
        match ExpiryRule::new("weeklies", ExpiryRuleKind::weekly([]), 1) {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "schedules.weeklies.weekly.weekdays");
                assert!(reason.contains("at least one weekday"));
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// A weekly rule naming a weekend day is rejected rather than silently
    /// producing nothing.
    #[test]
    fn test_weekly_naming_a_weekend_day_is_rejected() {
        match ExpiryRule::new(
            "weeklies",
            ExpiryRuleKind::weekly([Weekday::Mon, Weekday::Sat]),
            1,
        ) {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "schedules.weeklies.weekly.weekdays");
                assert!(reason.contains("Sat"));
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// A monthly rule naming a weekend day is rejected.
    #[test]
    fn test_monthly_naming_a_weekend_day_is_rejected() {
        match ExpiryRule::new(
            "monthlies",
            ExpiryRuleKind::Monthly {
                weekday: Weekday::Sun,
            },
            1,
        ) {
            Err(ChainError::Validation { field, .. }) => {
                assert_eq!(field, "schedules.monthlies.monthly.weekday");
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// A yearly rule naming a weekend day is rejected, like its monthly
    /// counterpart.
    #[test]
    fn test_yearly_naming_a_weekend_day_is_rejected() {
        match ExpiryRule::new(
            "leaps",
            ExpiryRuleKind::Yearly {
                weekday: Weekday::Sat,
                month: 12,
            },
            1,
        ) {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "schedules.leaps.yearly.weekday");
                assert!(reason.contains("Sat"));
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    /// A yearly rule naming a month outside `1..=12` is rejected.
    #[test]
    fn test_yearly_with_an_invalid_month_is_rejected() {
        match ExpiryRule::new(
            "leaps",
            ExpiryRuleKind::Yearly {
                weekday: Weekday::Fri,
                month: 13,
            },
            1,
        ) {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "schedules.leaps.yearly.month");
                assert!(reason.contains("between 1 and 12"));
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    // ---- arithmetic ------------------------------------------------------

    /// Month arithmetic that would leave the representable year range is a
    /// typed error, never a wrap.
    #[test]
    fn test_month_arithmetic_overflow_is_a_typed_error() {
        match add_months(i32::MAX, 12, 12) {
            Err(reason) => assert!(reason.contains("overflow")),
            Ok(result) => panic!("expected an overflow error, got {result:?}"),
        }
    }

    /// Year arithmetic that would overflow is a typed error.
    #[test]
    fn test_year_arithmetic_overflow_is_a_typed_error() {
        match add_years(i32::MAX, 1) {
            Err(reason) => assert!(reason.contains("overflow")),
            Ok(result) => panic!("expected an overflow error, got {result:?}"),
        }
    }

    /// Month arithmetic rolls into the next year correctly.
    #[test]
    fn test_month_arithmetic_rolls_into_the_next_year() {
        match add_months(2026, 12, 1) {
            Ok((year, month)) => {
                assert_eq!(year, 2027);
                assert_eq!(month, 1);
            }
            Err(reason) => panic!("must not fail: {reason}"),
        }
    }

    /// A projection that cannot reach beyond the representable date range
    /// reports the rule that failed instead of returning a short inventory.
    #[test]
    fn test_unreachable_projection_reports_the_rule() {
        let schedule = ny_schedule(vec![rule(
            "leaps",
            ExpiryRuleKind::Yearly {
                weekday: Weekday::Fri,
                month: 12,
            },
            200,
        )]);

        // `NaiveDate` tops out well before 200 years past the maximum year, so
        // the projection cannot complete.
        let far_future = match Utc.with_ymd_and_hms(262_000, 1, 1, 0, 0, 0).single() {
            Some(instant) => instant,
            None => panic!("262000-01-01 must be representable"),
        };

        match RollingPlanner::new(&schedule).active_at(far_future) {
            Err(ChainError::Validation { field, .. }) => {
                assert_eq!(field, "schedules.leaps");
            }
            other => panic!("expected a projection error, got {other:?}"),
        }
    }

    // ---- calendar policy -------------------------------------------------

    /// The calendar's eligibility hook rejects weekends and accepts weekdays.
    #[test]
    fn test_weekdays_v1_eligibility_rejects_weekends_only() {
        assert_eq!(
            CalendarVersion::WeekdaysV1.eligible_date(date(2026, 1, 10)),
            None
        );
        assert_eq!(
            CalendarVersion::WeekdaysV1.eligible_date(date(2026, 1, 11)),
            None
        );
        assert_eq!(
            CalendarVersion::WeekdaysV1.eligible_date(date(2026, 1, 12)),
            Some(date(2026, 1, 12))
        );
    }

    /// The tzdb version is exposed and non-empty, so #44 can persist it as a
    /// replay input alongside the seed and the calendar version.
    #[test]
    fn test_tzdb_version_is_exposed_and_non_empty() {
        let version = tzdb_version();

        assert!(!version.is_empty());
        assert!(
            version.starts_with(|c: char| c.is_ascii_digit()),
            "expected an IANA release such as 2025b, got {version:?}"
        );
    }

    /// The calendar version round-trips through serde under its wire name, so
    /// a stored simulation keeps the policy it was created with.
    #[test]
    fn test_calendar_version_serde_round_trip() {
        let json = match serde_json::to_string(&CalendarVersion::WeekdaysV1) {
            Ok(json) => json,
            Err(error) => panic!("must serialize: {error}"),
        };
        assert_eq!(json, "\"weekdays_v1\"");
        assert_eq!(CalendarVersion::WeekdaysV1.as_str(), "weekdays_v1");

        match serde_json::from_str::<CalendarVersion>(&json) {
            Ok(parsed) => assert_eq!(parsed, CalendarVersion::WeekdaysV1),
            Err(error) => panic!("must deserialize: {error}"),
        }
    }

    // ---- stored shape ----------------------------------------------------

    /// The stored shape is pinned: a rule is one flat object tagged by `kind`,
    /// exactly as ADR 0001 §14.2 shows it. Changing this is a semver event for
    /// every persisted v2 simulation, so it is asserted literally.
    #[test]
    fn test_stored_rule_shape_is_flat_and_tagged_by_kind() {
        let monthly = rule(
            "monthlies",
            ExpiryRuleKind::Monthly {
                weekday: Weekday::Fri,
            },
            12,
        );
        let daily = rule("zero_dte", ExpiryRuleKind::Daily, 1);

        match serde_json::to_value(&monthly) {
            Ok(value) => assert_eq!(
                value,
                serde_json::json!({
                    "rule_id": "monthlies",
                    "kind": "monthly",
                    "weekday": "Fri",
                    "target_count": 12
                })
            ),
            Err(error) => panic!("must serialize: {error}"),
        }

        match serde_json::to_value(&daily) {
            Ok(value) => assert_eq!(
                value,
                serde_json::json!({
                    "rule_id": "zero_dte",
                    "kind": "daily",
                    "target_count": 1
                })
            ),
            Err(error) => panic!("must serialize: {error}"),
        }
    }

    /// The whole schedule round-trips through serde, which is what lets #44
    /// persist it as part of the stored v2 parameters.
    #[test]
    fn test_schedule_serde_round_trip() {
        let schedule = reference_schedule();

        let json = match serde_json::to_string(&schedule) {
            Ok(json) => json,
            Err(error) => panic!("must serialize: {error}"),
        };
        match serde_json::from_str::<ExpirationSchedule>(&json) {
            Ok(parsed) => assert_eq!(parsed, schedule),
            Err(error) => panic!("must deserialize: {error}"),
        }
    }

    /// Deserialization runs the same validation as construction, so a stored
    /// schedule cannot smuggle an out-of-range count past the constructor and
    /// into the projection loop.
    #[test]
    fn test_deserialization_rejects_an_out_of_range_target_count() {
        let json = r#"{
            "calendar": "weekdays_v1",
            "timezone": "America/New_York",
            "expiration_time": "17:00:00",
            "rules": [
                { "rule_id": "zero_dte", "kind": "daily", "target_count": 100000000000 }
            ]
        }"#;

        assert!(serde_json::from_str::<ExpirationSchedule>(json).is_err());
    }

    /// Deserialization rejects a weekend weekday just as construction does.
    #[test]
    fn test_deserialization_rejects_a_weekend_weekday() {
        let json = r#"{
            "calendar": "weekdays_v1",
            "timezone": "America/New_York",
            "expiration_time": "17:00:00",
            "rules": [
                { "rule_id": "monthlies", "kind": "monthly", "weekday": "Sat", "target_count": 1 }
            ]
        }"#;

        assert!(serde_json::from_str::<ExpirationSchedule>(json).is_err());
    }

    /// Deserialization rejects an empty rule list.
    #[test]
    fn test_deserialization_rejects_an_empty_schedule() {
        let json = r#"{
            "calendar": "weekdays_v1",
            "timezone": "America/New_York",
            "expiration_time": "17:00:00",
            "rules": []
        }"#;

        assert!(serde_json::from_str::<ExpirationSchedule>(json).is_err());
    }

    /// Deserialization normalises exactly as construction does, so a stored
    /// schedule written in any rule order loads identically.
    #[test]
    fn test_deserialization_normalises_rule_order() {
        let json = r#"{
            "calendar": "weekdays_v1",
            "timezone": "America/New_York",
            "expiration_time": "17:00:00",
            "rules": [
                { "rule_id": "zero_dte", "kind": "daily", "target_count": 1 },
                { "rule_id": "monthlies", "kind": "monthly", "weekday": "Fri", "target_count": 1 }
            ]
        }"#;

        match serde_json::from_str::<ExpirationSchedule>(json) {
            Ok(schedule) => {
                let ids: Vec<&str> = schedule.rules().iter().map(ExpiryRule::rule_id).collect();
                assert_eq!(ids, vec!["monthlies", "zero_dte"]);
            }
            Err(error) => panic!("must deserialize: {error}"),
        }
    }

    /// A weekday set written with duplicates and out of order loads normalised,
    /// so the `Deserialize` path cannot smuggle a form the constructor would
    /// never produce into the store.
    #[test]
    fn test_deserialization_normalises_weekly_weekdays() {
        let json = r#"{
            "calendar": "weekdays_v1",
            "timezone": "America/New_York",
            "expiration_time": "17:00:00",
            "rules": [
                { "rule_id": "weeklies", "kind": "weekly", "target_count": 1,
                  "weekdays": ["Fri", "Mon", "Fri", "Wed"] }
            ]
        }"#;

        match serde_json::from_str::<ExpirationSchedule>(json) {
            Ok(schedule) => match schedule.rules().first().map(ExpiryRule::kind) {
                Some(ExpiryRuleKind::Weekly { weekdays }) => assert_eq!(
                    *weekdays,
                    vec![Weekday::Mon, Weekday::Wed, Weekday::Fri],
                    "the stored set must be deduplicated and Monday-first"
                ),
                other => panic!("must load a weekly rule, got {other:?}"),
            },
            Err(error) => panic!("must deserialize: {error}"),
        }
    }

    /// An unknown key inside a rule is schema drift and is rejected by name,
    /// as ADR 0001 §4.1 requires, rather than silently dropped.
    #[test]
    fn test_deserialization_rejects_an_unknown_rule_field() {
        let json = r#"{
            "calendar": "weekdays_v1",
            "timezone": "America/New_York",
            "expiration_time": "17:00:00",
            "rules": [
                { "rule_id": "zero_dte", "kind": "daily", "target_count": 1,
                  "weekday": "Fri" }
            ]
        }"#;

        match serde_json::from_str::<ExpirationSchedule>(json) {
            Ok(schedule) => panic!("must reject the stray field, got {schedule:?}"),
            Err(error) => assert!(
                error.to_string().contains("weekday"),
                "the error must name the offending field, got {error}"
            ),
        }
    }

    /// The same rejection at the schedule level, where nothing is flattened and
    /// serde's own `deny_unknown_fields` does the work.
    #[test]
    fn test_deserialization_rejects_an_unknown_schedule_field() {
        let json = r#"{
            "calendar": "weekdays_v1",
            "timezone": "America/New_York",
            "expiration_time": "17:00:00",
            "not_a_schedule_field": true,
            "rules": [
                { "rule_id": "zero_dte", "kind": "daily", "target_count": 1 }
            ]
        }"#;

        assert!(serde_json::from_str::<ExpirationSchedule>(json).is_err());
    }

    /// A `yearly` rule may omit `month`, and it loads as December — the default
    /// ADR 0001 §4.1 specifies, and the one
    /// [`ExpiryRuleKind::yearly`] applies on the construction path.
    #[test]
    fn test_deserialization_defaults_the_yearly_month() {
        let json = r#"{
            "calendar": "weekdays_v1",
            "timezone": "America/New_York",
            "expiration_time": "17:00:00",
            "rules": [
                { "rule_id": "leaps", "kind": "yearly", "target_count": 1, "weekday": "Fri" }
            ]
        }"#;

        match serde_json::from_str::<ExpirationSchedule>(json) {
            Ok(schedule) => assert_eq!(
                schedule.rules().first().map(ExpiryRule::kind),
                Some(&ExpiryRuleKind::yearly(Weekday::Fri)),
                "an omitted month must default to December on both paths"
            ),
            Err(error) => panic!("must deserialize: {error}"),
        }
    }

    /// Every kind survives serialize then deserialize unchanged.
    ///
    /// The two directions are separate code paths — Serialize goes through the
    /// flattened internally-tagged enum, Deserialize through
    /// [`ExpiryRuleWire`] — so a field-name drift between them would make a
    /// stored schedule unreadable rather than merely odd.
    #[test]
    fn test_every_rule_kind_survives_a_round_trip() {
        let kinds = vec![
            ExpiryRuleKind::Daily,
            ExpiryRuleKind::weekly([Weekday::Mon, Weekday::Wed, Weekday::Fri]),
            ExpiryRuleKind::Monthly {
                weekday: Weekday::Fri,
            },
            ExpiryRuleKind::yearly(Weekday::Fri),
        ];

        for kind in kinds {
            let schedule = ny_schedule(vec![rule("only", kind.clone(), 1)]);
            let json = match serde_json::to_string(&schedule) {
                Ok(json) => json,
                Err(error) => panic!("must serialize {kind:?}: {error}"),
            };

            match serde_json::from_str::<ExpirationSchedule>(&json) {
                Ok(loaded) => assert_eq!(loaded, schedule, "{kind:?} must round-trip"),
                Err(error) => panic!("must deserialize {kind:?} from {json}: {error}"),
            }
        }
    }

    /// The wire tag and the domain kind stay in step, which is what keeps a
    /// serialised kind readable back.
    #[test]
    fn test_every_kind_maps_to_its_wire_tag() {
        assert_eq!(ExpiryRuleKind::Daily.tag(), ExpiryRuleKindTag::Daily);
        assert_eq!(
            ExpiryRuleKind::weekly([Weekday::Mon]).tag(),
            ExpiryRuleKindTag::Weekly
        );
        assert_eq!(
            ExpiryRuleKind::Monthly {
                weekday: Weekday::Fri
            }
            .tag(),
            ExpiryRuleKindTag::Monthly
        );
        assert_eq!(
            ExpiryRuleKind::yearly(Weekday::Fri).tag(),
            ExpiryRuleKindTag::Yearly
        );
    }

    /// Construction normalises the rule order by id, so the stored schedule
    /// does not depend on the order the client submitted.
    #[test]
    fn test_schedule_rules_are_ordered_by_id() {
        let schedule = ny_schedule(vec![
            rule("zero_dte", ExpiryRuleKind::Daily, 1),
            rule(
                "monthlies",
                ExpiryRuleKind::Monthly {
                    weekday: Weekday::Fri,
                },
                1,
            ),
        ]);

        let ids: Vec<&str> = schedule.rules().iter().map(ExpiryRule::rule_id).collect();
        assert_eq!(ids, vec!["monthlies", "zero_dte"]);
    }

    /// The accessors expose exactly what the schedule was built with.
    #[test]
    fn test_schedule_accessors_expose_the_constructed_values() {
        let schedule = reference_schedule();

        assert_eq!(schedule.calendar(), CalendarVersion::WeekdaysV1);
        assert_eq!(schedule.timezone(), New_York);
        assert_eq!(schedule.expiration_time(), at_1700());
        assert_eq!(schedule.rules().len(), 3);

        let planner = RollingPlanner::new(&schedule);
        assert_eq!(planner.schedule(), &schedule);

        let first = match schedule.rules().first() {
            Some(rule) => rule,
            None => panic!("the reference schedule has three rules"),
        };
        assert_eq!(first.rule_id(), "monthlies");
        assert_eq!(first.target_count().get(), 12);
        assert_eq!(
            first.kind(),
            &ExpiryRuleKind::Monthly {
                weekday: Weekday::Fri
            }
        );
    }
}
