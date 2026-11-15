//! How a person works with agents, read from metadata alone: their
//! coordination type, streak, badges, weekly report card, year in review
//! (Wrapped), a team's leaderboard, and a signed export of it all.
//!
//! Only ledger rows Cork already writes, never code, prompts or message
//! text: `edit_reported` and `turn_usage` (when an agent session was at work,
//! and its agent hours, `active_s`), `agent_message` (messages and handoffs
//! sent to agents, counted, never read), the collision rows the waste report
//! counts (`gate_block`, `op_conflict`, `collision_prevented`,
//! `claim_prevented`, de-duplicated by [`crate::waste::incidents_by_person`]),
//! `block_outcome` (what the agent did after one) and the savings rows
//! (tokens saved, net of the briefings' own cost, as everywhere else).
//!
//! Weeks are ISO weeks, Monday 00:00 UTC (waste.rs's). A finished week is
//! tallied once per workspace, every member at once, and kept in
//! [`WEEK_BUCKET`]; the sweep does last week's every Monday whether or not
//! anyone looks, so Wrapped has a year of weeks. A type reads the last
//! [`TYPE_WEEKS`] full weeks, so it changes once a week, as habits do, and
//! needs [`NEED_DAYS`] days of work across [`NEED_WEEKS`] weeks: one busy
//! afternoon cannot make anyone a Conductor. The rules are public, word for
//! word, on the site's /types page (nextjs-app/src/app/types/rubric.ts):
//! change one and you change the other.
//!
//! What anyone else sees is opt-in: a public profile is off until its owner
//! turns it on, and the leaderboard shows teammates' counts, never their
//! type.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc, Weekday};
use serde_json::{json, Map, Value};

use crate::briefstat::row_saving;
use crate::store::{LedgerRow, Store};
use crate::waste::{agent_of, week_label, week_start, CAUGHT_PARTS, STALE_PARTS};
use crate::App;

/// `{workspace}:{monday}` -> `{v, people: {label: week}}`, and `rolled:{monday}`.
pub const WEEK_BUCKET: &str = "habits_week";
/// uid -> `{on, slug, label, name}`: the public profile switch.
pub const PUBLIC_BUCKET: &str = "habits_public";
/// slug -> `{uid}`: whose public profile an address is.
pub const SLUG_BUCKET: &str = "habits_slug";
/// The kept weeks' shape: a kept week of another version is tallied again.
const VERSION: u64 = 1;

/// Each edit or billed turn counts its session as at work this long either side.
pub const PAD_S: f64 = 300.0;
/// Agents at once, counted up to this; more counts as this many.
pub const MAX_LEVEL: usize = 8;
/// The type reads this many full weeks.
pub const TYPE_WEEKS: i64 = 4;
/// ...and needs work on this many days, in this many of those weeks.
pub const NEED_DAYS: usize = 6;
pub const NEED_WEEKS: usize = 2;
/// A week nobody kept is tallied from the ledger only this far back.
const BACKFILL_WEEKS: i64 = 13;
/// Streaks and badges read this many weeks back.
const YEAR_WEEKS: i64 = 53;
/// The current week is tallied again at most this often.
const NOW_TTL_S: f64 = 300.0;
const DAY_S: f64 = 86_400.0;
/// A signed report holds for two years; a card's image link for thirty days.
const REPORT_TTL_S: f64 = 2.0 * 365.0 * DAY_S;
const CARD_TTL_S: f64 = 30.0 * DAY_S;

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn count(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn float(v: &Value, key: &str) -> f64 {
    v.get(key).and_then(Value::as_f64).filter(|x| x.is_finite()).unwrap_or(0.0)
}

fn round(x: f64, places: usize) -> f64 {
    crate::compat::python_round(x, places)
}

fn day_of(ts: f64) -> NaiveDate {
    DateTime::<Utc>::from_timestamp(ts.floor() as i64, 0).unwrap_or_default().date_naive()
}

fn ts_of(day: NaiveDate) -> f64 {
    day.and_hms_opt(0, 0, 0).map(|d| d.and_utc().timestamp() as f64).unwrap_or(0.0)
}

fn weekend(day: NaiveDate) -> bool {
    matches!(day.weekday(), Weekday::Sat | Weekday::Sun)
}

/// The person a sender label names: `a@x`, `a@x#session`, `a@x#session/lead`.
fn person_of(label: &str) -> &str {
    label.split(['#', '/']).next().unwrap_or("").trim()
}

// --------------------------------------------------------------- one week

/// One person's week in one workspace (or several weeks, or workspaces,
/// added together).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Week {
    /// UTC days with an edit or a billed turn
    pub days: BTreeSet<NaiveDate>,
    pub sessions: u64,
    /// seconds at work with exactly i+1 agent sessions at once; the last slot
    /// is MAX_LEVEL or more
    pub level_s: [f64; MAX_LEVEL],
    pub peak: u64,
    pub edits: u64,
    /// messages sent to agents, handoffs included
    pub messages: u64,
    /// a handoff, or a message anchored to code (work handed to whoever is on it)
    pub handoffs: u64,
    /// collisions Cork stopped on this person's agents
    pub catches: u64,
    pub adapted: u64,
    pub waited: u64,
    pub overrode: u64,
    pub abandoned: u64,
    /// agent hours, as billing meters them
    pub hours_s: f64,
    /// net of the briefings' own cost
    pub tokens_saved: i64,
    /// hours in which 3+ people of the workspace had agents at work, this one among them
    pub crowded: u64,
    /// edits per repo
    pub repos: BTreeMap<String, u64>,
}

impl Week {
    pub fn add(&mut self, w: &Week) {
        self.days.extend(w.days.iter().copied());
        self.sessions += w.sessions;
        for (mine, theirs) in self.level_s.iter_mut().zip(w.level_s.iter()) {
            *mine += theirs;
        }
        self.peak = self.peak.max(w.peak);
        self.edits += w.edits;
        self.messages += w.messages;
        self.handoffs += w.handoffs;
        self.catches += w.catches;
        self.adapted += w.adapted;
        self.waited += w.waited;
        self.overrode += w.overrode;
        self.abandoned += w.abandoned;
        self.hours_s += w.hours_s;
        self.tokens_saved += w.tokens_saved;
        self.crowded += w.crowded;
        for (repo, n) in &w.repos {
            *self.repos.entry(repo.clone()).or_insert(0) += n;
        }
    }

    /// Time at least one session was at work.
    pub fn work_s(&self) -> f64 {
        self.level_s.iter().sum()
    }

    /// The share of working time with `k` or more sessions at once.
    pub fn share_from(&self, k: usize) -> f64 {
        let work = self.work_s();
        if work <= 0.0 || k == 0 {
            return 0.0;
        }
        self.level_s.iter().skip(k - 1).sum::<f64>() / work
    }

    /// Sessions at once on average, while any was at work.
    pub fn avg_at_once(&self) -> f64 {
        let work = self.work_s();
        if work <= 0.0 {
            return 0.0;
        }
        self.level_s.iter().enumerate().map(|(i, s)| (i + 1) as f64 * s).sum::<f64>() / work
    }

    pub fn settled(&self) -> u64 {
        self.adapted + self.waited + self.overrode + self.abandoned
    }

    /// Collisions after which the agent adapted to the teammate's work or let it finish.
    pub fn gave_way(&self) -> u64 {
        self.adapted + self.waited
    }

    pub fn gave_way_rate(&self) -> f64 {
        let settled = self.settled();
        if settled == 0 { 0.0 } else { self.gave_way() as f64 / settled as f64 }
    }

    pub fn hours(&self) -> f64 {
        self.hours_s / 3600.0
    }

    pub fn to_json(&self) -> Value {
        json!({
            "days": self.days.iter().map(|d| d.format("%Y-%m-%d").to_string()).collect::<Vec<_>>(),
            "sessions": self.sessions,
            "level_s": self.level_s.iter().map(|s| round(*s, 1)).collect::<Vec<_>>(),
            "peak": self.peak,
            "edits": self.edits,
            "messages": self.messages,
            "handoffs": self.handoffs,
            "catches": self.catches,
            "adapted": self.adapted,
            "waited": self.waited,
            "overrode": self.overrode,
            "abandoned": self.abandoned,
            "hours_s": round(self.hours_s, 1),
            "tokens_saved": self.tokens_saved,
            "crowded": self.crowded,
            "repos": self.repos,
        })
    }

    pub fn from_json(v: &Value) -> Week {
        let mut level_s = [0.0; MAX_LEVEL];
        for (slot, s) in level_s.iter_mut().zip(v.get("level_s").and_then(Value::as_array).into_iter().flatten()) {
            *slot = s.as_f64().unwrap_or(0.0);
        }
        Week {
            days: v
                .get("days")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|d| NaiveDate::parse_from_str(d.as_str()?, "%Y-%m-%d").ok())
                .collect(),
            sessions: count(v, "sessions"),
            level_s,
            peak: count(v, "peak"),
            edits: count(v, "edits"),
            messages: count(v, "messages"),
            handoffs: count(v, "handoffs"),
            catches: count(v, "catches"),
            adapted: count(v, "adapted"),
            waited: count(v, "waited"),
            overrode: count(v, "overrode"),
            abandoned: count(v, "abandoned"),
            hours_s: float(v, "hours_s"),
            tokens_saved: v.get("tokens_saved").and_then(Value::as_i64).unwrap_or(0),
            crowded: count(v, "crowded"),
            repos: v
                .get("repos")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .map(|(k, n)| (k.clone(), n.as_u64().unwrap_or(0)))
                .collect(),
        }
    }
}

