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
//! never change that stream either. A given `(schedule, simulated_at)` pair
//! always yields the same inventory, on any machine, in any locale, at any
//! point in the process's life.
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

// The planner is the bottom of the v0.2.0 stack and has no in-tree caller yet:
// issue #44 persists an `ExpirationSchedule` in the v2 session parameters and
// #46 calls `RollingPlanner::active_at` to build each snapshot. Landing it with
// its own tests first is what keeps every PR in the stack independently sound;
// this allowance goes away with #46, which is also where the types are
// re-exported through the session layer.
#![allow(dead_code)]

use crate::utils::ChainError;
use chrono::{DateTime, Datelike, Days, NaiveDate, NaiveTime, TimeZone, Utc, Weekday};
use chrono_tz::{GapInfo, Tz};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;

/// Maximum number of rules one schedule may declare.
///
/// A plain constant for now; issue #48 turns these bounds into validated
/// `OCS_MAX_*` environment knobs alongside the other request limits.
pub(crate) const MAX_SCHEDULE_RULES: usize = 16;

/// Maximum `target_count` a single rule may request.
pub(crate) const MAX_TARGET_COUNT: usize = 256;

/// Maximum number of expirations one snapshot may carry.
///
/// Validated against the **pre-deduplication** sum of every rule's
/// `target_count`, which is the tight upper bound on how many chains a
/// snapshot can hold. Checking the upper bound at construction keeps the
/// rejection deterministic: it does not depend on which dates happen to
/// coincide at a particular simulated instant.
pub(crate) const MAX_EXPIRATIONS_PER_SNAPSHOT: usize = 512;

/// Upper bound on how many candidate days a daily or weekly rule may scan
/// before the projection is considered pathological.
///
/// A weekly rule naming one weekday needs seven days per expiration; the
/// factor of eight plus a constant leaves room for weekends and for the
/// starting partial week without ever letting the scan run unbounded.
const DAY_SCAN_SLACK: usize = 32;

/// Upper bound on how many candidate periods a monthly or yearly rule may scan.
const PERIOD_SCAN_SLACK: usize = 2;

/// The versioned calendar policy a schedule is evaluated under.
///
/// The version is part of the stored simulation and part of the replay input
/// set, so a future policy can be added without silently changing the tape of
/// a simulation created under an older one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CalendarVersion {
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
    /// simulation stored under `weekdays_v1`.
    #[must_use]
    pub fn eligible_date(self, date: NaiveDate) -> Option<NaiveDate> {
        match self {
            CalendarVersion::WeekdaysV1 => match date.weekday() {
                Weekday::Sat | Weekday::Sun => None,
                _ => Some(date),
            },
        }
    }

    /// The stable wire name of this calendar version.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            CalendarVersion::WeekdaysV1 => "weekdays_v1",
        }
    }
}

/// What a rule expires on, independent of how many expirations it keeps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExpiryRuleKind {
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
    /// Builds a [`ExpiryRuleKind::Weekly`] with its weekday set normalised —
    /// deduplicated and ordered Monday-first — so that two schedules that name
    /// the same days in a different order are the same schedule.
    ///
    /// Normalising here rather than at validation time matters for replay: the
    /// normalised form is what is persisted and echoed back to the client.
    ///
    /// # Errors
    ///
    /// None directly; an empty or weekend-carrying set is rejected by
    /// [`ExpirationSchedule::validate`], which knows the owning rule id and can
    /// therefore name the offending field.
    #[must_use]
    pub fn weekly(weekdays: impl IntoIterator<Item = Weekday>) -> Self {
        let ordered: BTreeSet<u32> = weekdays
            .into_iter()
            .map(|day| day.num_days_from_monday())
            .collect();
        let weekdays = ordered
            .into_iter()
            .filter_map(Weekday::from_u64_mod7)
            .collect();
        ExpiryRuleKind::Weekly { weekdays }
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

/// Internal helper: `chrono::Weekday` has no infallible `from_u32`, so map the
/// Monday-based index back through the one form that cannot fail.
trait WeekdayFromIndex: Sized {
    fn from_u64_mod7(index: u32) -> Option<Self>;
}

impl WeekdayFromIndex for Weekday {
    fn from_u64_mod7(index: u32) -> Option<Self> {
        match index % 7 {
            0 => Some(Weekday::Mon),
            1 => Some(Weekday::Tue),
            2 => Some(Weekday::Wed),
            3 => Some(Weekday::Thu),
            4 => Some(Weekday::Fri),
            5 => Some(Weekday::Sat),
            6 => Some(Weekday::Sun),
            _ => None,
        }
    }
}

/// One expiration rule: what it expires on, and how many non-expired
/// expirations it keeps available at every step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpiryRule {
    /// Client-supplied stable identifier, unique within a schedule. It becomes
    /// the label on every chain this rule produces, so a client can tell which
    /// rule an expiration satisfies even when several rules share it.
    pub rule_id: String,
    /// What the rule expires on.
    pub kind: ExpiryRuleKind,
    /// How many non-expired expirations the rule keeps available. Counts are
    /// evaluated per rule, *before* coincident expirations are deduplicated.
    pub target_count: NonZeroUsize,
}

