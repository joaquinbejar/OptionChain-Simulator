# ADR 0001 — The v2 rolling-simulation contract

| | |
|---|---|
| **Status** | Accepted |
| **Date** | 2026-08-14 |
| **Milestone** | crate `v0.2.0` |
| **Issue** | [#42](https://github.com/joaquinbejar/OptionChain-Simulator/issues/42) |
| **Implemented by** | #43 (planner), #44 (session/persistence), #45 (factor tape), #46 (snapshots), #47 (REST), #48 (retention/limits), #49 (export) |

This ADR is the contract. It is written before any v2 code exists so that the
seven implementation issues have one authority to agree with, and so that a
reviewer can check an implementation against a specification rather than
against an intention.

---

## 1. Context

The service today accepts a single **relative** `days_to_expiration`
(`src/api/rest/requests.rs`, `src/session/model.rs`), builds exactly one
`OptionChain` per step (`src/domain/simulator.rs`), and stamps each response
with `Utc::now()` (`src/api/rest/handlers.rs`). The whole chain tape is
pre-materialised as a `RandomWalk<Positive, OptionChain>`, and the walk stops
when its one expiry reaches zero.

Three things follow from that design, and all three block the v0.2.0 goal of
serving replayable multi-month and multi-year backtests:

1. **A relative expiration cannot express a schedule.** "45 days from now"
   cannot say "the next three Monday/Wednesday/Friday weeklies expiring at
   17:00 New York time".
2. **A single expiry cannot roll.** When the one contract expires, the
   simulation ends instead of replenishing its inventory.
3. **A wall-clock response timestamp is not replayable.** Two runs of the same
   seeded session produce different `timestamp` values, and expirations
   expressed relative to "now" drift with the machine clock.

The v1 surface nevertheless has consumers — IronCondor reads those DTOs, and
the crate is published — so none of this may be fixed by changing v1. The
decision below adds a parallel v2 surface and freezes v1.

---

## 2. Decision

Introduce `/api/v2/simulations`: a deterministic, simulated-clock resource that
carries a **rolling inventory of absolute expirations**, serves one snapshot per
cursor position, and can be exported in bulk. `/api/v1/chain` is frozen exactly
as it is.

The v2 surface rests on five decisions, each detailed below:

| # | Decision |
|---|---|
| D1 | Time is **simulated**, never read from the wall clock after session creation (§3). |
| D2 | Expirations come from **versioned schedule rules** evaluated by a pure planner (§4, §5). |
| D3 | A snapshot is built **lazily** from a lightweight factor row plus the planner's output (§6, §7). |
| D4 | Replay is guaranteed by a **named, finite set of effective inputs** (§8). |
| D5 | Export is **read-only** and rebuilds from those inputs; it is not a persistence layer (§10). |

### 2.1 Module homes

v2 follows the existing `api → session → domain → infrastructure` flow. Nothing
below inverts it, and `domain` stays a **private** module — so the export path
in §10 goes through `SessionManager`, never from a handler straight into the
domain.

| concern | home | visibility |
|---|---|---|
| expiry **configuration** types (§4.1) | `src/domain/expiry.rs` | `pub`, re-exported through `session` |
| expiry **planner** — projection, dedupe, DST (§4, §5) | `src/domain/expiry.rs` | `pub(crate)`, never leaves the crate |
| factor tape (§8) | `src/domain/factors.rs` | `pub(crate)` |
| snapshot aggregate (§7) | `src/domain/series.rs` | `pub(crate)` |
| v2 parameters, session, stores (§3, §12.2) | `src/session/*` | public |
| v2 DTOs, routes, handlers, OpenAPI (§6, §7, §10, §11) | `src/api/rest/*` | public |
| retention knobs, metrics (§9) | `src/infrastructure/*`, `src/api/rest/limits.rs` pattern | internal |
| every error (§11) | `src/utils/error.rs` — `ChainError` | public |

The split matters. The **configuration** types — `ExpirationSchedule`,
`ExpiryRule`, `ExpiryRuleKind`, `CalendarVersion` — are re-exported through
`session`, because they are fields of the public `SimulationParametersV2` and a
consumer building parameters in Rust needs to name them. The **planner** —
`RollingPlanner`, `ActiveExpiry` — stays crate-internal: it is how snapshots get
built, not part of anyone's contract.

Publishing the configuration types puts `chrono_tz::Tz`, `chrono::Weekday` and
`chrono::NaiveTime` into this crate's public API, so a `chrono-tz` major bump
becomes a semver event here. That is accepted rather than overlooked: the crate
already publishes `optionstratlib::WalkType`, `TimeFrame`, `Positive` and
`Decimal` in `SimulationParameters`, IronCondor consumes the **REST** contract
rather than the Rust types, and the alternative — a parallel primitive-typed
representation in `session` — would mean two shapes, two validation paths, and a
standing risk that they drift.

What is *not* accepted is a validation bypass. **Every** stored v2 type — the
schedule and its rules, the simulation parameters, the simulation document —
routes `Deserialize` through a validating constructor, so a document loaded
from Redis is checked exactly like a request. This is not defensive
programming for its own sake: without it a `step_interval_seconds` of `0`
freezes the simulated clock and every snapshot silently carries the same
instant, a `steps` past the cap drives an unbounded factor tape, and a
sub-second `effective_start` breaks the whole-second rendering §10.2 relies on
for byte-comparable exports. Redis is an outer layer, and a domain type does
not trust one.

The stored shapes additionally reject unknown fields. That turns the
rolling-deploy hazard into a loud one: an old replica reading a document
written by a newer binary fails, rather than dropping the fields it does not
understand and writing the truncated document back with an intact revision so
the compare-and-swap succeeds. For the same reason a `schema_version` from the
future is refused on load.

---

## 3. Deterministic time semantics

### 3.1 The effective start

`POST /api/v2/simulations` accepts an optional `start_at` (RFC 3339). The
service resolves it **exactly once**, at request → parameters conversion:

- when supplied, it is normalised to UTC and truncated to whole seconds;
- when omitted, one value is generated from the wall clock at that moment,
  normalised the same way.

The resolved value is persisted as `effective_start` and returned in every
session and snapshot response. After that single resolution the wall clock is
never consulted again for simulation output. Truncation to whole seconds is
what lets every timestamp in the API and in the exports render as
`YYYY-MM-DDTHH:MM:SSZ` with no fractional part, which in turn is what makes
repeated exports byte-comparable.

### 3.2 The simulated clock

```
simulated_at(cursor) = effective_start + cursor × step_interval
```

- `cursor` is the session's `current_step`, starting at `0`.
- `step_interval` is fixed for the lifetime of the simulation. The request field
  is `step_interval_seconds` (`u64`), valid in `1..=31_536_000` (one second to
  one year).
- The multiplication and the addition are **checked**. An overflow is a typed
  error, never a wrapped or saturated timestamp.

When `step_interval_seconds` is omitted it is derived from `time_frame`:

| `time_frame` | derived seconds |
|---|---|
| `Second` | 1 |
| `Minute` | 60 |
| `Hour` | 3 600 |
| `Day` | 86 400 |
| `Week` | 604 800 |
| `Month` | 2 592 000 (30 d) |
| `Quarter` | 7 776 000 (90 d) |
| `Year` | 31 536 000 (365 d) |
| `Microsecond`, `Millisecond` | — derivation fails |
| `Custom(periods_per_year)` | `round(31_536_000 / periods_per_year)` |

`Microsecond` and `Millisecond` derive to less than one second, and a small
`Custom` derives to more than a year. Both — and any explicit value outside
`1..=31_536_000` — are rejected with a `400` naming `step_interval_seconds`,
rather than silently clamped. The resolved value is persisted and surfaced
alongside `effective_start`.

`time_frame` keeps its v1 meaning — it scales the stochastic model — while
`step_interval_seconds` drives the simulated clock. They are allowed to differ,
because a caller may legitimately want daily dynamics sampled on a coarser or
finer clock; both effective values are surfaced so the pair is always visible,
and both are replay inputs (§8).

### 3.3 The expiration cutoff

An expiration is **expired** at simulated time `t` when:

```
expires_at <= t
```

The comparison is on the absolute UTC instant, `<=` not `<`, so the boundary is
unambiguous:

| simulated_at (local, America/New_York) | a 17:00 same-day expiry |
|---|---|
| 16:59:59 | **present**, `days_to_expiration > 0` |
| 17:00:00 | **absent** — expired at exactly the cutoff |
| 17:00:01 | absent |

Whenever a rule loses an expiration to this cutoff, its replacement appears in
**the same snapshot**. There is never a step at which a rule holds fewer than
its `target_count` expirations.

---

## 4. Expiration schedules and the `weekdays_v1` calendar

### 4.1 Schedule shape

A simulation carries a `schedules` array. Each entry is one **flat object
tagged by `kind`**, with the kind-specific fields as siblings of `rule_id`.

The tag and those fields are declared explicitly rather than through an
internally-tagged enum: serde cannot combine that with `deny_unknown_fields`,
and giving up `deny_unknown_fields` would give up both guarantees §4.4 makes —
that an unknown field is rejected, and that a field which does not belong to
the rule's `kind` is rejected rather than silently ignored.

| field | meaning |
|---|---|
| `rule_id` | client-supplied stable identifier, unique within the simulation; becomes the label on every chain the rule produces. Constrained to `[A-Za-z0-9_-]`, 1–64 characters — it is echoed on every chain of every step and joined into a single CSV column with `\|` (§10.2), so a separator or a quote inside an id would corrupt that column |
| `kind` | `daily` \| `weekly` \| `monthly` \| `yearly` |
| `target_count` | how many non-expired expirations the rule keeps available at every step (`>= 1`) |
| `weekdays` | `weekly` only — non-empty set of weekdays |
| `weekday` | `monthly` / `yearly` only — the weekday whose **last** occurrence in the period expires |
| `month` | `yearly` only — the month whose last `weekday` expires (default `12`) |

The schedule as a whole carries the timezone, the expiration time of day, and
the calendar version:

| field | meaning |
|---|---|
| `timezone` | IANA zone name (e.g. `America/New_York`), applied to every rule |
| `expiration_time` | local time of day, `HH:MM` or `HH:MM:SS` |
| `calendar` | calendar policy version — `weekdays_v1` is the only accepted value today, and it is persisted so a future version cannot silently change a stored simulation's tape |
| `tzdb_version` | **resolved, not accepted** — the IANA time zone database the binary was built against (`chrono_tz::IANA_TZDB_VERSION`, e.g. `2025b`), persisted and echoed |

`tzdb_version` is the one schedule field a client cannot supply: the timezone
rules that turn `expiration_time` into an instant come from the database
compiled into the binary, so the service resolves it, persists it and echoes it
rather than accepting it. §8 explains why it belongs on the replay-input list.

### 4.2 `weekdays_v1`

`weekdays_v1` is deliberately the smallest calendar that is honest about what
it knows:

- **Eligible days are Monday through Friday.** Saturday and Sunday never carry
  an expiration.
- **`daily` / 0DTE** — every eligible weekday expires at `expiration_time`.
  A Friday-evening simulated clock therefore rolls to Monday, not Saturday.
- **`weekly`** — every named weekday expires. Naming a weekend day is a
  validation error rather than a silently dropped rule.
- **`monthly`** — the **last** occurrence of `weekday` in each calendar month,
  evaluated in the schedule's timezone. "Last Friday of the month" is exactly
  what it says; it is not the third Friday, and it is not adjusted.
- **`yearly` / LEAPS** — the last occurrence of `weekday` in `month` of each
  year. A `target_count` of 2 or more reaches beyond one year, which is what
  makes LEAPS-style scenarios expressible.
- **No exchange-holiday database is bundled.** `weekdays_v1` knows weekends and
  nothing else. The planner exposes a holiday-adjustment hook so a future
  `weekdays_v2` can roll a holiday expiry to the previous eligible weekday
  without reinterpreting any simulation stored under `weekdays_v1`.

### 4.3 Local time to absolute instant

`expiration_time` is a **local** time in the schedule's zone; `expires_at` is
always an absolute UTC instant. The conversion resolves the two irregular cases
of a DST transition explicitly, via `chrono::MappedLocalTime`:

| case | `MappedLocalTime` | rule |
|---|---|---|
| normal | `Single(t)` | use `t` |
| **fold** (clock repeats, local time occurs twice) | `Ambiguous(earlier, later)` | use **`earlier`** — the first occurrence |
| **gap** (clock jumps, local time never occurs) | `None` | use the **first instant after the gap**, read from `chrono_tz::GapInfo::new(local, tz).end` |

Resolving the gap from `GapInfo` rather than by adding a fixed offset is
deliberately shift-agnostic: it is correct for the common one-hour jump and
equally correct for the 30-minute transitions used by zones such as
`Australia/Lord_Howe`, which a fixed `+1 h` adjustment would get wrong. When
`GapInfo` cannot report an end — at the limits of the known timestamp table —
the conversion fails with a typed error rather than falling back silently.

Both choices are arbitrary in the sense that either would be defensible; what
matters is that they are fixed, documented, and tested, because the whole
replay guarantee rests on the same local time always mapping to the same
instant.

The practical consequence for the reference schedule: a 17:00 New York
expiration is `22:00Z` under EST and `21:00Z` under EDT. The absolute instant
moves across a DST boundary; the local expiration time does not.

### 4.4 Invalid schedules

Every one of these is rejected at the DTO conversion boundary with a
field-specific validation error, never a panic and never a silently degraded
simulation:

- an unknown field anywhere in the request (`deny_unknown_fields`), or a field
  not valid for a rule's `kind` — `weekdays` on a `daily` rule, `month` on a
  `weekly` rule;
- an unknown or unparseable IANA timezone;
- an unparseable or out-of-range `expiration_time`;
- a `calendar` other than `weekdays_v1`;
- a `rule_id` that is empty, longer than 64 characters, carries a character
  outside `[A-Za-z0-9_-]`, or is duplicated;
- `target_count == 0`, or above the configured cap (§9.3);
- a `weekly` rule with an empty `weekdays` set, or naming Saturday/Sunday;
- a `monthly` / `yearly` rule naming a weekend `weekday`, or a `month` outside
  `1..=12`;
- an empty `schedules` array, or more rules than the configured cap (§9.3);
- a projected inventory larger than the configured per-snapshot cap (§9.3);
- any date arithmetic that would overflow while projecting the requested count;
- a top-level `volatility` that disagrees with the walk model's own
  `volatility`. A simulation has exactly one base volatility: v1 accepts the
  pair and silently prices step zero at one while walking on the other, and v2
  refuses the contradiction at the boundary rather than letting the domain pick
  a winner. `Historical` carries no model volatility, so there is nothing to
  disagree with. The check runs on the stored document too, so a hand-edited
  simulation cannot smuggle the contradiction back in.

---

## 5. Overlap semantics

Rules overlap constantly — the last Friday of a month is also a Friday weekly,
and on a Friday it is also the 0DTE. The rule is:

> **Counts are evaluated per rule. Coincident physical expirations are priced
> once and carry every matching label.**

The planner therefore works in two phases:

1. **Per-rule projection.** Each rule independently produces exactly
   `target_count` non-expired expirations. A rule is never starved because
   another rule already claimed the same date.
2. **Physical deduplication.** The union is then deduplicated by `expires_at`.
   One surviving entry carries the sorted union of the contributing
   `rule_id`s as its `labels`, and the result is ordered chronologically.

So the number of chains in a snapshot is at most, and usually fewer than, the
sum of the `target_count`s. A client that needs to know a rule is satisfied
reads the labels, not the chain count.

The pricing consequence matters as much as the bookkeeping one: a coincident
expiration is built **once**. Two labels never mean two chains with two
independently-built strike ladders that could diverge.

---

## 6. Resources and lifecycle

| method | path | semantics |
|---|---|---|
| `POST` | `/api/v2/simulations` | create; resolves and returns every replay input (§8) |
| `GET` | `/api/v2/simulations/{id}` | session metadata: cursor, state, version, effective parameters |
| `GET` | `/api/v2/simulations/{id}/snapshot` | **safe, repeatable peek** at the current cursor — never advances, never persists |
| `POST` | `/api/v2/simulations/{id}/step?expected_step=N` | **serve-then-advance**: returns the snapshot at the current cursor, then advances exactly once |
| `DELETE` | `/api/v2/simulations/{id}` | delete the session and evict its cached state |
| `GET` | `/api/v2/simulations/{id}/export?…` | read-only bulk tape (§10) |

The peek/advance split mirrors v1's `GET /api/v1/chain` and
`POST /api/v1/chain/step`, which is deliberate — it is the one v1 semantic
worth carrying forward unchanged. v2 takes the id from the **path** rather than
from v1's `?sessionid=` query parameter; that is the only intentional
divergence, and it does not touch v1.

### 6.1 Two distinct concurrency mechanisms

v1 already ships both, and v2 reuses them **with v1's names and v1's status
codes** rather than inventing a third vocabulary:

- **`expected_step`** — an optional client *precondition* on the cursor, exactly
  as in `POST /api/v1/chain/step` (`src/api/rest/handlers.rs:316`, `:370`). If
  the stored `current_step` differs, the advance is refused with **`412`** and a
  body carrying `error` and `current_step`, and nothing is persisted. This makes
  a retry after a timeout safe: it cannot double-advance.
- **`version`** — the session's independent optimistic-concurrency counter
  (`src/session/model.rs:263-271`), used for the compare-and-swap save. Two
  concurrent advances that both pass the precondition still produce one winner;
  the loser's CAS fails and surfaces as **`409`**.

The two are not the same thing and are not collapsed: `412` means "the cursor is
not where you thought", `409` means "someone else committed first".

**A v2 simulation is immutable after creation.** There is no `PATCH` and no
`PUT`. Changing the seed, the start, the schedules, or the chain shape changes
the tape, so it creates a new simulation instead of mutating one. The existing
`SessionState` machine is reused, but `Modified` and `Reinitialized` are
unreachable for v2; the lifecycle is `Initialized → InProgress → Completed`, and
`version` therefore advances only with the cursor.

---

## 7. The snapshot

A snapshot carries the state of the whole simulated market at one cursor
position: one clock, one spot, one base volatility, and the ordered set of
chains that are alive at that instant.

The example below is cursor `1` of the reference configuration in §14
(`effective_start` `2026-01-05T14:30:00Z`, `step_interval_seconds` `86400`), so
`simulated_at` is `2026-01-06T14:30:00Z` — Tuesday 09:30 in New York.

```jsonc
{
  "id": "0a5b2c34-7f1e-4c2a-9b8d-1e2f3a4b5c6d",
  "state": "in_progress",
  "version": 1,
  "cursor": { "current_step": 1, "total_steps": 500 },
  "simulated_at": "2026-01-06T14:30:00Z",
  "underlying": {
    "symbol": "SPX",
    "price": 5012.34,
    "base_volatility": 0.1832
  },
  "chains": [
    {
      "expires_at": "2026-01-06T22:00:00Z",
      "days_to_expiration": 0.3125,
      "labels": ["zero_dte"],
      "contracts": [
        {
          "strike": 5000.0,
          "implied_volatility": 0.1914,
          "gamma": 0.0031,
          "call": { "bid": 21.4,  "ask": 22.1,  "mid": 21.75, "delta":  0.55 },
          "put":  { "bid":  9.05, "ask":  9.55, "mid":  9.30, "delta": -0.45 }
        }
      ]
    }
  ]
}
```

Invariants a reviewer can check:

- `chains` is ordered by `expires_at` ascending; `contracts` by `strike`
  ascending; `labels` sorted.
- Every chain shares the snapshot's `price` and `base_volatility`, and has its
  own absolute `expires_at` and **fractional** `days_to_expiration`, computed
  from the same `(simulated_at, expires_at)` pair the planner used. Here
  7.5 hours = `0.3125` days.
- `days_to_expiration` is strictly positive. An expired chain is never emitted.
- Per-strike `implied_volatility` differs from the snapshot's
  `base_volatility` by the configured skew and smile — the base is the input to
  the ladder, not a copy of it.
- All numeric fields are `f64` **at the REST boundary only**; the domain works
  in `Positive`/`Decimal`.
- `expires_at` is the **only** expiration a client sees. Upstream's
  `OptionChain` also carries a `YYYY-MM-DD` string that it stamps from the host
  clock — `get_date_string()` reads `Utc::now()` for a relative expiration and
  ignores the thread-local reference, and there is no hook to change that. It is
  upstream metadata, it is not part of this contract, and the v2 DTOs must not
  surface it. Nothing that reaches a price depends on it: premiums, Greeks and
  the derived strike interval all come from the fractional days value directly,
  so the snapshot's *surfaced* content stays a pure function of the effective
  inputs.
- The identifier field is `id` and the resource is a *simulation*; "session" is
  v1 vocabulary and is not used on the v2 wire.
- `state` is serialised **`snake_case`** — `initialized`, `in_progress`,
  `completed`. This intentionally differs from v1, which renders the `Display`
  form and emits `"In Progress"` **with a space**
  (`src/session/model.rs:81`, `src/api/rest/handlers.rs:283`). v1's spelling is
  frozen (§12.1); v2 does not inherit it.

---

## 8. Replay

> A v2 simulation's complete tape is reproduced exactly by re-creating it with
> the same **effective seed**, **effective start**, **step interval**,
> **time frame**, **timezone**, **calendar version**, **IANA tzdb version**,
> **normalised schedules**, and market and chain parameters (`symbol`, `steps`,
> `initial_price`, `volatility`, `risk_free_rate`, `dividend_yield`, `method`,
> `chain_size`, `strike_interval`, `skew_slope`, `smile_curve`, `spread`).

That list is exhaustive and is meant to be checkable. All of it is resolved once
at creation, persisted, and echoed in the creation and session responses — so a
client that records the creation response can reproduce the run without having
recorded the request.  This extends the existing v1 effective-seed contract
rather than replacing it.

**Why the tzdb version is on that list.** Every `expires_at` is a function of
the IANA rules compiled into the binary, and `chrono-tz` ships tzdb updates in
*patch* releases while the workspace pins `major.minor` and does not commit
`Cargo.lock`. Two builds of the same commit can therefore embed different data.
That is theoretical for `America/New_York` and entirely real for zones such as
`Africa/Cairo` or `Asia/Jerusalem`, which change DST rules on weeks of notice.
Versioning `weekdays_v1` while leaving the larger source of calendar truth
unversioned would be a false guarantee, so the effective tzdb release
(`chrono_tz::IANA_TZDB_VERSION`, e.g. `"2025b"`) is persisted and echoed like
the seed. A replay against a different tzdb is still a replay — it is just one
the client can now detect.

Three properties keep the guarantee honest, and each is a test in the issues
that implement it:

- **The factor tape is independent of the schedules.** Adding, removing or
  reordering expiration rules leaves spot and base volatility unchanged at
  every step, because the planner draws no randomness and lazy chain building
  consumes none.
- **In-process eviction is invisible.** Dropping a cached factor tape or
  snapshot, or restarting the process, rebuilds from the same effective inputs
  and yields the same values. This is the existing LRU behaviour of the walk
  cache (`src/domain/simulator.rs`), extended to the v2 caches.
- **Store expiry is *not* invisible, and is a different thing.** If the v2
  session itself is evicted by its idle TTL (§9), its id stops resolving and
  further calls return `404`. The tape is still reproducible — but only by
  creating a new simulation from the recorded effective inputs, which is
  precisely why they are all echoed at creation.
- **Nothing downstream of creation reads the wall clock.** `simulated_at`,
  `expires_at`, and `days_to_expiration` are all functions of the effective
  inputs and the cursor.

---

## 9. Retention, eviction, and limits

Real service resources are bounded; simulated horizons are not. A simulation
whose clock spans three years must not be killed by an idle timeout measured in
real minutes, and must not pin three years of option contracts in memory.

### 9.1 Session retention

The retention lifetime of a v2 session is operational and completely
independent of the months or years its simulated clock spans. It is
configurable, validated at startup, and applied consistently by the in-memory
and Redis stores, which must agree on expiry and renewal semantics — one shared
default constant rather than one per backend, because two independently-named
defaults are how that agreement drifts. v1's hard-coded one-hour Redis TTL
(`src/main.rs`) is unchanged; only v2 gets the knob.

The window is measured from the **last write**, not the last access. §6 defines
the snapshot endpoint as a safe peek that persists nothing, so a client that
only ever peeks does not refresh its retention. Both backends behave the same
way, which is the property that matters for correctness; whether a peek *should*
refresh is a product decision for #48.

### 9.2 Cache eviction

Two v2 caches are bounded, both with least-recently-used eviction following the
existing `enforce_capacity` pattern (`src/domain/simulator.rs`):

- the **factor tape** per simulation — `O(steps)`, cheap;
- the **snapshot cache** per `(simulation, step)` — expensive, since a snapshot
  holds every strike of every active expiry.

Cached state is evicted on delete, on completion, on store cleanup, and under
capacity or idle pressure. For that to be possible without leaking, v2 store
cleanup reports the **ids** it expired rather than only a count, so the matching
domain-cache entries go with them; the v1 `SessionStore::cleanup` signature
(which returns `usize`) is unchanged. No lock is held across an `await`.

An in-flight export holds an **immutable snapshot of the effective parameters**
taken when it started, so a concurrent TTL expiry cannot invalidate a download
that is already streaming.

### 9.3 Limits

Following the existing `OCS_MAX_*` pattern (`src/api/rest/limits.rs`, which
already bounds `OCS_MAX_STEPS` at 10 000 and `OCS_MAX_CHAIN_SIZE` at 500), v2
adds caps so that a single request cannot ask for unbounded work:

| knob | bounds |
|---|---|
| `OCS_MAX_SCHEDULES` | rules per simulation |
| `OCS_MAX_TARGET_COUNT` | `target_count` of one rule |
| `OCS_MAX_EXPIRATIONS_PER_SNAPSHOT` | expirations in one snapshot, enforced at schedule validation on the **pre-deduplication** sum of every rule's `target_count` |
| `OCS_MAX_EXPORT_ROWS` | rows one export may produce |
| v2 session idle TTL, factor-tape and snapshot cache capacities | §9.1, §9.2 |

The pre-deduplication sum is the tight upper bound on how many chains a snapshot
can hold, and checking it at construction keeps the rejection deterministic: a
post-deduplication check would accept or reject the same schedule depending on
which dates happened to coincide at the instant it was evaluated.

Breaching a request-shaped cap is a `400` naming the offending field; breaching
`OCS_MAX_EXPORT_ROWS` is a `400` naming the range. Every knob is documented in
`.env.example` with its default and range, validated at startup, and covered by
a missing/invalid-value test.

Without these, the existing defaults alone permit an export of
10 000 steps × N expirations × 1 001 strikes: streaming bounds the *memory*,
but nothing would bound the *work*.

---

## 10. Export

```
GET /api/v2/simulations/{id}/export
      ?dataset=underlying|volatility|option_chains
      &format=json|csv
      &from_step=<usize>&to_step=<usize>
```

Export is **read-only**. It replays from step `0` — or from the requested,
validated range — off an immutable snapshot of the effective parameters (§9.2).
It never advances the cursor, never changes the session state or version, and
never alters what the next `snapshot` call returns. A client can export a
simulation it has not walked at all, and get the complete tape.

Range semantics: `from_step` and `to_step` are **inclusive**, defaulting to `0`
and to the last generated step (`total_steps - 1`). `from_step > to_step`, any
bound beyond the tape, and a range exceeding `OCS_MAX_EXPORT_ROWS` are `400`s.

### 10.1 Datasets

Every dataset is ordered by `step` ascending, then `expires_at` ascending, then
`strike` ascending. JSON and CSV carry the same rows, in the same order, with
the same values.

**`underlying`** — one row per step.

| column | type |
|---|---|
| `step` | integer |
| `simulated_at` | RFC 3339 UTC |
| `symbol` | string |
| `price` | number |

**`volatility`** — one row per step.

| column | type |
|---|---|
| `step` | integer |
| `simulated_at` | RFC 3339 UTC |
| `symbol` | string |
| `base_volatility` | number |

**`option_chains`** — one row per (step × expiration × strike).

| column | type |
|---|---|
| `step` | integer |
| `simulated_at` | RFC 3339 UTC |
| `symbol` | string |
| `expires_at` | RFC 3339 UTC |
| `labels` | `\|`-joined rule ids, sorted |
| `days_to_expiration` | number |
| `strike` | number |
| `implied_volatility` | number |
| `call_bid`, `call_ask`, `call_mid`, `call_delta` | number, optional |
| `put_bid`, `put_ask`, `put_mid`, `put_delta` | number, optional |
| `gamma` | number, optional |

### 10.2 Rendering

- **JSON** is a single valid array of row objects, streamed. Numeric columns are
  JSON **numbers**, not strings — the same `f64` REST-boundary representation
  used by the snapshot in §7, so a client parses one numeric convention across
  the whole v2 surface. An absent optional numeric is `null`.
- **CSV** is RFC 4180: a header row always present, `CRLF` line endings, fields
  quoted only when they contain a comma, a quote or a line break, and embedded
  quotes doubled. An absent optional numeric is an **empty field** — not
  `null`, not `0`. `labels` is joined with `|` inside one field precisely so a
  multi-label chain never depends on quoting to stay one column.
- **Numbers** are rendered from the same `f64` the JSON carries, using Rust's
  shortest round-trip formatting: no locale, no thousands separator, `.` as the
  decimal point, no exponent for the magnitudes involved here. The same input
  therefore always produces the same characters.
- **Timestamps** are `YYYY-MM-DDTHH:MM:SSZ` in both formats.

Together these make a repeated export of the same simulation byte-identical,
which is the property a backtest harness needs in order to cache.

---

## 11. Error semantics

v2 maps failures through `ChainError` and the single existing HTTP boundary
(`src/api/rest/error.rs`). No handler invents its own mapping.

| status | condition | body |
|---|---|---|
| `400` | invalid field, schedule, dataset, format, range, or a breached cap | `{ "error": "...", "field": "..." }` |
| `404` | unknown or expired simulation id | `{ "error": "..." }` |
| `409` | the compare-and-swap lost to a concurrent writer | `{ "error": "..." }` |
| `410` | the simulation is completed / the tape is exhausted | `{ "error": "..." }` |
| `412` | `expected_step` does not match the stored cursor | `{ "error": "...", "current_step": N }` |
| `500` | internal failure | `{ "error": "Internal server error" }` |

The `400` body is the existing `ValidationErrorResponse` shape — `error` plus
the offending `field` — so a client can point a user at one input. The `412`
body matches v1's precondition body exactly
(`src/api/rest/handlers.rs:374-377`). Error messages never contain credentials,
connection strings, or environment values.

**One implementation note that is easy to get wrong.** A schedule rule is
validated during deserialization, not after it, because the rule type owns its
own invariants (§4.4). Its `ChainError::Validation` is therefore flattened into
a serde error before any handler sees it, and actix's default JSON extractor
would render that as a plaintext `400` with no `field`. The v2 routes must
register a `web::JsonConfig` error handler that recovers the message into a
`ValidationErrorResponse`; without it, the whole rule-level class of §4.4
failures silently loses the structured `field` this section promises. That
handler is #47's to add, and is a required part of its acceptance.

---

## 12. Compatibility and release policy

### 12.1 v1 is frozen

No removal, no rename, and no change of meaning — of field **names**, **types**,
**optionality**, or **values** — for the whole v0.2.0 cycle:

- the routes `POST`/`GET`/`PUT`/`PATCH`/`DELETE /api/v1/chain` and
  `POST /api/v1/chain/step`;
- the query parameters `sessionid` and `expected_step`, spelled exactly as they
  are today;
- `CreateSessionRequest`, `UpdateSessionRequest` (including its `Patch`
  tri-state semantics), `ApiWalkType`, `ApiTimeFrame`, `SessionResponse`,
  `SessionParametersResponse`, `ChainResponse`, `OptionContractResponse`,
  `OptionPriceResponse`, `SessionInfoResponse`, `ErrorResponse`,
  `ValidationErrorResponse`;
- the ad-hoc bodies: the `412` `{error, current_step}`
  (`handlers.rs:374-377`) and the `DELETE` `200` `{message, session_id}`
  (`handlers.rs:738-742`);
- **rendered values, not only shapes** — in particular `state` is the `Display`
  form and stays `"In Progress"` with a space (`model.rs:81`,
  `handlers.rs:283`), and `time_frame` stays the `Display` form
  (`handlers.rs:274`). "Normalising" either of these to its serde spelling is a
  breaking change, not a cleanup;
- v1's `timestamp` semantics — it stays the wall-clock serve time. v2's
  determinism is **not** back-ported;
- serve-then-advance on `POST /api/v1/chain/step` and the safe repeatable
  `GET /api/v1/chain` peek;
- every status code and OpenAPI example.

**One documented exception, because this crate does not own it.**
`SessionParametersResponse.method` is a `serde_json::Value` produced from
upstream `optionstratlib::WalkType` (`responses.rs:36`, `handlers.rs:262`), and
optionstratlib has added `WalkType` variants in *patch* releases. An upstream
bump may therefore add a variant, and `ApiWalkType` must be extended to expose
it (the exhaustive match makes that a compile error by design, per CLAUDE.md).
That is additive; renaming or removing an existing variant's JSON shape is not,
and is out of scope for this stack.

Golden tests pin all of the above: a checked-in v1 request JSON and a v1
stored-session JSON that must keep deserialising, plus an OpenAPI snapshot for
the v1 paths. They land with #44 and #47 and guard every PR above them in the
stack.

### 12.2 Stored sessions

v2 sessions are a **separate type with a separate storage identity**: a distinct
Redis key prefix, an explicit `schema_version`, and — in the in-memory store —
a separate map, so a v2 id can never resolve a v1 `Session` or vice versa. Old
v1 JSON is never reinterpreted as rolling configuration, and a v1 session
written by an older binary keeps loading unchanged. Breaking either stored shape
remains a semver event.

### 12.3 OpenAPI

`actix_extras` remains unavailable — optionstratlib enables
`utoipa/axum_extras` unconditionally and utoipa's framework extras are mutually
exclusive (`Cargo.toml`). Every v2 path and query parameter (`{id}`, `dataset`,
`format`, `from_step`, `to_step`, `expected_step`) is therefore declared with a
manual `params(...)`, exactly as v1 does (`handlers.rs:330-333`).

v2 schema names must not collide with v1's in the shared components namespace
(`src/api/rest/swagger.rs`); v2 types take distinct names or an explicit
`#[schema(as = ...)]`, or the frozen v1 OpenAPI snapshot breaks.

### 12.4 Release

v2 is purely additive to the public API, so it ships as the minor bump to
`v0.2.0`. Publication to crates.io is a **separate, explicitly approved step**
(the `release-crate` procedure) after the whole implementation stack has merged
and CI is green on the release commit. No PR in this stack publishes.

---

## 13. Dependencies

Adding a dependency requires the repository owner's explicit approval
(`rules/global_rules.md`). Two crates are **required** by this stack, and both
were approved for it on 2026-08-14, before implementation:

| crate | version | to be added by | rationale |
|---|---|---|---|
| `chrono-tz` | `0.10`, features `["serde"]` | #43 | IANA timezone data for the local expiry cutoff (§4.3), integrating with the `chrono` stack already used throughout. `serde` support is needed to persist the zone in the stored v2 parameters. |
| `csv` | `1.4` | #49 | RFC 4180 quoting and escaping (§10.2), streaming into a `Write` sink so export stays bounded-memory. |

Neither is in `Cargo.toml` yet. Each will be declared by the PR that first needs
it, in `[workspace.dependencies]` as `major.minor`, and consumed by the root
crate with `{ workspace = true }` — never re-pinned inside a member crate.

**No other dependency is authorised by this ADR.** Anything else — an
exchange-holiday database in particular (§15) — needs its own approval before it
is added.

---

## 14. Worked example — the reference configuration

One rolling 0DTE, three Monday/Wednesday/Friday weeklies, and twelve
last-Friday monthlies, all expiring at 17:00 America/New_York.

### 14.1 Request

`POST /api/v2/simulations`

```json
{
  "symbol": "SPX",
  "steps": 500,
  "start_at": "2026-01-05T14:30:00Z",
  "step_interval_seconds": 86400,
  "timezone": "America/New_York",
  "calendar": "weekdays_v1",
  "expiration_time": "17:00",
  "schedules": [
    { "rule_id": "zero_dte", "kind": "daily",   "target_count": 1 },
    { "rule_id": "weeklies", "kind": "weekly",  "target_count": 3,
      "weekdays": ["Mon", "Wed", "Fri"] },
    { "rule_id": "monthlies","kind": "monthly", "target_count": 12,
      "weekday": "Fri" }
  ],
  "initial_price": 5000.0,
  "volatility": 0.18,
  "risk_free_rate": 0.04,
  "dividend_yield": 0.012,
  "method": { "GeometricBrownian": { "dt": 0.004, "drift": 0.05, "volatility": 0.18 } },
  "time_frame": "Day",
  "chain_size": 15,
  "strike_interval": 25.0,
  "skew_slope": -0.2,
  "smile_curve": 0.4,
  "spread": 0.02,
  "seed": 42
}
```

### 14.2 Creation response

```json
{
  "id": "0a5b2c34-7f1e-4c2a-9b8d-1e2f3a4b5c6d",
  "state": "initialized",
  "version": 0,
  "cursor": { "current_step": 0, "total_steps": 500 },
  "parameters": {
    "symbol": "SPX",
    "steps": 500,
    "seed": 42,
    "effective_start": "2026-01-05T14:30:00Z",
    "step_interval_seconds": 86400,
    "time_frame": "Day",
    "timezone": "America/New_York",
    "calendar": "weekdays_v1",
    "tzdb_version": "2025b",
    "expiration_time": "17:00:00",
    "schedules": [
      { "rule_id": "monthlies","kind": "monthly", "target_count": 12, "weekday": "Fri" },
      { "rule_id": "weeklies", "kind": "weekly",  "target_count": 3,
        "weekdays": ["Mon", "Wed", "Fri"] },
      { "rule_id": "zero_dte", "kind": "daily",   "target_count": 1 }
    ],
    "initial_price": 5000.0,
    "volatility": 0.18,
    "risk_free_rate": 0.04,
    "dividend_yield": 0.012,
    "method": { "GeometricBrownian": { "dt": 0.004, "drift": 0.05, "volatility": 0.18 } },
    "chain_size": 15,
    "strike_interval": 25.0,
    "skew_slope": -0.2,
    "smile_curve": 0.4,
    "spread": 0.02
  }
}
```

The echo is **complete** — it is exactly the replay-input list of §8, so a
client that stores this response can reproduce the tape without the request.
The `schedules` come back **normalised** — sorted by `rule_id`, weekday sets
sorted, `expiration_time` expanded to `HH:MM:SS` — because it is the normalised
form, not the submitted form, that is the replay input.

### 14.3 Inventory at step 0

`simulated_at` = `2026-01-05T14:30:00Z`, which is Monday 2026-01-05 09:30 New
York. January is EST, so a 17:00 local expiry is `22:00Z`.

| `expires_at` | local | labels |
|---|---|---|
| `2026-01-05T22:00:00Z` | Mon 05 Jan 17:00 | `weeklies`, `zero_dte` |
| `2026-01-07T22:00:00Z` | Wed 07 Jan 17:00 | `weeklies` |
| `2026-01-09T22:00:00Z` | Fri 09 Jan 17:00 | `weeklies` |
| `2026-01-30T22:00:00Z` | Fri 30 Jan 17:00 | `monthlies` |
| `2026-02-27T22:00:00Z` | Fri 27 Feb 17:00 | `monthlies` |
| `2026-03-27T21:00:00Z` | Fri 27 Mar 17:00 | `monthlies` |
| … nine more last-Fridays, through `2026-12-25T22:00:00Z` | | `monthlies` |

Fifteen chains, not sixteen: Monday's 0DTE and the first weekly are the same
physical expiration, priced once and carrying both labels. Each rule is still
satisfied — `weeklies` counts that shared expiry as the first of its three.

Note `2026-03-27T21:00:00Z`: New York is on EDT by late March, so the same
17:00 local expiration is an hour earlier in UTC. The local time is what the
schedule fixes.

### 14.4 Crossing the cutoff

Walking to a `simulated_at` of Monday 17:00 local (`2026-01-05T22:00:00Z`):

- `2026-01-05T22:00:00Z` satisfies `expires_at <= simulated_at`, so it is
  expired and gone;
- `zero_dte` replenishes to Tuesday `2026-01-06T22:00:00Z` in **the same
  snapshot**;
- `weeklies` replenishes to `2026-01-12T22:00:00Z`, keeping three;
- the Wednesday and Friday weeklies are unchanged.

At `2026-01-05T21:59:59Z` the Monday expiry is still present with a small
positive `days_to_expiration`. There is no step in between at which either rule
holds fewer expirations than its `target_count`.

### 14.5 The awkward calendars, resolved

| case | behaviour |
|---|---|
| Friday-evening 0DTE | rolls to **Monday** — Saturday and Sunday are not eligible |
| month boundary | the last Friday of the next month is projected as soon as this month's is consumed; `monthlies` never drops below 12 |
| year boundary | projection crosses into the next year unchanged: once the 2026-01-30 monthly expires, the twelfth becomes 2027-01-29 |
| leap year | 2028-02-25 is the last Friday of a 29-day February and is projected as such |
| DST fold | ambiguous local 17:00 resolves to the **earlier** instant |
| DST gap | a local time inside the gap resolves to the first instant after the gap (§4.3) |
| duplicate expiration | one chain, both labels, both rules counted |
| invalid schedule | `400` with the offending field (§4.4) |

---

## 15. Out of scope

Explicitly **not** part of this contract or the v0.2.0 stack:

- a full exchange-holiday database (the hook exists; the data does not);
- stochastic skew or smile coefficients — skew and smile stay static inputs;
- any frontend or visualisation work;
- ZIP or multi-dataset bundle downloads;
- warehouse persistence of generated chains — export rebuilds, it does not
  store;
- v1 deprecation or removal;
- `PATCH` / `PUT` on v2 simulations (§6);
- redesigning upstream `OptionSeries` / `generator_optionseries`;
- publishing the crate (§12.4).

---

## 16. Consequences

**Good.** v1 consumers, IronCondor included, are untouched. The replay
guarantee becomes checkable rather than aspirational, because §8 names a finite
list of inputs. Memory stops scaling with the horizon: the tape stores factor
rows, and chains are built on demand. Long horizons stop being a special case.

**Costly.** Two parallel API surfaces, two session types, and two stored
schemas have to be maintained until v1 is retired — and retiring it is not in
this stack. v2 deliberately carries v1's `expected_step`/`412` vocabulary while
diverging on the id location and the `state` spelling, so the two surfaces are
similar without being identical; §6 and §7 say exactly where they part. The
planner reimplements calendar arithmetic that a holiday-aware exchange calendar
would eventually replace.

**Risky, and mitigated.** The largest risk is a silent divergence between the
mirrored upstream walk kernels and the factor tape — the same class of risk the
existing `Walker` already carries. It is mitigated the same way: same-seed ⇒
identical-tape tests, extended in #45 and #46 to cover full rows and full
snapshots rather than spot prices alone, and a `reproducibility-reviewer` audit
on every PR that touches the walk path.