/// Every ledger kind a week reads.
pub fn kinds() -> Vec<&'static str> {
    let mut kinds = vec!["edit_reported", "turn_usage", "agent_message", "block_outcome", "gate_block", "op_conflict"];
    for (kind, _) in crate::briefstat::SOURCE_KINDS {
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
    }
    kinds
}

/// Every person's week from rows already read: `rows` are (repo, row)
/// pairs, `from..to` the week. Pure, so the tests can seed rows.
pub fn tally(rows: &[(String, LedgerRow)], from: f64, to: f64) -> BTreeMap<String, Week> {
    let mut people: BTreeMap<String, Week> = BTreeMap::new();
    // person -> session -> the spans it was at work
    let mut spans: BTreeMap<String, BTreeMap<String, Vec<(f64, f64)>>> = BTreeMap::new();
    let mut hours: BTreeMap<i64, BTreeSet<String>> = BTreeMap::new();
    let mut sends: BTreeSet<String> = BTreeSet::new();
    for (repo, row) in rows {
        if row.ts < from || row.ts >= to {
            continue;
        }
        let p = &row.payload;
        if let Some(worth) = row_saving(&row.kind, p) {
            let user = text(p, "user");
            if !user.is_empty() {
                people.entry(user.to_string()).or_default().tokens_saved += worth.tokens_saved as i64 - worth.cost_tokens as i64;
            }
        }
        match row.kind.as_str() {
            "edit_reported" | "turn_usage" => {
                let user = text(p, "user");
                if user.is_empty() {
                    continue;
                }
                let week = people.entry(user.to_string()).or_default();
                week.days.insert(day_of(row.ts));
                if row.kind == "turn_usage" {
                    week.hours_s += float(p, "active_s");
                } else {
                    week.edits += 1;
                    *week.repos.entry(repo.clone()).or_insert(0) += 1;
                }
                // a subagent works inside its session: one agent, not two
                let session = agent_of(user, text(p, "session"));
                spans
                    .entry(user.to_string())
                    .or_default()
                    .entry(session)
                    .or_default()
                    .push(((row.ts - PAD_S).max(from), (row.ts + PAD_S).min(to)));
                hours.entry((row.ts / 3600.0).floor() as i64).or_default().insert(user.to_string());
            }
            "agent_message" => {
                let sender = text(p, "from");
                let user = person_of(sender);
                if user.is_empty() {
                    continue;
                }
                let anchor = text(p, "anchor");
                // an anchored send writes one row per recipient: one send
                if !sends.insert(format!("{sender}|{}|{anchor}", row.ts)) {
                    continue;
                }
                let week = people.entry(user.to_string()).or_default();
                week.messages += 1;
                if !anchor.is_empty() || p.get("handoff").and_then(Value::as_bool) == Some(true) {
                    week.handoffs += 1;
                }
            }
            "block_outcome" => {
                let kind = text(p, "kind");
                let user = text(p, "user");
                if user.is_empty() || !(STALE_PARTS.contains(&kind) || CAUGHT_PARTS.contains(&kind)) {
                    continue;
                }
                let week = people.entry(user.to_string()).or_default();
                match text(p, "outcome") {
                    "adapted" => week.adapted += 1,
                    "waited" => week.waited += 1,
                    "overrode" => week.overrode += 1,
                    "abandoned" => week.abandoned += 1,
                    _ => {}
                }
            }
            _ => {}
        }
    }
    for (user, first) in crate::waste::incidents_by_person(rows) {
        if !user.is_empty() && first >= from && first < to {
            people.entry(user).or_default().catches += 1;
        }
    }
    for (user, sessions) in spans {
        let week = people.entry(user).or_default();
        week.sessions = sessions.len() as u64;
        let (level_s, peak) = levels(sessions.into_values().collect());
        week.level_s = level_s;
        week.peak = peak;
    }
    for present in hours.values().filter(|p| p.len() >= 3) {
        for user in present {
            people.entry(user.clone()).or_default().crowded += 1;
        }
    }
    people
}

/// Time at each number of sessions at once, and the most at once, from
/// each session's spans.
fn levels(sessions: Vec<Vec<(f64, f64)>>) -> ([f64; MAX_LEVEL], u64) {
    let mut edges: Vec<(f64, i64)> = Vec::new();
    for mut spans in sessions {
        spans.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut merged: Vec<(f64, f64)> = Vec::new();
        for (start, end) in spans.into_iter().filter(|(s, e)| e > s) {
            match merged.last_mut() {
                Some(last) if start <= last.1 => last.1 = last.1.max(end),
                _ => merged.push((start, end)),
            }
        }
        for (start, end) in merged {
            edges.push((start, 1));
            edges.push((end, -1));
        }
    }
    // an end before a start at the same instant: touching is not overlapping
    edges.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    let mut out = [0.0; MAX_LEVEL];
    let (mut level, mut peak, mut at) = (0i64, 0u64, 0.0);
    for (t, step) in edges {
        if level > 0 {
            out[(level as usize).min(MAX_LEVEL) - 1] += t - at;
        }
        level += step;
        peak = peak.max(level.max(0) as u64);
        at = t;
    }
    (out, peak)
}

// ------------------------------------------------------------------ types

/// A coordination type. The site's /types page says the same, word for word.
pub struct TypeDef {
    pub id: &'static str,
    pub name: &'static str,
    pub line: &'static str,
    pub rule: &'static str,
    pub rare: bool,
}

/// In the order they are tried: the first whose rule holds is the type.
pub const TYPES: [TypeDef; 10] = [
    TypeDef {
        id: "air_traffic_control", name: "Air Traffic Control", rare: true,
        line: "Five agents in the air, work routed between them, and every near-miss handled.",
        rule: "5 or more agents at once at some point; 3 or more at once for at least a quarter of working time; 20 or more messages or handoffs to agents; and after at least 90% of 10 or more settled collisions, the agent adapted or waited.",
    },
    TypeDef {
        id: "clean_sheet", name: "Clean Sheet", rare: true,
        line: "A month of three agents side by side, and not one collision.",
        rule: "Agent work in each of the 4 weeks; 40 or more agent hours; 3 or more agents at once for at least 20% of working time; and no collisions at all.",
    },
    TypeDef {
        id: "cowboy", name: "Cowboy", rare: false,
        line: "Rides into busy code, collides often, and rarely changes course.",
        rule: "8 or more collisions, at least one per active day; and after fewer than half of 5 or more settled collisions, the agent adapted or waited.",
    },
    TypeDef {
        id: "conductor", name: "Conductor", rare: false,
        line: "Runs several agents at once and routes work between them.",
        rule: "2 or more agents at once for at least 40% of working time, and 10 or more messages or handoffs to agents.",
    },
    TypeDef {
        id: "good_neighbor", name: "Good Neighbor", rare: false,
        line: "Works where others work, and gives way when a teammate's change lands first.",
        rule: "5 or more collisions; and after at least 75% of 5 or more settled collisions, the agent adapted or waited.",
    },
    TypeDef {
        id: "swarm", name: "Swarm", rare: false,
        line: "Many agents at once, each left to its own lane.",
        rule: "3 or more agents at once for at least 20% of working time.",
    },
    TypeDef {
        id: "duet", name: "Duet", rare: false,
        line: "Two agents side by side, most of the time.",
        rule: "2 or more agents at once for at least 40% of working time.",
    },
    TypeDef {
        id: "relay", name: "Relay", rare: false,
        line: "One agent at a time, with work passed from session to session.",
        rule: "3 or more handoffs.",
    },
    TypeDef {
        id: "solo_act", name: "Solo Act", rare: false,
        line: "One agent at a time, start to finish.",
        rule: "2 or more agents at once for less than 15% of working time.",
    },
    TypeDef {
        id: "all_rounder", name: "All-Rounder", rare: false,
        line: "A bit of everything: no one habit stands out yet.",
        rule: "None of the above.",
    },
];

pub fn type_def(id: &str) -> Option<&'static TypeDef> {
    TYPES.iter().find(|t| t.id == id)
}

fn type_json(t: &TypeDef) -> Value {
    json!({"id": t.id, "name": t.name, "line": t.line, "rule": t.rule, "rare": t.rare})
}

/// Which rule holds for `w`, the weeks added together: `active_weeks` of
/// `of_weeks` had agent work.
fn rule_of(w: &Week, active_weeks: usize, of_weeks: usize) -> &'static str {
    let days = w.days.len() as u64;
    let (two, three) = (w.share_from(2), w.share_from(3));
    let (settled, gave) = (w.settled(), w.gave_way_rate());
    if w.peak >= 5 && three >= 0.25 && w.messages >= 20 && settled >= 10 && gave >= 0.9 {
        "air_traffic_control"
    } else if of_weeks as i64 >= TYPE_WEEKS && active_weeks == of_weeks && w.hours() >= 40.0 && three >= 0.2 && w.catches == 0 {
        "clean_sheet"
    } else if w.catches >= 8 && w.catches >= days && settled >= 5 && gave < 0.5 {
        "cowboy"
    } else if two >= 0.4 && w.messages >= 10 {
        "conductor"
    } else if w.catches >= 5 && settled >= 5 && gave >= 0.75 {
        "good_neighbor"
    } else if three >= 0.2 {
        "swarm"
    } else if two >= 0.4 {
        "duet"
    } else if w.handoffs >= 3 {
        "relay"
    } else if two < 0.15 {
        "solo_act"
    } else {
        "all_rounder"
    }
}