impl ExpiryRule {
    /// Builds a rule, rejecting a `target_count` of zero or above
    /// [`MAX_TARGET_COUNT`].
    ///
    /// A rule keeping zero expirations is not a rule, and the type reflects
    /// that: `target_count` is a [`NonZeroUsize`] internally, so the invalid
    /// state is unrepresentable past this constructor.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Validation`] naming `schedules.<rule_id>.target_count`
    /// when the count is zero or exceeds [`MAX_TARGET_COUNT`].
    pub fn new(
        rule_id: impl Into<String>,
        kind: ExpiryRuleKind,
        target_count: usize,
    ) -> Result<Self, ChainError> {
        let rule_id = rule_id.into();
        let field = format!("schedules.{rule_id}.target_count");

        let target_count =
            NonZeroUsize::new(target_count).ok_or_else(|| ChainError::Validation {
                field: field.clone(),
                reason: "must be at least 1".to_string(),
            })?;
        if target_count.get() > MAX_TARGET_COUNT {
            return Err(ChainError::Validation {
                field,
                reason: format!(
                    "must not exceed {MAX_TARGET_COUNT}, got {}",
                    target_count.get()
                ),
            });
        }

        Ok(Self {
            rule_id,
            kind,
            target_count,
        })
    }
}

/// A complete, versioned expiration schedule: the calendar policy, the zone and
/// local time every rule expires at, and the rules themselves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpirationSchedule {
    /// The calendar policy the rules are evaluated under.
    pub calendar: CalendarVersion,
    /// The IANA zone `expiration_time` is expressed in.
    pub timezone: Tz,
    /// The **local** time of day at which every rule's expirations expire.
    pub expiration_time: NaiveTime,
    /// The rules, ordered by `rule_id` after [`ExpirationSchedule::normalize`].
    pub rules: Vec<ExpiryRule>,
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
    pub fn new(
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
        schedule.normalize();
        Ok(schedule)
    }

    /// Orders the rules by `rule_id`. Called after validation, which has
    /// already established that the ids are unique.
    fn normalize(&mut self) {
        self.rules.sort_by(|a, b| a.rule_id.cmp(&b.rule_id));
    }

    /// Rejects every schedule the planner cannot evaluate unambiguously.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Validation`] naming the offending field when:
    ///
    /// - the rule list is empty, or longer than [`MAX_SCHEDULE_RULES`];
    /// - a `rule_id` is empty, or duplicated;
    /// - the pre-deduplication sum of `target_count` exceeds
    ///   [`MAX_EXPIRATIONS_PER_SNAPSHOT`], or overflows;
    /// - a `Weekly` rule names no weekday, or names Saturday or Sunday;
    /// - a `Monthly` or `Yearly` rule names Saturday or Sunday;
    /// - a `Yearly` rule names a month outside `1..=12`.
    pub fn validate(&self) -> Result<(), ChainError> {
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
            if rule.rule_id.trim().is_empty() {
                return Err(ChainError::Validation {
                    field: "schedules.rule_id".to_string(),
                    reason: "must not be empty".to_string(),
                });
            }
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

            validate_rule_kind(&rule.rule_id, &rule.kind)?;
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
pub struct ActiveExpiry {
    /// The absolute expiration instant, in UTC.
    pub expires_at: DateTime<Utc>,
    /// The ids of every rule this expiration satisfies, sorted. A date claimed
    /// by both a weekly and a monthly rule appears once, with both labels.
    pub labels: Vec<String>,
}

/// Evaluates an [`ExpirationSchedule`] against a simulated instant.
///
/// Pure: constructing a planner performs no I/O and evaluating it draws no
/// randomness and reads no clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollingPlanner {
    schedule: ExpirationSchedule,
}

impl RollingPlanner {
    /// Builds a planner from a schedule that has already been validated by
    /// [`ExpirationSchedule::new`].
    #[must_use]
    pub fn new(schedule: ExpirationSchedule) -> Self {
        Self { schedule }
    }

    /// The schedule this planner evaluates.
    #[must_use]
    pub fn schedule(&self) -> &ExpirationSchedule {
        &self.schedule
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
    pub fn active_at(&self, simulated_at: DateTime<Utc>) -> Result<Vec<ActiveExpiry>, ChainError> {
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
    fn project_rule(
        &self,
        rule: &ExpiryRule,
        simulated_at: DateTime<Utc>,
    ) -> Result<Vec<DateTime<Utc>>, ChainError> {
        let wanted = rule.target_count.get();
        let start = simulated_at
            .with_timezone(&self.schedule.timezone)
            .date_naive();
        let mut found = Vec::with_capacity(wanted);

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
                    let (year, month) = add_months(start.year(), start.month(), index)?;
                    last_weekday_of_month(rule, year, month, *weekday).map(Some)
                })?;
            }
            ExpiryRuleKind::Yearly { weekday, month } => {
                self.scan_periods(rule, simulated_at, wanted, &mut found, |index| {
                    let year = add_years(start.year(), index)?;
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
        found: &mut Vec<DateTime<Utc>>,
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
            if found.len() >= wanted {
                return Ok(());
            }

            if accepts(date)
                && let Some(eligible) = self.schedule.calendar.eligible_date(date)
            {
                let expires_at = self.instant_for(rule, eligible)?;
                if expires_at > simulated_at {
                    found.push(expires_at);
                }
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
        found: &mut Vec<DateTime<Utc>>,
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
                found.push(expires_at);
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
fn add_months(year: i32, month: u32, offset: u32) -> Result<(i32, u32), ChainError> {
    let zero_based = month
        .checked_sub(1)
        .ok_or_else(|| month_overflow("month underflows"))?;
    let total = zero_based
        .checked_add(offset)
        .ok_or_else(|| month_overflow("month arithmetic overflows"))?;

    let years =
        i32::try_from(total / 12).map_err(|_| month_overflow("year arithmetic overflows"))?;
    let year = year
        .checked_add(years)
        .ok_or_else(|| month_overflow("year arithmetic overflows"))?;
    let month = (total % 12)
        .checked_add(1)
        .ok_or_else(|| month_overflow("month arithmetic overflows"))?;

    Ok((year, month))
}

/// Advances `year` by `offset` years, with checked arithmetic.
fn add_years(year: i32, offset: u32) -> Result<i32, ChainError> {
    let offset = i32::try_from(offset).map_err(|_| month_overflow("year arithmetic overflows"))?;
    year.checked_add(offset)
        .ok_or_else(|| month_overflow("year arithmetic overflows"))
}

/// Builds the error reported when calendar arithmetic leaves the representable
/// range.
#[cold]
fn month_overflow(reason: &str) -> ChainError {
    ChainError::Validation {
        field: "schedules".to_string(),
        reason: reason.to_string(),
    }
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
    let (next_year, next_month) = add_months(year, month, 1)?;
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
    use chrono_tz::America::New_York;
    use chrono_tz::Australia::Lord_Howe;
    use chrono_tz::Europe::Madrid;

    /// 17:00, the local expiration time used by the reference configuration.
    fn at_1700() -> NaiveTime {
        match NaiveTime::from_hms_opt(17, 0, 0) {
            Some(time) => time,
            None => unreachable!("17:00:00 is a valid time"),
        }
    }

    /// Builds a UTC instant from its parts, for terse test setup.
    fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        match Utc
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
        {
            Some(instant) => instant,
            None => unreachable!("test instants are unambiguous in UTC"),
        }
    }

    /// Builds a rule, failing the test rather than panicking on a bad literal.
    fn rule(id: &str, kind: ExpiryRuleKind, count: usize) -> ExpiryRule {
        match ExpiryRule::new(id, kind, count) {
            Ok(rule) => rule,
            Err(error) => unreachable!("test rule must be valid: {error}"),
        }
    }

    /// Builds a New York schedule expiring at 17:00 local.
    fn ny_schedule(rules: Vec<ExpiryRule>) -> ExpirationSchedule {
        match ExpirationSchedule::new(CalendarVersion::WeekdaysV1, New_York, at_1700(), rules) {
            Ok(schedule) => schedule,
            Err(error) => unreachable!("test schedule must be valid: {error}"),
        }
    }

    /// The full reference configuration from ADR 0001 §14.
    fn reference_planner() -> RollingPlanner {
        RollingPlanner::new(ny_schedule(vec![
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
        ]))
    }

    fn active(planner: &RollingPlanner, at: DateTime<Utc>) -> Vec<ActiveExpiry> {
        match planner.active_at(at) {
            Ok(expiries) => expiries,
            Err(error) => unreachable!("planner must project: {error}"),
        }
    }

    // ---- cutoff boundary -------------------------------------------------

    /// One minute before the cutoff the same-day 0DTE is still present, and it
    /// is the nearest expiration.
    #[test]
    fn test_daily_before_cutoff_keeps_same_day_expiry() {
        let planner = RollingPlanner::new(ny_schedule(vec![rule(
            "zero_dte",
            ExpiryRuleKind::Daily,
            1,
        )]));

        // Monday 2026-01-05 16:59 New York = 21:59 UTC (EST).
        let expiries = active(&planner, utc(2026, 1, 5, 21, 59));

        assert_eq!(expiries.len(), 1);
        assert_eq!(expiries[0].expires_at, utc(2026, 1, 5, 22, 0));
        assert_eq!(expiries[0].labels, vec!["zero_dte".to_string()]);
    }

    /// At exactly the cutoff the expiry is gone and its replacement is present
    /// in the same result — `expires_at <= simulated_at` is expired.
    #[test]
    fn test_daily_at_cutoff_rolls_to_next_eligible_day() {
        let planner = RollingPlanner::new(ny_schedule(vec![rule(
            "zero_dte",
            ExpiryRuleKind::Daily,
            1,
        )]));

        let expiries = active(&planner, utc(2026, 1, 5, 22, 0));

        assert_eq!(expiries.len(), 1);
        // Tuesday 2026-01-06 17:00 New York.
        assert_eq!(expiries[0].expires_at, utc(2026, 1, 6, 22, 0));
    }

    /// One second after the cutoff behaves exactly like the cutoff itself.
    #[test]
    fn test_daily_after_cutoff_matches_cutoff_behaviour() {
        let planner = RollingPlanner::new(ny_schedule(vec![rule(
            "zero_dte",
            ExpiryRuleKind::Daily,
            1,
        )]));

        let at_cutoff = active(&planner, utc(2026, 1, 5, 22, 0));
        let after_cutoff =
            match planner.active_at(utc(2026, 1, 5, 22, 0) + chrono::Duration::seconds(1)) {
                Ok(expiries) => expiries,
                Err(error) => unreachable!("planner must project: {error}"),
            };

        assert_eq!(at_cutoff, after_cutoff);
    }

    // ---- weekends and rolling -------------------------------------------

    /// After Friday's cutoff the 0DTE rolls to Monday: `weekdays_v1` has no
    /// eligible weekend day.
    #[test]
    fn test_daily_rolls_over_the_weekend_to_monday() {
        let planner = RollingPlanner::new(ny_schedule(vec![rule(
            "zero_dte",
            ExpiryRuleKind::Daily,
            1,
        )]));

        // Friday 2026-01-09 17:00 New York, at the cutoff.
        let expiries = active(&planner, utc(2026, 1, 9, 22, 0));

        // Monday 2026-01-12, not Saturday the 10th.
        assert_eq!(expiries[0].expires_at, utc(2026, 1, 12, 22, 0));
    }

    /// A daily rule keeping five expirations spans a weekend without ever
    /// producing a Saturday or a Sunday.
    #[test]
    fn test_daily_never_emits_a_weekend_expiry() {
        let planner = RollingPlanner::new(ny_schedule(vec![rule(
            "zero_dte",
            ExpiryRuleKind::Daily,
            5,
        )]));

        let expiries = active(&planner, utc(2026, 1, 8, 12, 0));

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
        let planner = RollingPlanner::new(ny_schedule(vec![rule(
            "weeklies",
            ExpiryRuleKind::weekly([Weekday::Mon, Weekday::Wed, Weekday::Fri]),
            3,
        )]));

        // Monday 2026-01-05 09:30 New York.
        let expiries = active(&planner, utc(2026, 1, 5, 14, 30));

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
        let planner = RollingPlanner::new(ny_schedule(vec![rule(
            "weeklies",
            ExpiryRuleKind::weekly([Weekday::Mon, Weekday::Wed, Weekday::Fri]),
            3,
        )]));

        let expiries = active(&planner, utc(2026, 1, 5, 22, 0));

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

    /// The weekday set is normalised — deduplicated and Monday-first — so the
    /// order a client submits cannot change the schedule.
    #[test]
    fn test_weekly_weekday_set_is_normalised() {
        let kind = ExpiryRuleKind::weekly([Weekday::Fri, Weekday::Mon, Weekday::Fri]);

        match kind {
            ExpiryRuleKind::Weekly { weekdays } => {
                assert_eq!(weekdays, vec![Weekday::Mon, Weekday::Fri]);
            }
            other => unreachable!("expected a weekly rule, got {other:?}"),
        }
    }

    // ---- monthly ---------------------------------------------------------

    /// Twelve last-Friday monthlies stay twelve, and start where ADR 0001 §14.3
    /// says they do.
    #[test]
    fn test_monthly_last_friday_projects_twelve() {
        let planner = RollingPlanner::new(ny_schedule(vec![rule(
            "monthlies",
            ExpiryRuleKind::Monthly {
                weekday: Weekday::Fri,
            },
            12,
        )]));

        let expiries = active(&planner, utc(2026, 1, 5, 14, 30));

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
        let planner = RollingPlanner::new(ny_schedule(vec![rule(
            "monthlies",
            ExpiryRuleKind::Monthly {
                weekday: Weekday::Fri,
            },
            12,
        )]));

        let expiries = active(&planner, utc(2026, 1, 30, 22, 0));

        assert_eq!(expiries.len(), 12);
        assert_eq!(expiries[0].expires_at, utc(2026, 2, 27, 22, 0));
        assert_eq!(expiries[11].expires_at, utc(2027, 1, 29, 22, 0));
    }

    /// A twenty-nine-day February resolves its last Friday like any other
    /// month.
    #[test]
    fn test_monthly_handles_a_leap_year_february() {
        let planner = RollingPlanner::new(ny_schedule(vec![rule(
            "monthlies",
            ExpiryRuleKind::Monthly {
                weekday: Weekday::Fri,
            },
            1,
        )]));

        let expiries = active(&planner, utc(2028, 2, 1, 14, 30));

        assert_eq!(expiries.len(), 1);
        assert_eq!(expiries[0].expires_at, utc(2028, 2, 25, 22, 0));
    }

    /// The projection is stable across every month of a year: a monthly rule
    /// asked at any point keeps exactly its count.
    #[test]
    fn test_monthly_count_is_stable_across_a_whole_year() {
        let planner = RollingPlanner::new(ny_schedule(vec![rule(
            "monthlies",
            ExpiryRuleKind::Monthly {
                weekday: Weekday::Fri,
            },
            12,
        )]));

        for month in 1..=12 {
            let expiries = active(&planner, utc(2026, month, 15, 12, 0));
            assert_eq!(expiries.len(), 12, "month {month} lost inventory");
        }
    }

    // ---- yearly / LEAPS --------------------------------------------------

    /// A yearly rule reaches at least a year out, which is what makes
    /// LEAPS-style scenarios expressible.
    #[test]
    fn test_yearly_reaches_beyond_one_year() {
        let planner = RollingPlanner::new(ny_schedule(vec![rule(
            "leaps",
            ExpiryRuleKind::Yearly {
                weekday: Weekday::Fri,
                month: 12,
            },
            2,
        )]));

        let simulated_at = utc(2026, 1, 5, 14, 30);
        let expiries = active(&planner, simulated_at);

        assert_eq!(expiries.len(), 2);
        assert_eq!(expiries[0].expires_at, utc(2026, 12, 25, 22, 0));
        assert_eq!(expiries[1].expires_at, utc(2027, 12, 31, 22, 0));
        assert!(expiries[1].expires_at - simulated_at > chrono::Duration::days(365));
    }

    /// A yearly rule whose month has already passed this year starts from next
    /// year rather than returning nothing.
    #[test]
    fn test_yearly_skips_a_month_already_past() {
        let planner = RollingPlanner::new(ny_schedule(vec![rule(
            "leaps",
            ExpiryRuleKind::Yearly {
                weekday: Weekday::Fri,
                month: 1,
            },
            1,
        )]));

        let expiries = active(&planner, utc(2026, 6, 1, 12, 0));

        assert_eq!(expiries[0].expires_at, utc(2027, 1, 29, 22, 0));
    }

    // ---- overlap and deduplication --------------------------------------

    /// A date claimed by two rules appears once, carrying both labels — and
    /// each rule still counted it towards its own target.
    #[test]
    fn test_coincident_expiry_is_deduplicated_and_carries_both_labels() {
        let planner = reference_planner();

        let expiries = active(&planner, utc(2026, 1, 5, 14, 30));

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
        let planner = reference_planner();

        let expiries = active(&planner, utc(2026, 1, 5, 14, 30));

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
        let planner = reference_planner();

        let expiries = active(&planner, utc(2026, 1, 5, 14, 30));

        for pair in expiries.windows(2) {
            assert!(
                pair[0].expires_at < pair[1].expires_at,
                "results must be strictly increasing"
            );
        }
    }

    // ---- DST -------------------------------------------------------------

    /// A local expiration time inside a spring-forward gap resolves to the
    /// first instant after the gap, not to a time that never happened.
    ///
    /// Madrid springs forward on 2026-03-29 at 02:00 local, so 02:30 does not
    /// exist that day.
    #[test]
    fn test_dst_gap_resolves_to_the_first_instant_after_the_gap() {
        let expiration_time = match NaiveTime::from_hms_opt(2, 30, 0) {
            Some(time) => time,
            None => unreachable!("02:30:00 is a valid time"),
        };
        let schedule = match ExpirationSchedule::new(
            CalendarVersion::WeekdaysV1,
            Madrid,
            expiration_time,
            vec![rule(
                "weekly_sunday_gap",
                ExpiryRuleKind::weekly([Weekday::Mon]),
                1,
            )],
        ) {
            Ok(schedule) => schedule,
            Err(error) => unreachable!("schedule must be valid: {error}"),
        };
        let planner = RollingPlanner::new(schedule);

        // The Monday after the transition is unaffected; assert the helper
        // directly on the transition day, which is the interesting case.
        let gap_day = match NaiveDate::from_ymd_opt(2026, 3, 29) {
            Some(date) => date,
            None => unreachable!("2026-03-29 is a valid date"),
        };
        let probe = rule("probe", ExpiryRuleKind::Daily, 1);
        let resolved = match planner.instant_for(&probe, gap_day) {
            Ok(instant) => instant,
            Err(error) => unreachable!("gap must resolve: {error}"),
        };

        // 02:00 local CET is 01:00 UTC; the clock jumps straight to 03:00 CEST,
        // so the first instant after the gap is 01:00 UTC.
        assert_eq!(resolved, utc(2026, 3, 29, 1, 0));
    }

    /// A local expiration time inside an autumn fold occurs twice; the earlier
    /// instant is chosen, deterministically.
    #[test]
    fn test_dst_fold_resolves_to_the_earlier_instant() {
        let expiration_time = match NaiveTime::from_hms_opt(2, 30, 0) {
            Some(time) => time,
            None => unreachable!("02:30:00 is a valid time"),
        };
        let schedule = match ExpirationSchedule::new(
            CalendarVersion::WeekdaysV1,
            Madrid,
            expiration_time,
            vec![rule("probe", ExpiryRuleKind::Daily, 1)],
        ) {
            Ok(schedule) => schedule,
            Err(error) => unreachable!("schedule must be valid: {error}"),
        };
        let planner = RollingPlanner::new(schedule);

        // Madrid falls back on 2026-10-25 at 03:00 local, so 02:30 occurs
        // twice: once at 00:30 UTC (CEST) and once at 01:30 UTC (CET).
        let fold_day = match NaiveDate::from_ymd_opt(2026, 10, 25) {
            Some(date) => date,
            None => unreachable!("2026-10-25 is a valid date"),
        };
        let probe = rule("probe", ExpiryRuleKind::Daily, 1);
        let resolved = match planner.instant_for(&probe, fold_day) {
            Ok(instant) => instant,
            Err(error) => unreachable!("fold must resolve: {error}"),
        };

        assert_eq!(resolved, utc(2026, 10, 25, 0, 30));
    }

    /// The gap rule is shift-agnostic: Lord Howe Island jumps thirty minutes,
    /// and reading the end of the gap handles it without any special case.
    #[test]
    fn test_dst_gap_handles_a_thirty_minute_transition() {
        let expiration_time = match NaiveTime::from_hms_opt(2, 15, 0) {
            Some(time) => time,
            None => unreachable!("02:15:00 is a valid time"),
        };
        let schedule = match ExpirationSchedule::new(
            CalendarVersion::WeekdaysV1,
            Lord_Howe,
            expiration_time,
            vec![rule("probe", ExpiryRuleKind::Daily, 1)],
        ) {
            Ok(schedule) => schedule,
            Err(error) => unreachable!("schedule must be valid: {error}"),
        };
        let planner = RollingPlanner::new(schedule);

        // Lord Howe springs forward on 2026-10-04 at 02:00 local by thirty
        // minutes, so 02:15 does not exist. The gap ends at 02:30 local,
        // which is 15:30 UTC on the previous day.
        let gap_day = match NaiveDate::from_ymd_opt(2026, 10, 4) {
            Some(date) => date,
            None => unreachable!("2026-10-04 is a valid date"),
        };
        let probe = rule("probe", ExpiryRuleKind::Daily, 1);
        let resolved = match planner.instant_for(&probe, gap_day) {
            Ok(instant) => instant,
            Err(error) => unreachable!("gap must resolve: {error}"),
        };

        assert_eq!(resolved, utc(2026, 10, 3, 15, 30));
    }

    /// Crossing a DST boundary moves the absolute instant but never the local
    /// expiration time.
    #[test]
    fn test_local_expiration_time_is_stable_across_a_dst_boundary() {
        let planner = RollingPlanner::new(ny_schedule(vec![rule(
            "monthlies",
            ExpiryRuleKind::Monthly {
                weekday: Weekday::Fri,
            },
            4,
        )]));

        let expiries = active(&planner, utc(2026, 1, 5, 14, 30));

        for expiry in &expiries {
            let local = expiry.expires_at.with_timezone(&New_York);
            assert_eq!(local.time(), at_1700(), "local expiration time drifted");
        }
    }

    // ---- determinism -----------------------------------------------------

    /// The same inputs produce the same output, call after call.
    #[test]
    fn test_projection_is_stable_across_repeated_calls() {
        let planner = reference_planner();
        let at = utc(2026, 1, 5, 14, 30);

        assert_eq!(active(&planner, at), active(&planner, at));
    }

    /// Reordering the rules cannot change the result: the schedule is
    /// normalised and the output is keyed by instant.
    #[test]
    fn test_rule_order_does_not_change_the_result() {
        let forwards = reference_planner();
        let backwards = RollingPlanner::new(ny_schedule(vec![
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
        ]));

        let at = utc(2026, 1, 5, 14, 30);
        assert_eq!(active(&forwards, at), active(&backwards, at));
    }

    /// The result depends only on the simulated instant, not on the zone the
    /// caller happens to express it in.
    #[test]
    fn test_result_depends_only_on_the_absolute_instant() {
        let planner = reference_planner();
        let as_utc = utc(2026, 1, 5, 14, 30);
        let same_instant_elsewhere = as_utc.with_timezone(&Madrid).with_timezone(&Utc);

        assert_eq!(
            active(&planner, as_utc),
            active(&planner, same_instant_elsewhere)
        );
    }

    // ---- validation ------------------------------------------------------

    /// A zero `target_count` is rejected at construction, naming the field.
    #[test]
    fn test_zero_target_count_is_rejected() {
        let error = ExpiryRule::new("zero_dte", ExpiryRuleKind::Daily, 0);

        match error {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "schedules.zero_dte.target_count");
                assert!(reason.contains("at least 1"));
            }
            other => unreachable!("expected a validation error, got {other:?}"),
        }
    }

    /// A `target_count` above the cap is rejected.
    #[test]
    fn test_excessive_target_count_is_rejected() {
        let error = ExpiryRule::new("zero_dte", ExpiryRuleKind::Daily, MAX_TARGET_COUNT + 1);

        match error {
            Err(ChainError::Validation { field, .. }) => {
                assert_eq!(field, "schedules.zero_dte.target_count");
            }
            other => unreachable!("expected a validation error, got {other:?}"),
        }
    }

    /// Duplicate rule ids are rejected: labels would otherwise be ambiguous.
    #[test]
    fn test_duplicate_rule_id_is_rejected() {
        let error = ExpirationSchedule::new(
            CalendarVersion::WeekdaysV1,
            New_York,
            at_1700(),
            vec![
                rule("same", ExpiryRuleKind::Daily, 1),
                rule("same", ExpiryRuleKind::weekly([Weekday::Mon]), 1),
            ],
        );

        match error {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "schedules.same.rule_id");
                assert!(reason.contains("unique"));
            }
            other => unreachable!("expected a validation error, got {other:?}"),
        }
    }

    /// An empty rule id is rejected.
    #[test]
    fn test_empty_rule_id_is_rejected() {
        let error = ExpirationSchedule::new(
            CalendarVersion::WeekdaysV1,
            New_York,
            at_1700(),
            vec![rule("   ", ExpiryRuleKind::Daily, 1)],
        );

        match error {
            Err(ChainError::Validation { field, .. }) => {
                assert_eq!(field, "schedules.rule_id");
            }
            other => unreachable!("expected a validation error, got {other:?}"),
        }
    }

    /// An empty schedule is rejected rather than producing empty snapshots.
    #[test]
    fn test_empty_schedule_is_rejected() {
        let error =
            ExpirationSchedule::new(CalendarVersion::WeekdaysV1, New_York, at_1700(), vec![]);

        match error {
            Err(ChainError::Validation { field, .. }) => assert_eq!(field, "schedules"),
            other => unreachable!("expected a validation error, got {other:?}"),
        }
    }

    /// More rules than the cap is rejected.
    #[test]
    fn test_too_many_rules_is_rejected() {
        let rules = (0..=MAX_SCHEDULE_RULES)
            .map(|index| rule(&format!("rule_{index}"), ExpiryRuleKind::Daily, 1))
            .collect();
        let error =
            ExpirationSchedule::new(CalendarVersion::WeekdaysV1, New_York, at_1700(), rules);

        match error {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "schedules");
                assert!(reason.contains("rules"));
            }
            other => unreachable!("expected a validation error, got {other:?}"),
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
        let error =
            ExpirationSchedule::new(CalendarVersion::WeekdaysV1, New_York, at_1700(), rules);

        match error {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "schedules");
                assert!(reason.contains("total expiration count"));
            }
            other => unreachable!("expected a validation error, got {other:?}"),
        }
    }

    /// A weekly rule naming no weekday is rejected.
    #[test]
    fn test_weekly_without_weekdays_is_rejected() {
        let error = ExpirationSchedule::new(
            CalendarVersion::WeekdaysV1,
            New_York,
            at_1700(),
            vec![rule("weeklies", ExpiryRuleKind::weekly([]), 1)],
        );

        match error {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "schedules.weeklies.weekly.weekdays");
                assert!(reason.contains("at least one weekday"));
            }
            other => unreachable!("expected a validation error, got {other:?}"),
        }
    }

    /// A weekly rule naming a weekend day is rejected rather than silently
    /// producing nothing.
    #[test]
    fn test_weekly_naming_a_weekend_day_is_rejected() {
        let error = ExpirationSchedule::new(
            CalendarVersion::WeekdaysV1,
            New_York,
            at_1700(),
            vec![rule(
                "weeklies",
                ExpiryRuleKind::weekly([Weekday::Mon, Weekday::Sat]),
                1,
            )],
        );

        match error {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "schedules.weeklies.weekly.weekdays");
                assert!(reason.contains("Sat"));
            }
            other => unreachable!("expected a validation error, got {other:?}"),
        }
    }

    /// A monthly rule naming a weekend day is rejected.
    #[test]
    fn test_monthly_naming_a_weekend_day_is_rejected() {
        let error = ExpirationSchedule::new(
            CalendarVersion::WeekdaysV1,
            New_York,
            at_1700(),
            vec![rule(
                "monthlies",
                ExpiryRuleKind::Monthly {
                    weekday: Weekday::Sun,
                },
                1,
            )],
        );

        match error {
            Err(ChainError::Validation { field, .. }) => {
                assert_eq!(field, "schedules.monthlies.monthly.weekday");
            }
            other => unreachable!("expected a validation error, got {other:?}"),
        }
    }

    /// A yearly rule naming a month outside `1..=12` is rejected.
    #[test]
    fn test_yearly_with_an_invalid_month_is_rejected() {
        let error = ExpirationSchedule::new(
            CalendarVersion::WeekdaysV1,
            New_York,
            at_1700(),
            vec![rule(
                "leaps",
                ExpiryRuleKind::Yearly {
                    weekday: Weekday::Fri,
                    month: 13,
                },
                1,
            )],
        );

        match error {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "schedules.leaps.yearly.month");
                assert!(reason.contains("between 1 and 12"));
            }
            other => unreachable!("expected a validation error, got {other:?}"),
        }
    }

    // ---- arithmetic ------------------------------------------------------

    /// Month arithmetic that would leave the representable year range is a
    /// typed error, never a wrap.
    #[test]
    fn test_month_arithmetic_overflow_is_a_typed_error() {
        match add_months(i32::MAX, 12, 12) {
            Err(ChainError::Validation { field, reason }) => {
                assert_eq!(field, "schedules");
                assert!(reason.contains("overflow"));
            }
            other => unreachable!("expected an overflow error, got {other:?}"),
        }
    }

    /// Year arithmetic that would overflow is a typed error.
    #[test]
    fn test_year_arithmetic_overflow_is_a_typed_error() {
        match add_years(i32::MAX, 1) {
            Err(ChainError::Validation { reason, .. }) => assert!(reason.contains("overflow")),
            other => unreachable!("expected an overflow error, got {other:?}"),
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
            Err(error) => unreachable!("must not fail: {error}"),
        }
    }

    /// A projection that cannot reach beyond the representable date range
    /// reports the rule that failed instead of returning a short inventory.
    #[test]
    fn test_unreachable_projection_reports_the_rule() {
        let planner = RollingPlanner::new(ny_schedule(vec![rule(
            "leaps",
            ExpiryRuleKind::Yearly {
                weekday: Weekday::Fri,
                month: 12,
            },
            200,
        )]));

        // NaiveDate tops out well before 200 years past the maximum year, so
        // the projection cannot complete.
        let far_future = match Utc.with_ymd_and_hms(262_000, 1, 1, 0, 0, 0).single() {
            Some(instant) => instant,
            None => unreachable!("262000-01-01 is representable"),
        };

        match planner.active_at(far_future) {
            Err(ChainError::Validation { field, .. }) => {
                assert!(field.starts_with("schedules."), "unexpected field {field}");
            }
            other => unreachable!("expected a projection error, got {other:?}"),
        }
    }

    // ---- calendar policy -------------------------------------------------

    /// The calendar's eligibility hook rejects weekends and accepts weekdays.
    #[test]
    fn test_weekdays_v1_eligibility() {
        let saturday = match NaiveDate::from_ymd_opt(2026, 1, 10) {
            Some(date) => date,
            None => unreachable!("2026-01-10 is a valid date"),
        };
        let monday = match NaiveDate::from_ymd_opt(2026, 1, 12) {
            Some(date) => date,
            None => unreachable!("2026-01-12 is a valid date"),
        };

        assert_eq!(CalendarVersion::WeekdaysV1.eligible_date(saturday), None);
        assert_eq!(
            CalendarVersion::WeekdaysV1.eligible_date(monday),
            Some(monday)
        );
    }

    /// The calendar version round-trips through serde under its wire name, so
    /// a stored simulation keeps the policy it was created with.
    #[test]
    fn test_calendar_version_serde_round_trip() {
        let json = match serde_json::to_string(&CalendarVersion::WeekdaysV1) {
            Ok(json) => json,
            Err(error) => unreachable!("must serialize: {error}"),
        };
        assert_eq!(json, "\"weekdays_v1\"");
        assert_eq!(CalendarVersion::WeekdaysV1.as_str(), "weekdays_v1");

        match serde_json::from_str::<CalendarVersion>(&json) {
            Ok(parsed) => assert_eq!(parsed, CalendarVersion::WeekdaysV1),
            Err(error) => unreachable!("must deserialize: {error}"),
        }
    }

    /// The whole schedule round-trips through serde, which is what lets #44
    /// persist it as part of the stored v2 parameters.
    #[test]
    fn test_schedule_serde_round_trip() {
        let schedule = ny_schedule(vec![
            rule("zero_dte", ExpiryRuleKind::Daily, 1),
            rule(
                "weeklies",
                ExpiryRuleKind::weekly([Weekday::Mon, Weekday::Wed, Weekday::Fri]),
                3,
            ),
        ]);

        let json = match serde_json::to_string(&schedule) {
            Ok(json) => json,
            Err(error) => unreachable!("must serialize: {error}"),
        };
        match serde_json::from_str::<ExpirationSchedule>(&json) {
            Ok(parsed) => assert_eq!(parsed, schedule),
            Err(error) => unreachable!("must deserialize: {error}"),
        }
    }

    /// Validation orders the rules by id, so the stored schedule does not
    /// depend on the order the client submitted.
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

        let ids: Vec<&str> = schedule
            .rules
            .iter()
            .map(|rule| rule.rule_id.as_str())
            .collect();
        assert_eq!(ids, vec!["monthlies", "zero_dte"]);
    }
}