/// The type the full weeks `weeks` add up to, or, before there is enough
/// to go on, (days with work, weeks with work) so far.
pub fn classify(weeks: &[Week]) -> Result<&'static TypeDef, (usize, usize)> {
    let mut all = Week::default();
    for w in weeks {
        all.add(w);
    }
    let active_weeks = weeks.iter().filter(|w| !w.days.is_empty()).count();
    if all.days.len() < NEED_DAYS || active_weeks < NEED_WEEKS {
        return Err((all.days.len(), active_weeks));
    }
    Ok(type_def(rule_of(&all, active_weeks, weeks.len())).unwrap_or(&TYPES[TYPES.len() - 1]))
}

/// The numbers the rules read, so a person can see why.
fn signals(w: &Week, active_weeks: usize) -> Value {
    json!({
        "active_days": w.days.len(), "weeks_active": active_weeks, "sessions": w.sessions,
        "hours": round(w.hours(), 1), "share_two_plus": round(w.share_from(2), 3), "share_three_plus": round(w.share_from(3), 3),
        "avg_at_once": round(w.avg_at_once(), 2), "peak": w.peak, "messages": w.messages, "handoffs": w.handoffs,
        "collisions": w.catches, "settled": w.settled(), "gave_way": w.gave_way(),
    })
}

// ---------------------------------------------------------------- streaks

/// The run of worked days at each day from the first worked one to
/// `today`. A Saturday or Sunday without work neither counts nor breaks a
/// run, and neither does today before it has any.
fn runs(days: &BTreeSet<NaiveDate>, today: NaiveDate) -> Vec<(NaiveDate, u32)> {
    let Some(mut day) = days.iter().next().copied() else { return Vec::new() };
    let mut out = Vec::new();
    let mut run = 0u32;
    while day <= today {
        if days.contains(&day) {
            run += 1;
        } else if day != today && !weekend(day) {
            run = 0;
        }
        out.push((day, run));
        let Some(next) = day.succ_opt() else { break };
        day = next;
    }
    out
}

/// (the streak now, the longest), in days with agent work.
pub fn streaks(days: &BTreeSet<NaiveDate>, today: NaiveDate) -> (u32, u32) {
    let runs = runs(days, today);
    (runs.last().map(|r| r.1).unwrap_or(0), runs.iter().map(|r| r.1).max().unwrap_or(0))
}

// ----------------------------------------------------------------- badges

pub struct BadgeDef {
    pub id: &'static str,
    pub name: &'static str,
    pub rule: &'static str,
    pub rare: bool,
    /// locked until someone this person invited joins
    pub invite: bool,
}

pub const BADGES: [BadgeDef; 11] = [
    BadgeDef { id: "first_catch", name: "First catch", rule: "Cork stopped a collision on one of your agents.", rare: false, invite: false },
    BadgeDef { id: "catches_10", name: "Ten catches", rule: "10 collisions stopped on your agents.", rare: false, invite: false },
    BadgeDef { id: "catches_100", name: "A hundred catches", rule: "100 collisions stopped on your agents.", rare: false, invite: false },
    BadgeDef { id: "first_handoff", name: "First handoff", rule: "You or your agent handed work to another agent: a handoff, or a message anchored to code.", rare: false, invite: false },
    BadgeDef { id: "three_at_once", name: "Three at once", rule: "3 of your agents at work at the same time.", rare: false, invite: false },
    BadgeDef { id: "five_at_once", name: "Five at once", rule: "5 of your agents at work at the same time.", rare: false, invite: false },
    BadgeDef { id: "full_week", name: "Full week", rule: "A 5-day streak.", rare: false, invite: false },
    BadgeDef { id: "twenty_days", name: "Twenty days", rule: "A 20-day streak.", rare: false, invite: false },
    BadgeDef { id: "hours_100", name: "A hundred hours", rule: "100 agent hours.", rare: false, invite: false },
    BadgeDef { id: "gave_way", name: "Gave way", rule: "After 10 collisions, your agent adapted to the teammate's change or waited for it.", rare: false, invite: false },
    BadgeDef {
        id: "full_house", name: "Full house",
        rule: "3 or more people of one workspace with agents at work in the same hour, you among them. Unlocks once someone you invited joins.",
        rare: true, invite: true,
    },
];

/// Every badge, earned or not, with the week it was first earned; the
/// invite badge stays locked until `invited`.
pub fn badges(weeks: &BTreeMap<NaiveDate, Week>, today: NaiveDate, invited: bool) -> Vec<Value> {
    let mut earned: BTreeMap<&'static str, NaiveDate> = BTreeMap::new();
    let mut total = Week::default();
    for (monday, week) in weeks {
        total.add(week);
        for (id, yes) in [
            ("first_catch", total.catches >= 1),
            ("catches_10", total.catches >= 10),
            ("catches_100", total.catches >= 100),
            ("first_handoff", total.handoffs >= 1),
            ("three_at_once", total.peak >= 3),
            ("five_at_once", total.peak >= 5),
            ("hours_100", total.hours() >= 100.0),
            ("gave_way", total.gave_way() >= 10),
            ("full_house", total.crowded >= 1),
        ] {
            if yes {
                earned.entry(id).or_insert(*monday);
            }
        }
    }
    let days: BTreeSet<NaiveDate> = weeks.values().flat_map(|w| w.days.iter().copied()).collect();
    let runs = runs(&days, today);
    for (length, id) in [(5, "full_week"), (20, "twenty_days")] {
        if let Some((day, _)) = runs.iter().find(|(_, run)| *run >= length) {
            earned.insert(id, *day - Duration::days(day.weekday().num_days_from_monday() as i64));
        }
    }
    BADGES
        .iter()
        .map(|b| {
            let locked = b.invite && !invited;
            let at = earned.get(b.id).filter(|_| !locked);
            json!({
                "id": b.id, "name": b.name, "rule": b.rule, "rare": b.rare, "locked": locked,
                "earned": at.is_some(), "week": at.map(|m| week_label(*m)),
            })
        })
        .collect()
}

// ------------------------------------------------------------ kept weeks

fn week_key(workspace: &str, monday: NaiveDate) -> String {
    format!("{workspace}:{}", monday.format("%Y-%m-%d"))
}

fn people_of(record: &Value) -> BTreeMap<String, Week> {
    record
        .get("people")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(label, w)| (label.clone(), Week::from_json(w)))
        .collect()
}

fn people_json(people: &BTreeMap<String, Week>) -> Value {
    Value::Object(people.iter().map(|(label, w)| (label.clone(), w.to_json())).collect::<Map<_, _>>())
}

/// Every repo of the workspace, as the owner sees them.
fn scopes_of(store: &Store, workspace: &str) -> Vec<String> {
    let owner = crate::workspaces::get(store, workspace).map(|w| crate::text(&w, "owner")).unwrap_or_default();
    let member = crate::auth::member(store, workspace, &owner);
    crate::access::visible_scopes(store, workspace, member.as_ref()).into_iter().collect()
}

fn read(store: &Store, workspace: &str, since: f64, until: f64, now: f64) -> Vec<(String, LedgerRow)> {
    let kinds = kinds();
    let mut rows = Vec::new();
    for scope in scopes_of(store, workspace) {
        // settle what timed out before reading it (waste.rs does the same)
        crate::outcomes::sweep(store, &scope, now);
        let repo = scope.split_once(':').map(|(_, repo)| repo).unwrap_or(&scope).to_string();
        rows.extend(store.ledger_since_kinds(&scope, since, &kinds).into_iter().filter(|r| r.ts < until).map(|r| (repo.clone(), r)));
    }
    rows
}

/// One workspace's people, week by week, for the Mondays asked: kept weeks
/// as kept, the rest tallied from the ledger (no earlier than the plan's
/// history or [`BACKFILL_WEEKS`]) and kept once finished. The current
/// week is tallied again every few minutes and never kept.
pub fn workspace_weeks(store: &Store, workspace: &str, mondays: &[NaiveDate], now: f64) -> BTreeMap<NaiveDate, BTreeMap<String, Week>> {
    let current = week_start(now);
    let floor = crate::access::plan_for(store, workspace).history_window_s().map(|w| now - w).unwrap_or(0.0);
    let oldest = current - Duration::weeks(BACKFILL_WEEKS);
    let mut out = BTreeMap::new();
    let mut missing: Vec<NaiveDate> = Vec::new();
    for monday in mondays.iter().copied().collect::<BTreeSet<_>>() {
        if monday > current {
            continue;
        }
        if monday == current {
            out.insert(monday, this_week(store, workspace, now, floor));
            continue;
        }
        match store.kv_get(WEEK_BUCKET, &week_key(workspace, monday)).filter(|r| count(r, "v") == VERSION) {
            Some(record) => {
                out.insert(monday, people_of(&record));
            }
            None if monday >= oldest && ts_of(monday + Duration::weeks(1)) > floor => missing.push(monday),
            None => {}
        }
    }
    if let (Some(first), Some(last)) = (missing.first().copied(), missing.last().copied()) {
        let rows = read(store, workspace, ts_of(first).max(floor), ts_of(last + Duration::weeks(1)), now);
        let mut by_week: BTreeMap<NaiveDate, Vec<(String, LedgerRow)>> = BTreeMap::new();
        for row in rows {
            by_week.entry(week_start(row.1.ts)).or_default().push(row);
        }
        for monday in missing {
            let from = ts_of(monday).max(floor);
            let people = tally(by_week.get(&monday).map(Vec::as_slice).unwrap_or(&[]), from, ts_of(monday + Duration::weeks(1)));
            let _ = store.kv_put(WEEK_BUCKET, &week_key(workspace, monday), &json!({"v": VERSION, "people": people_json(&people)}), now);
            out.insert(monday, people);
        }
    }
    out
}

fn this_week(store: &Store, workspace: &str, now: f64, floor: f64) -> BTreeMap<String, Week> {
    let monday = week_start(now);
    let key = format!("habitsnow:{workspace}");
    if let Some(kept) = store.eph_get(&key).filter(|k| text(k, "monday") == monday.format("%Y-%m-%d").to_string()) {
        return people_of(&kept);
    }
    let from = ts_of(monday).max(floor);
    let people = tally(&read(store, workspace, from, now + 60.0, now), from, now + 60.0);
    let _ = store.eph_set(&key, &json!({"monday": monday.format("%Y-%m-%d").to_string(), "people": people_json(&people)}), Some(NOW_TTL_S));
    people
}

/// Once a week (the notification sweep calls it), every workspace's last
/// full week is tallied and kept, so the weeks are there for Wrapped
/// whether or not anyone looked.
pub fn roll(store: &Store, at: f64) {
    let last = week_start(at) - Duration::weeks(1);
    let marker = format!("rolled:{}", last.format("%Y-%m-%d"));
    if store.kv_get(WEEK_BUCKET, &marker).is_some() {
        return;
    }
    let _ = store.kv_put(WEEK_BUCKET, &marker, &json!({"at": at}), at);
    for (id, ws) in store.kv_list("ws", "") {
        let id = if text(&ws, "id").is_empty() { id } else { text(&ws, "id").to_string() };
        workspace_weeks(store, &id, &[last], at);
    }
}

/// One person's weeks across `workspaces`, added together, for the Mondays asked.
fn person_weeks(store: &Store, workspaces: &[String], label: &str, mondays: &[NaiveDate], now: f64) -> BTreeMap<NaiveDate, Week> {
    let mut out: BTreeMap<NaiveDate, Week> = BTreeMap::new();
    for workspace in workspaces {
        for (monday, people) in workspace_weeks(store, workspace, mondays, now) {
            if let Some(week) = people.get(label) {
                out.entry(monday).or_default().add(week);
            }
        }
    }
    out
}

fn mondays_back(now: f64, weeks: i64) -> Vec<NaiveDate> {
    let current = week_start(now);
    (0..=weeks).rev().map(|k| current - Duration::weeks(k)).collect()
}

// ----------------------------------------------------------------- people

/// Who a view is about.
pub struct Me {
    pub uid: String,
    /// what the ledger calls them: email, else uid
    pub label: String,
    pub name: String,
    pub username: String,
    pub avatar: String,
    pub workspaces: Vec<String>,
}

impl Me {
    fn of(store: &Store, uid: &str, label: &str, name: &str, bound: &str) -> Me {
        let profile = store.kv_get("profile", uid).unwrap_or_else(|| json!({}));
        let workspaces = if bound.is_empty() {
            crate::workspaces::list_for(store, uid).iter().map(|w| crate::text(w, "id")).filter(|id| !id.is_empty()).collect()
        } else {
            vec![bound.to_string()]
        };
        let shown = [text(&profile, "name"), name, text(&profile, "github_login"), text(&profile, "username")]
            .into_iter()
            .find(|n| !n.trim().is_empty())
            .unwrap_or("")
            .trim()
            .to_string();
        Me {
            uid: uid.to_string(),
            label: label.to_string(),
            name: shown,
            username: text(&profile, "username").to_string(),
            avatar: text(&profile, "avatar").to_string(),
            workspaces,
        }
    }

    fn from_principal(store: &Store, p: &crate::auth::Principal) -> Me {
        Me::of(store, &p.uid, &p.label(), &p.name, &p.workspace)
    }
}

/// Has anyone this person invited joined? A member they added to any of
/// their workspaces, or the invite bonus billing granted them.
pub fn invited(store: &Store, uid: &str) -> bool {
    if store.kv_get(crate::billing::HOURS_FLAGS_BUCKET, &format!("bonus:invite:{uid}")).is_some() {
        return true;
    }
    crate::workspaces::list_for(store, uid).iter().any(|w| {
        crate::workspaces::members(store, &crate::text(w, "id"))
            .iter()
            .any(|m| crate::text(m, "added_by") == uid && crate::text(m, "uid") != uid)
    })
}

fn numbers(w: &Week) -> Value {
    json!({
        "catches": w.catches, "hours": round(w.hours(), 1), "tokens_saved": w.tokens_saved, "handoffs": w.handoffs,
        "messages": w.messages, "days": w.days.len(), "agents_at_once": w.peak, "edits": w.edits, "gave_way": w.gave_way(),
    })
}

fn change(last: &Value, prev: &Value) -> Value {
    let mut out = Map::new();
    for (key, value) in last.as_object().into_iter().flatten() {
        let (a, b) = (value.as_f64().unwrap_or(0.0), prev.get(key).and_then(Value::as_f64).unwrap_or(0.0));
        out.insert(key.clone(), json!(round(a - b, 1)));
    }
    Value::Object(out)
}

/// The four numbers on the card.
fn card_stats(w: &Week) -> Value {
    json!({"agents_at_once": w.peak, "catches": w.catches, "handoffs": w.handoffs, "hours": round(w.hours(), 1)})
}

/// A person's view: type, card, streak, badges and this week's report card.
pub fn view(store: &Store, me: &Me, now: f64) -> Value {
    let current = week_start(now);
    let weeks = person_weeks(store, &me.workspaces, &me.label, &mondays_back(now, YEAR_WEEKS), now);
    let full: Vec<NaiveDate> = (1..=TYPE_WEEKS).rev().map(|k| current - Duration::weeks(k)).collect();
    let window: Vec<Week> = full.iter().map(|m| weeks.get(m).cloned().unwrap_or_default()).collect();
    let mut sum = Week::default();
    for w in &window {
        sum.add(w);
    }
    let active_weeks = window.iter().filter(|w| !w.days.is_empty()).count();
    let today = day_of(now);
    let days: BTreeSet<NaiveDate> = weeks.values().flat_map(|w| w.days.iter().copied()).collect();
    let (streak, best) = streaks(&days, today);
    let invited = invited(store, &me.uid);
    let (kind, pending) = match classify(&window) {
        Ok(t) => (type_json(t), Value::Null),
        Err((days, weeks)) => (Value::Null, json!({"days": days, "weeks": weeks, "need_days": NEED_DAYS, "need_weeks": NEED_WEEKS})),
    };
    let last = weeks.get(&(current - Duration::weeks(1))).cloned().unwrap_or_default();
    let prev = weeks.get(&(current - Duration::weeks(2))).cloned().unwrap_or_default();
    let (last_n, prev_n) = (numbers(&last), numbers(&prev));
    let mut all = Week::default();
    for w in weeks.values() {
        all.add(w);
    }
    let public = public_record(store, &me.uid);
    let window_json = json!({
        "from": full[0].format("%Y-%m-%d").to_string(),
        "to": (current - Duration::days(1)).format("%Y-%m-%d").to_string(),
        "weeks": TYPE_WEEKS,
    });
    json!({
        "person": {"name": me.name, "username": me.username, "avatar": me.avatar},
        "type": kind,
        "pending": pending,
        "window": window_json,
        "card": card_stats(&sum),
        "signals": signals(&sum, active_weeks),
        "streak": {"current": streak, "best": best, "today": days.contains(&today)},
        "badges": badges(&weeks, today, invited),
        "week": {
            "label": week_label(current - Duration::weeks(1)), "prev_label": week_label(current - Duration::weeks(2)),
            "last": last_n, "prev": prev_n, "change": change(&last_n, &prev_n),
            "so_far": numbers(&weeks.get(&current).cloned().unwrap_or_default()),
        },
        "all_time": {"catches": all.catches, "hours": round(all.hours(), 1), "handoffs": all.handoffs, "days": all.days.len(),
                     "agents_at_once": all.peak, "since": weeks.keys().next().map(|m| m.format("%Y-%m-%d").to_string())},
        "invited": invited,
        "public": {"on": public.0, "slug": public.1},
        "computed_at": now,
    })
}

// --------------------------------------------------------------- the card

/// What the shareable card shows, from a view.
fn card_of(view: &Value) -> Value {
    json!({
        "name": view.pointer("/person/name").cloned().unwrap_or(json!("")),
        "username": view.pointer("/person/username").cloned().unwrap_or(json!("")),
        "type": view.get("type").cloned().unwrap_or(Value::Null),
        "stats": view.get("card").cloned().unwrap_or(json!({})),
        "streak": view.pointer("/streak/current").cloned().unwrap_or(json!(0)),
        "window": view.get("window").cloned().unwrap_or(json!({})),
    })
}

/// A signed link to the card's picture: the site draws it from what the
/// token carries, so a card cannot say what Cork did not.
fn card_token(store: &Store, view: &Value, now: f64) -> String {
    crate::notify::sign(store, &json!({"k": "card", "exp": now + CARD_TTL_S, "c": card_of(view)}))
}

// ---------------------------------------------------------- public profile

/// (on, slug).
fn public_record(store: &Store, uid: &str) -> (bool, Option<String>) {
    let record = store.kv_get(PUBLIC_BUCKET, uid).unwrap_or_else(|| json!({}));
    let slug = Some(text(&record, "slug").to_string()).filter(|s| !s.is_empty());
    (record.get("on").and_then(Value::as_bool) == Some(true), slug)
}

/// The address: the person's Cork username when it is free, else a random one.
fn new_slug(store: &Store, me: &Me) -> String {
    let taken = |s: &str| store.kv_get(SLUG_BUCKET, s).is_some_and(|r| text(&r, "uid") != me.uid);
    let username = me.username.trim().to_lowercase();
    if !username.is_empty() && !taken(&username) {
        return username;
    }
    loop {
        let slug: String = crate::compat::new_id().chars().filter(char::is_ascii_alphanumeric).take(10).collect::<String>().to_lowercase();
        if slug.len() >= 6 && !taken(&slug) {
            return slug;
        }
    }
}

/// Turn the public profile on or off. Off keeps the address, so turning it
/// back on brings the same link back.
pub fn set_public(store: &Store, me: &Me, on: bool, now: f64) -> Value {
    let (_, slug) = public_record(store, &me.uid);
    let slug = slug.unwrap_or_else(|| new_slug(store, me));
    let _ = store.kv_put(SLUG_BUCKET, &slug, &json!({"uid": me.uid}), now);
    let _ = store.kv_put(PUBLIC_BUCKET, &me.uid, &json!({"on": on, "slug": slug, "label": me.label, "name": me.name, "at": now}), now);
    let _ = store.eph_delete(&format!("habitspub:{slug}"));
    json!({"ok": true, "on": on, "slug": slug})
}

/// The public profile at `slug`, when its owner turned it on: the card,
/// the streak and the badges earned. Nothing else.
pub fn public_profile(store: &Store, slug: &str, now: f64) -> Option<Value> {
    let key = format!("habitspub:{slug}");
    if let Some(kept) = store.eph_get(&key) {
        return Some(kept);
    }
    let uid = store.kv_get(SLUG_BUCKET, slug).map(|r| text(&r, "uid").to_string())?;
    let record = store.kv_get(PUBLIC_BUCKET, &uid)?;
    if record.get("on").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let me = Me::of(store, &uid, text(&record, "label"), text(&record, "name"), "");
    let view = view(store, &me, now);
    let earned: Vec<Value> = view["badges"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| b["earned"] == json!(true))
        .map(|b| json!({"id": b["id"], "name": b["name"], "rule": b["rule"], "rare": b["rare"]}))
        .collect();
    let out = json!({
        "slug": slug,
        "card": card_of(&view),
        "avatar": me.avatar,
        "streak": view["streak"],
        "badges": earned,
        "pending": view["pending"],
    });
    let _ = store.eph_set(&key, &out, Some(NOW_TTL_S));
    Some(out)
}

// ------------------------------------------------------------ leaderboard

/// A workspace's members ranked over the last full weeks, on collisions
/// caught, handoffs and streaks. Only numbers above zero are ranked; whoever
/// has none is listed by name, without one. Never a teammate's type: that
/// is theirs to share. Locked until someone has joined on an invite.
pub fn leaderboard(store: &Store, workspace: &str, now: f64) -> Value {
    let members = crate::workspaces::members(store, workspace);
    let team = members.iter().any(|m| {
        let by = crate::text(m, "added_by");
        !by.is_empty() && by != crate::text(m, "uid")
    });
    if !team {
        return json!({"locked": true, "why": "The leaderboard opens once a teammate accepts an invite to this workspace."});
    }
    let current = week_start(now);
    let weeks = workspace_weeks(store, workspace, &mondays_back(now, BACKFILL_WEEKS), now);
    let window: BTreeSet<NaiveDate> = (1..=TYPE_WEEKS).map(|k| current - Duration::weeks(k)).collect();
    let today = day_of(now);
    let mut rows: Vec<(Value, u64, u64, u32)> = Vec::new();
    for m in &members {
        let email = crate::text(m, "email");
        let label = if email.is_empty() { crate::text(m, "uid") } else { email };
        let mut sum = Week::default();
        let mut days: BTreeSet<NaiveDate> = BTreeSet::new();
        for (monday, people) in &weeks {
            if let Some(w) = people.get(&label) {
                days.extend(w.days.iter().copied());
                if window.contains(monday) {
                    sum.add(w);
                }
            }
        }
        let who = json!({"uid": crate::text(m, "uid"), "label": label, "name": crate::text(m, "name")});
        rows.push((who, sum.catches, sum.handoffs, streaks(&days, today).0));
    }
    let board = |pick: &dyn Fn(&(Value, u64, u64, u32)) -> u64| -> Value {
        let mut ranked: Vec<(&Value, u64)> = rows.iter().map(|r| (&r.0, pick(r))).filter(|(_, n)| *n > 0).collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then(crate::text(a.0, "label").cmp(&crate::text(b.0, "label"))));
        let mut out: Vec<Value> = Vec::new();
        for (i, (who, n)) in ranked.iter().enumerate() {
            // a tie shares the place above it
            let place = if i > 0 && ranked[i - 1].1 == *n { out[i - 1]["place"].clone() } else { json!(i + 1) };
            out.push(json!({"who": who, "value": n, "place": place}));
        }
        Value::Array(out)
    };
    json!({
        "locked": false,
        "weeks": TYPE_WEEKS,
        "from": (current - Duration::weeks(TYPE_WEEKS)).format("%Y-%m-%d").to_string(),
        "boards": {
            "catches": board(&|r| r.1),
            "handoffs": board(&|r| r.2),
            "streak": board(&|r| r.3 as u64),
        },
        "members": rows.iter().map(|r| r.0.clone()).collect::<Vec<_>>(),
    })
}

// ---------------------------------------------------------------- wrapped

/// The year in review: `year`'s weeks, as kept.
pub fn wrapped(store: &Store, me: &Me, year: i32, now: f64) -> Value {
    let current = week_start(now);
    let mut monday = week_start(ts_of(NaiveDate::from_ymd_opt(year, 1, 1).unwrap_or_default()));
    if monday.year() < year {
        monday += Duration::weeks(1);
    }
    let mut mondays = Vec::new();
    while monday.year() == year && monday <= current {
        mondays.push(monday);
        monday += Duration::weeks(1);
    }
    let weeks = person_weeks(store, &me.workspaces, &me.label, &mondays, now);
    let mut total = Week::default();
    for w in weeks.values() {
        total.add(w);
    }
    let end = NaiveDate::from_ymd_opt(year, 12, 31).unwrap_or_default().min(day_of(now));
    let (_, best) = streaks(&total.days, end);
    let mut months = [0u64; 12];
    for day in &total.days {
        months[day.month0() as usize] += 1;
    }
    let busiest_month = months.iter().enumerate().max_by_key(|(i, n)| (**n, std::cmp::Reverse(*i))).filter(|(_, n)| **n > 0).map(|(i, _)| i + 1);
    let biggest = weeks.iter().max_by(|a, b| a.1.hours_s.total_cmp(&b.1.hours_s)).filter(|(_, w)| w.hours_s > 0.0);
    let mut repos: Vec<(&String, &u64)> = total.repos.iter().collect();
    repos.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    let mut last: Vec<Week> = mondays.iter().rev().filter(|m| **m < current).take(TYPE_WEEKS as usize).map(|m| weeks.get(m).cloned().unwrap_or_default()).collect();
    last.reverse();
    let kind = classify(&last).ok().map(type_json).unwrap_or(Value::Null);
    let earned: Vec<Value> = badges(&weeks, end, invited(store, &me.uid))
        .into_iter()
        .filter(|b| b["earned"] == json!(true))
        .collect();
    json!({
        "year": year,
        "person": {"name": me.name, "username": me.username},
        "weeks_with_data": weeks.values().filter(|w| !w.days.is_empty()).count(),
        "first_week": weeks.iter().find(|(_, w)| !w.days.is_empty()).map(|(m, _)| m.format("%Y-%m-%d").to_string()),
        "totals": numbers(&total),
        "sessions": total.sessions,
        "best_streak": best,
        "months": months,
        "busiest_month": busiest_month,
        "biggest_week": biggest.map(|(m, w)| json!({"week": week_label(*m), "start": m.format("%Y-%m-%d").to_string(), "hours": round(w.hours(), 1)})),
        "top_repos": repos.iter().take(3).map(|(r, n)| json!({"repo": r, "edits": n})).collect::<Vec<_>>(),
        "avg_at_once": round(total.avg_at_once(), 2),
        "share_two_plus": round(total.share_from(2), 3),
        "type": kind,
        "badges": earned,
    })
}

// ----------------------------------------------------------------- export

/// The signed report: the person's type and numbers, signed with the
/// server's key ([`crate::notify::sign`], the key that signs every email
/// link), so whoever is handed it can check it at `/api/habits/verify`.
pub fn export(store: &Store, me: &Me, now: f64) -> Value {
    let view = view(store, me, now);
    let issued = DateTime::<Utc>::from_timestamp(now as i64, 0).map(|d| d.to_rfc3339()).unwrap_or_default();
    let report = json!({
        "format": "cork.habits.v1",
        "issued_at": issued,
        "issuer": crate::notify::links().api,
        "subject": {"name": me.name, "username": me.username},
        "metadata_only": true,
        "window": view["window"],
        "type": view["type"].get("id").map(|_| json!({"id": view["type"]["id"], "name": view["type"]["name"]})).unwrap_or(Value::Null),
        "stats": view["card"],
        "signals": view["signals"],
        "streak": {"current": view["streak"]["current"], "best": view["streak"]["best"]},
        "all_time": view["all_time"],
        "badges": view["badges"].as_array().into_iter().flatten().filter(|b| b["earned"] == json!(true)).map(|b| b["id"].clone()).collect::<Vec<_>>(),
    });
    let signature = crate::notify::sign(store, &json!({"k": "report", "exp": now + REPORT_TTL_S, "r": report}));
    json!({"report": report, "signature": signature, "verify": format!("{}/api/habits/verify", crate::notify::links().api)})
}

/// Check a signed report: the signature must hold, and a report sent beside
/// it must be the one it signed, field for field.
pub fn verify(store: &Store, signature: &str, report: Option<&Value>, now: f64) -> Value {
    match crate::notify::verify(store, signature, "report", now) {
        Ok(payload) => {
            let signed = payload.get("r").cloned().unwrap_or(Value::Null);
            if report.is_some_and(|r| *r != signed) {
                return json!({"valid": false, "error": "the report was changed after it was signed"});
            }
            json!({"valid": true, "report": signed, "expires_at": payload.get("exp")})
        }
        Err(why) => json!({"valid": false, "error": why.replace("link", "signature")}),
    }
}

// ----------------------------------------------------------------- digest

fn tokens(n: i64) -> String {
    let a = n.unsigned_abs() as f64;
    let sign = if n < 0 { "-" } else { "" };
    if a >= 1e6 {
        format!("{sign}{:.1}M", a / 1e6)
    } else if a >= 1e3 {
        format!("{sign}{:.0}K", a / 1e3)
    } else {
        format!("{sign}{a}")
    }
}

fn delta(n: f64, show: &dyn Fn(f64) -> String) -> String {
    if n.abs() < 0.05 {
        "same as the week before".into()
    } else if n > 0.0 {
        format!("+{}", show(n))
    } else {
        format!("-{}", show(-n))
    }
}

/// The Monday digest's report card for one person: last week's type,
/// streak and numbers, each against the week before. None after a week
/// with no agent work.
pub fn digest_item(store: &Store, at: f64, uid: &str, label: &str, workspace: &str) -> Option<crate::notify::Item> {
    let me = Me::of(store, uid, label, "", "");
    let view = view(store, &me, at);
    if view.pointer("/week/last/days").and_then(Value::as_u64).unwrap_or(0) == 0 {
        return None;
    }
    let (last, moved) = (&view["week"]["last"], &view["week"]["change"]);
    let n = |v: &Value, k: &str| v.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    let whole = |x: f64| format!("{}", x.round() as i64);
    let type_line = match view["type"].get("name").and_then(Value::as_str) {
        Some(name) => format!("you are a {name}"),
        None => "your type is still forming".into(),
    };
    let streak = view["streak"]["current"].as_u64().unwrap_or(0);
    let text = format!(
        "Your report card for {}: {type_line}, on a {streak}-day streak. {} collisions caught ({}), {:.1} agent hours ({}), \
{} tokens saved ({}), {} handoffs ({}).",
        view["week"]["label"].as_str().unwrap_or(""),
        whole(n(last, "catches")), delta(n(moved, "catches"), &whole),
        n(last, "hours"), delta(n(moved, "hours"), &|x| format!("{x:.1}")),
        tokens(n(last, "tokens_saved") as i64), delta(n(moved, "tokens_saved"), &|x| tokens(x as i64)),
        whole(n(last, "handoffs")), delta(n(moved, "handoffs"), &whole),
    );
    Some(crate::notify::Item {
        key: format!("card:{uid}:{}", view["week"]["label"].as_str().unwrap_or("")),
        ttl_s: 8.0 * DAY_S,
        ts: at,
        ws: workspace.to_string(),
        subject: format!("Your Cork week: {type_line}"),
        text,
        link: crate::notify::links().page("/dashboard/you", workspace, &[]),
        action: "See your card".into(),
    })
}

// ----------------------------------------------------------------- routes

fn refuse(status: StatusCode, error: &str) -> (StatusCode, Json<Value>) {
    (status, Json(json!({"error": error})))
}

fn me_of(app: &App, headers: &HeaderMap, now: f64) -> Option<Me> {
    let principal = app.dashboard_auth.principal_of(&app.store, headers, "", now)?;
    Some(Me::from_principal(&app.store, &principal))
}

/// GET /api/me/habits: the caller's type, card, streak, badges and report card.
pub async fn api_habits(State(app): State<Arc<App>>, headers: HeaderMap) -> (StatusCode, Json<Value>) {
    let now = crate::store::now();
    let Some(me) = me_of(&app, &headers, now) else { return refuse(StatusCode::UNAUTHORIZED, "unauthorized") };
    let mut out = view(&app.store, &me, now);
    out["card_token"] = json!(card_token(&app.store, &out, now));
    (StatusCode::OK, Json(out))
}

/// GET /api/me/habits/public: whether the public profile is on, and its address.
pub async fn api_public_settings(State(app): State<Arc<App>>, headers: HeaderMap) -> (StatusCode, Json<Value>) {
    let now = crate::store::now();
    let Some(me) = me_of(&app, &headers, now) else { return refuse(StatusCode::UNAUTHORIZED, "unauthorized") };
    let (on, slug) = public_record(&app.store, &me.uid);
    (StatusCode::OK, Json(json!({"on": on, "slug": slug})))
}

/// PUT /api/me/habits/public {"on": bool}: the public profile, off by default.
pub async fn api_set_public(State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let now = crate::store::now();
    let Some(me) = me_of(&app, &headers, now) else { return refuse(StatusCode::UNAUTHORIZED, "unauthorized") };
    let Some(on) = body.get("on").and_then(Value::as_bool) else { return refuse(StatusCode::BAD_REQUEST, "send {\"on\": true} or {\"on\": false}") };
    (StatusCode::OK, Json(set_public(&app.store, &me, on, now)))
}

/// GET /api/me/habits/wrapped?year=2026: the year in review.
pub async fn api_wrapped(State(app): State<Arc<App>>, headers: HeaderMap, Query(query): Query<Value>) -> (StatusCode, Json<Value>) {
    let now = crate::store::now();
    let Some(me) = me_of(&app, &headers, now) else { return refuse(StatusCode::UNAUTHORIZED, "unauthorized") };
    let this_year = day_of(now).year();
    let year = query.get("year").and_then(Value::as_str).and_then(|y| y.trim().parse::<i32>().ok()).unwrap_or(this_year);
    if !(2024..=this_year).contains(&year) {
        return refuse(StatusCode::BAD_REQUEST, "no such year");
    }
    (StatusCode::OK, Json(wrapped(&app.store, &me, year, now)))
}

/// GET /api/me/habits/export: the signed report.
pub async fn api_export(State(app): State<Arc<App>>, headers: HeaderMap) -> (StatusCode, Json<Value>) {
    let now = crate::store::now();
    let Some(me) = me_of(&app, &headers, now) else { return refuse(StatusCode::UNAUTHORIZED, "unauthorized") };
    (StatusCode::OK, Json(export(&app.store, &me, now)))
}

/// GET /api/workspaces/{id}/leaderboard: members only.
pub async fn api_leaderboard(
    State(app): State<Arc<App>>, Path(workspace_id): Path<String>, headers: HeaderMap,
) -> (StatusCode, Json<Value>) {
    let now = crate::store::now();
    let principal = app.dashboard_auth.principal_of(&app.store, &headers, "", now);
    let auth = match crate::auth::workspace_auth(&app.store, principal, &workspace_id) {
        Ok(auth) => auth,
        Err((status, body)) => return (StatusCode::from_u16(status).unwrap(), Json(body)),
    };
    let mut out = leaderboard(&app.store, &auth.workspace, now);
    out["workspace"] = json!(auth.workspace);
    (StatusCode::OK, Json(out))
}

/// GET /api/public/habits/{slug}: a public profile, no sign-in; 404 unless
/// its owner turned it on.
pub async fn api_public(State(app): State<Arc<App>>, Path(slug): Path<String>) -> (StatusCode, Json<Value>) {
    let slug = slug.trim().to_lowercase();
    match public_profile(&app.store, &slug, crate::store::now()) {
        Some(profile) => (StatusCode::OK, Json(profile)),
        None => refuse(StatusCode::NOT_FOUND, "no public profile here"),
    }
}

/// GET /api/habits/card?t=…: what a card token says, for the site's picture of it.
pub async fn api_card(State(app): State<Arc<App>>, Query(query): Query<Value>) -> (StatusCode, Json<Value>) {
    let token = query.get("t").and_then(Value::as_str).unwrap_or("");
    match crate::notify::verify(&app.store, token, "card", crate::store::now()) {
        Ok(payload) => (StatusCode::OK, Json(json!({"card": payload.get("c").cloned().unwrap_or(Value::Null)}))),
        Err(why) => refuse(StatusCode::NOT_FOUND, why),
    }
}

/// POST /api/habits/verify {"report", "signature"} (or the export as it
/// was downloaded): whether Cork signed it. No sign-in: for whoever was
/// handed it.
pub async fn api_verify(State(app): State<Arc<App>>, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let signature = text(&body, "signature");
    if signature.is_empty() {
        return refuse(StatusCode::BAD_REQUEST, "send the export: {\"report\": …, \"signature\": \"…\"}");
    }
    (StatusCode::OK, Json(verify(&app.store, signature, body.get("report"), crate::store::now())))
}

/// GET /api/habits/verify?signature=…: the same, for a signature alone.
pub async fn api_verify_get(State(app): State<Arc<App>>, Query(query): Query<Value>) -> (StatusCode, Json<Value>) {
    let signature = query.get("signature").and_then(Value::as_str).unwrap_or("");
    if signature.is_empty() {
        return refuse(StatusCode::BAD_REQUEST, "pass ?signature=");
    }
    (StatusCode::OK, Json(verify(&app.store, signature, None, crate::store::now())))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tuesday 2025-10-14 12:00 UTC: week 2025-W42 starts Monday 10-13
    const NOW: f64 = 1_760_443_200.0;
    const HOUR: f64 = 3600.0;

    fn row(kind: &str, ts: f64, payload: Value) -> (String, LedgerRow) {
        ("org/app".to_string(), LedgerRow { seq: 0, kind: kind.to_string(), payload, ts })
    }

    fn monday() -> f64 {
        ts_of(week_start(NOW))
    }

    fn edit(user: &str, session: &str, ts: f64) -> (String, LedgerRow) {
        row("edit_reported", ts, json!({"user": user, "session": session, "path": "a.py"}))
    }

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn two_sessions_at_once_are_two_agents_and_a_subagent_is_not_a_third() {
        let m = monday();
        let rows = vec![
            edit("a@x", "s1", m + HOUR),
            edit("a@x", "s1", m + HOUR + 240.0),
            // s2 overlaps s1 for a while; s1.ingest is s1's own side session
            edit("a@x", "s2", m + HOUR + 300.0),
            edit("a@x", "s1.ingest", m + HOUR + 310.0),
            row("turn_usage", m + HOUR + 400.0, json!({"user": "a@x", "session": "s1", "worker": "w1", "active_s": 1800.0})),
            // a second day, one agent
            edit("a@x", "s3", m + DAY_S + HOUR),
        ];
        let people = tally(&rows, m, m + 7.0 * DAY_S);
        let a = &people["a@x"];
        assert_eq!(a.sessions, 3);
        assert_eq!(a.peak, 2, "{:?}", a.level_s);
        assert!(a.level_s[1] > 0.0 && a.level_s[2] == 0.0);
        assert_eq!(a.days.len(), 2);
        assert_eq!(a.edits, 5);
        assert!((a.hours() - 0.5).abs() < 1e-9);
        assert!(a.share_from(2) > 0.0 && a.share_from(2) < 1.0);
    }

    #[test]
    fn messages_count_sends_and_handoffs_count_work_routed() {
        let m = monday();
        let rows = vec![
            row("agent_message", m + 10.0, json!({"from": "a@x", "to": "b@x", "id": "1"})),
            row("agent_message", m + 20.0, json!({"from": "a@x", "to": "b@x", "id": "2", "handoff": true})),
            // one anchored send to two people writes two rows
            row("agent_message", m + 30.0, json!({"from": "a@x#s1/lead", "to": "b@x", "id": "3", "anchor": "a.py::f"})),
            row("agent_message", m + 30.0, json!({"from": "a@x#s1/lead", "to": "c@x", "id": "4", "anchor": "a.py::f"})),
        ];
        let a = &tally(&rows, m, m + 7.0 * DAY_S)["a@x"];
        assert_eq!((a.messages, a.handoffs), (3, 2));
    }

    #[test]
    fn collisions_outcomes_savings_and_a_crowded_hour() {
        let m = monday();
        let rows = vec![
            // one agent blocked twice on one path within ten minutes: one collision
            row("gate_block", m + 100.0, json!({"user": "a@x", "session": "s1", "path": "calc.py"})),
            row("gate_block", m + 200.0, json!({"user": "a@x", "session": "s1", "path": "calc.py"})),
            row("collision_prevented", m + 5000.0, json!({"user": "a@x", "session": "s1", "paths": ["tax.py"], "tokens_saved": 1000})),
            row("block_outcome", m + 300.0, json!({"kind": "teammate_change", "outcome": "adapted", "user": "a@x", "session": "s1"})),
            row("block_outcome", m + 400.0, json!({"kind": "decision_hold", "outcome": "overrode", "user": "a@x", "session": "s1"})),
            edit("a@x", "s1", m + 3.0 * HOUR + 10.0),
            edit("b@x", "s9", m + 3.0 * HOUR + 20.0),
            edit("c@x", "s7", m + 3.0 * HOUR + 30.0),
        ];
        let people = tally(&rows, m, m + 7.0 * DAY_S);
        let a = &people["a@x"];
        assert_eq!(a.catches, 2);
        assert_eq!((a.adapted, a.overrode), (1, 0), "a decision hold is not a collision");
        assert_eq!(a.tokens_saved, 1000);
        assert_eq!((a.crowded, people["b@x"].crowded), (1, 1));
    }

    #[test]
    fn a_week_survives_being_kept() {
        let m = monday();
        let rows = vec![edit("a@x", "s1", m + 10.0), edit("a@x", "s2", m + 20.0)];
        let week = tally(&rows, m, m + 7.0 * DAY_S)["a@x"].clone();
        assert_eq!(Week::from_json(&week.to_json()), week);
    }

    fn week(days: &[&str], two: f64, three: f64, f: impl Fn(&mut Week)) -> Week {
        let mut w = Week { days: days.iter().map(|d| day(d)).collect(), ..Week::default() };
        // 10 hours at work: the shares asked for at 2 and 3+ at once
        w.level_s[0] = 36_000.0 * (1.0 - two);
        w.level_s[1] = 36_000.0 * (two - three);
        w.level_s[2] = 36_000.0 * three;
        w.peak = if three > 0.0 { 3 } else if two > 0.0 { 2 } else { 1 };
        w.hours_s = 36_000.0;
        f(&mut w);
        w
    }

    fn type_of(weeks: &[Week]) -> &'static str {
        classify(weeks).map(|t| t.id).unwrap_or("none")
    }

    #[test]
    fn a_type_needs_six_days_across_two_weeks() {
        let busy = week(&["2025-09-15", "2025-09-16", "2025-09-17", "2025-09-18", "2025-09-19", "2025-09-20"], 0.0, 0.0, |_| {});
        assert_eq!(classify(&[busy.clone(), Week::default(), Week::default(), Week::default()]).err(), Some((6, 1)));
        let other = week(&["2025-09-22"], 0.0, 0.0, |_| {});
        assert_eq!(type_of(&[busy, other, Week::default(), Week::default()]), "solo_act");
    }

    #[test]
    fn each_rule_in_its_order() {
        let a = |two, three, f: &dyn Fn(&mut Week)| {
            vec![
                week(&["2025-09-15", "2025-09-16", "2025-09-17"], two, three, f),
                week(&["2025-09-22", "2025-09-23", "2025-09-24"], two, three, f),
                week(&["2025-09-29"], two, three, f),
                week(&["2025-10-06"], two, three, f),
            ]
        };
        assert_eq!(type_of(&a(0.1, 0.0, &|_| {})), "solo_act");
        assert_eq!(type_of(&a(0.5, 0.0, &|_| {})), "duet");
        assert_eq!(type_of(&a(0.5, 0.0, &|w| w.messages = 3)), "conductor", "12 messages in four weeks");
        assert_eq!(type_of(&a(0.3, 0.25, &|w| w.catches = 1)), "swarm");
        assert_eq!(type_of(&a(0.1, 0.0, &|w| w.handoffs = 1)), "relay");
        assert_eq!(type_of(&a(0.25, 0.0, &|_| {})), "all_rounder");
        // 3 collisions a week, 12 over 8 active days; 4 of 16 settled gave way
        let cowboy = a(0.5, 0.0, &|w| {
            w.catches = 3;
            w.adapted = 1;
            w.overrode = 3;
            w.messages = 5;
        });
        assert_eq!(type_of(&cowboy), "cowboy", "before conductor");
        let neighbor = a(0.1, 0.0, &|w| {
            w.catches = 2;
            w.adapted = 2;
        });
        assert_eq!(type_of(&neighbor), "good_neighbor");
        // every week, 40 hours, a quarter three at once, no collisions
        assert_eq!(type_of(&a(0.35, 0.25, &|_| {})), "clean_sheet");
        let atc = a(0.6, 0.3, &|w| {
            w.peak = 5;
            w.messages = 6;
            w.catches = 1;
            w.adapted = 3;
        });
        assert_eq!(type_of(&atc), "air_traffic_control");
        let mut lapsed = a(0.35, 0.25, &|_| {});
        lapsed[2] = Week::default();
        lapsed[0].days.insert(day("2025-09-18"));
        lapsed[0].hours_s += 36_000.0;
        assert_eq!(type_of(&lapsed), "swarm", "a clean sheet needs every week");
    }

    #[test]
    fn weekends_neither_count_nor_break_a_streak_and_today_is_not_over() {
        // Mon 2025-10-06 .. Fri 10-10 worked, the weekend off, today Mon 10-13 not yet
        let days: BTreeSet<NaiveDate> = ["2025-10-06", "2025-10-07", "2025-10-08", "2025-10-09", "2025-10-10"].iter().map(|d| day(d)).collect();
        assert_eq!(streaks(&days, day("2025-10-13")), (5, 5));
        // Tuesday passes without work: the streak is over
        assert_eq!(streaks(&days, day("2025-10-14")), (0, 5));
        let mut sunday = days.clone();
        sunday.insert(day("2025-10-12"));
        sunday.insert(day("2025-10-13"));
        assert_eq!(streaks(&sunday, day("2025-10-13")), (7, 7), "a weekend worked counts");
        let mut gap = days.clone();
        gap.remove(&day("2025-10-08"));
        assert_eq!(streaks(&gap, day("2025-10-10")), (2, 2));
    }

    #[test]
    fn badges_carry_the_week_earned_and_the_invite_badge_waits_for_an_invite() {
        let m1 = day("2025-09-29");
        let m2 = day("2025-10-06");
        let mut weeks = BTreeMap::new();
        weeks.insert(m1, Week { catches: 1, crowded: 1, days: [day("2025-09-29")].into(), ..Week::default() });
        weeks.insert(m2, Week {
            catches: 9, peak: 3, handoffs: 1,
            days: ["2025-10-06", "2025-10-07", "2025-10-08", "2025-10-09", "2025-10-10"].iter().map(|d| day(d)).collect(),
            ..Week::default()
        });
        let got = |invited: bool| -> BTreeMap<String, Value> {
            badges(&weeks, day("2025-10-13"), invited).into_iter().map(|b| (b["id"].as_str().unwrap().to_string(), b)).collect()
        };
        let b = got(false);
        assert_eq!(b["first_catch"]["week"], "2025-W40");
        assert_eq!(b["catches_10"]["week"], "2025-W41");
        assert_eq!(b["three_at_once"]["earned"], true);
        assert_eq!(b["first_handoff"]["earned"], true);
        assert_eq!(b["full_week"]["week"], "2025-W41");
        assert_eq!(b["catches_100"]["earned"], false);
        assert_eq!((b["full_house"]["earned"].clone(), b["full_house"]["locked"].clone()), (json!(false), json!(true)));
        let b = got(true);
        assert_eq!((b["full_house"]["earned"].clone(), b["full_house"]["week"].clone()), (json!(true), json!("2025-W40")));
    }

    #[test]
    fn a_kept_week_is_read_back_and_the_report_signature_holds_only_unchanged() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let now = crate::store::now();
        let me = Me { uid: "u1".into(), label: "a@x".into(), name: "Ada".into(), username: String::new(), avatar: String::new(), workspaces: vec![] };
        let signed = export(&store, &me, now);
        let ok = verify(&store, signed["signature"].as_str().unwrap(), Some(&signed["report"]), now);
        assert_eq!(ok["valid"], true, "{ok}");
        let mut forged = signed["report"].clone();
        forged["stats"]["catches"] = json!(999);
        assert_eq!(verify(&store, signed["signature"].as_str().unwrap(), Some(&forged), now)["valid"], false);
        assert_eq!(verify(&store, "nope.nope", None, now)["valid"], false);
        // the public profile is off until turned on, and keeps its address
        assert!(public_profile(&store, "anything", now).is_none());
        let on = set_public(&store, &me, true, now);
        let slug = on["slug"].as_str().unwrap().to_string();
        assert!(public_profile(&store, &slug, now).is_some());
        let off = set_public(&store, &me, false, now);
        assert_eq!(off["slug"], json!(slug));
        assert!(public_profile(&store, &slug, now).is_none());
    }

    // the routes, end to end through their handlers
    mod route {
        use super::*;

        const WS: &str = "wsh";

        fn app() -> Arc<App> {
            let store = Arc::new(Store::open(std::path::Path::new(":memory:")).unwrap());
            store.kv_put(crate::billing::BILLING_BUCKET, WS, &json!({"plan": "team", "seats": 5, "status": "active"}), 1.0).unwrap();
            store.kv_put("ws", WS, &json!({"id": WS, "name": "Acme", "owner": "alice"}), 0.0).unwrap();
            for (uid, role) in [("alice", "owner"), ("bob", "member")] {
                store.kv_put("member", &format!("{WS}/{uid}"), &json!({
                    "uid": uid, "email": format!("{uid}@x.dev"), "role": role, "access": "write", "repos": "*", "added": 0.0,
                }), 0.0).unwrap();
                store.kv_put("uws", &format!("{uid}/{WS}"), &json!({"workspace": WS}), 0.0).unwrap();
            }
            let protocol = crate::protocol_ws::Hub::open(Arc::clone(&store), "");
            Arc::new(App {
                store,
                aliases: crate::repo::Aliases::new(),
                started: std::time::Instant::now(),
                dashboard_url: String::new(),
                public_url: String::new(),
                mcp_url: String::new(),
                dashboard_auth: crate::auth::DashboardAuth { test_mode: true, firebase_project_id: String::new(), firebase_emulator: false },
                hot_ttl_s: 900.0,
                idle_after_s: 600.0,
                intent_ttl_s: 900.0,
                protocol,
            })
        }

        fn bearer(token: &str) -> HeaderMap {
            let mut headers = HeaderMap::new();
            headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
            headers
        }

        async fn board(app: &Arc<App>, token: &str) -> (StatusCode, Value) {
            let (status, Json(body)) = api_leaderboard(State(Arc::clone(app)), Path(WS.to_string()), bearer(token)).await;
            (status, body)
        }

        #[tokio::test]
        async fn the_leaderboard_opens_on_an_invite_and_ranks_only_what_is_above_zero() {
            let app = app();
            let repo = "github.com/acme/api";
            app.store.kv_put("ghrepo", &format!("{WS}:{repo}"), &json!({"repo_id": repo}), 0.0).unwrap();
            let scope = app.aliases.scope_for(WS, repo);
            // last full week: bob's agent is stopped twice, on two files
            let t = ts_of(week_start(crate::store::now()) - Duration::days(3));
            app.store.ledger_append(&scope, "gate_block", &json!({"user": "bob@x.dev", "session": "s1", "path": "x.py"}), t).unwrap();
            app.store.ledger_append(&scope, "gate_block", &json!({"user": "bob@x.dev", "session": "s1", "path": "y.py"}), t + 5.0).unwrap();
            app.store.ledger_append(&scope, "edit_reported", &json!({"user": "alice@x.dev", "session": "a1", "path": "z.py"}), t).unwrap();

            let (status, body) = board(&app, "test:bob").await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["locked"], true, "nobody has joined on an invite yet");

            app.store.kv_put("member", &format!("{WS}/bob"), &json!({
                "uid": "bob", "email": "bob@x.dev", "role": "member", "access": "write", "repos": "*", "added": 0.0, "added_by": "alice",
            }), 0.0).unwrap();
            let (_, body) = board(&app, "test:bob").await;
            assert_eq!(body["locked"], false, "{body}");
            let catches = body["boards"]["catches"].as_array().unwrap();
            assert_eq!(catches.len(), 1, "alice has none, so she is not ranked: {body}");
            assert_eq!((catches[0]["who"]["uid"].clone(), catches[0]["value"].clone()), (json!("bob"), json!(2)));
            assert!(body.to_string().find("\"type\"").is_none(), "never a teammate's type");
            // the week was kept
            assert!(app.store.kv_get(WEEK_BUCKET, &week_key(WS, week_start(t))).is_some());

            assert_eq!(board(&app, "test:mallory").await.0, StatusCode::FORBIDDEN);
            let (status, Json(mine)) = api_habits(State(Arc::clone(&app)), bearer("test:alice")).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(mine["public"]["on"], false, "off by default");
            assert_eq!(mine["invited"], true, "bob joined on her invite");
            assert!(mine["card_token"].as_str().is_some_and(|t| !t.is_empty()));
            let (status, _) = api_habits(State(Arc::clone(&app)), HeaderMap::new()).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }
    }
}
