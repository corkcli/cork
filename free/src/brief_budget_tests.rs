//! What the briefings cost the agent, measured on one fixture repo: the
//! session start and a prompt briefing, in characters and in tokens (chars
//! over four, the carried-cost method the studies use), held to a budget.
//! Every byte here rides in every later message of the session.

use serde_json::{json, Value};

use crate::store::{now, Store};

pub(crate) const SCOPE: &str = "w:lanternly";

/// A small booking app: money helpers, pricing on top of them, invoices and
/// an API on top of pricing, a report nobody calls, and tests.
pub(crate) const FILES: [(&str, &str); 12] = [
    ("lanternly/utils/money.py", "\"\"\"Money helpers: integer cents everywhere, rounding half up.\"\"\"\n\n\ndef pct_of(cents: int, pct: float) -> int:\n    \"\"\"pct percent of cents, rounded half up to a whole cent.\"\"\"\n    return int(cents * pct / 100 + 0.5)\n\n\ndef fmt_cents(cents: int, currency: str = 'USD') -> str:\n    \"\"\"Cents as a display string, with the currency's symbol.\"\"\"\n    return f'${cents / 100:.2f}'\n\n\ndef parse_cents(text: str) -> int:\n    \"\"\"A display string back to cents.\"\"\"\n    return int(float(text.strip('$')) * 100)\n"),
    ("lanternly/billing/pricing.py", "\"\"\"What a booking costs: the space's rate, the member's plan discount, and fees.\"\"\"\nfrom lanternly.utils.money import pct_of\n\n\ndef price_for(booking, member, space) -> int:\n    \"\"\"Cents the member pays for this booking.\"\"\"\n    base = space.rate * booking.hours\n    return base - pct_of(base, member.discount)\n\n\ndef card_fee(cents: int) -> int:\n    \"\"\"The card processor's fee on a charge, in cents.\"\"\"\n    return pct_of(cents, 2.9) + 30\n\n\ndef peak_multiplier(hour: int) -> float:\n    \"\"\"How much more an hour costs at peak.\"\"\"\n    return 1.5 if 9 <= hour < 17 else 1.0\n"),
    ("lanternly/billing/invoices.py", "\"\"\"Invoices: one per member per month.\"\"\"\nfrom lanternly.billing.pricing import price_for, card_fee\n\n\ndef subtotal(bookings, member, spaces) -> int:\n    \"\"\"Sum of the month's bookings, in cents.\"\"\"\n    return sum(price_for(b, member, spaces[b.space_id]) for b in bookings)\n\n\ndef total_due(bookings, member, spaces) -> int:\n    \"\"\"Subtotal plus the card fee.\"\"\"\n    s = subtotal(bookings, member, spaces)\n    return s + card_fee(s)\n"),
    ("lanternly/api/bookings.py", "\"\"\"The bookings API: create, list, cancel.\"\"\"\nfrom lanternly.billing.pricing import price_for\n\n\ndef create_booking(store, member, space, hours: int):\n    \"\"\"Books the space and charges the member.\"\"\"\n    return price_for(None, member, space)\n\n\ndef list_bookings(store, member_id: int | None = None) -> list:\n    \"\"\"A member's bookings, newest first.\"\"\"\n    return []\n\n\ndef cancel_booking(store, booking_id: int) -> bool:\n    \"\"\"Cancels a booking; refunds are a separate step.\"\"\"\n    return True\n"),
    ("lanternly/api/router.py", "\"\"\"Routes requests to handlers.\"\"\"\nfrom lanternly.api.bookings import create_booking, list_bookings\n\n\ndef handle(request):\n    \"\"\"Dispatch one request.\"\"\"\n    if request.method == 'POST':\n        return create_booking(None, None, None, 1)\n    return list_bookings(None)\n"),
    ("lanternly/core/models.py", "\"\"\"The data model.\"\"\"\n\n\nclass Member:\n    \"\"\"A paying member, with a plan discount in percent.\"\"\"\n    pass\n\n\nclass Space:\n    \"\"\"A bookable room, with an hourly rate in cents.\"\"\"\n    pass\n\n\nclass Booking:\n    \"\"\"A member's hours in a space.\"\"\"\n    pass\n"),
    ("lanternly/core/db.py", "\"\"\"Storage.\"\"\"\n\n\ndef connect(url: str):\n    \"\"\"A connection to the database at url.\"\"\"\n    return None\n\n\ndef migrate(conn) -> int:\n    \"\"\"Runs pending migrations; returns how many ran.\"\"\"\n    return 0\n"),
    ("lanternly/reports/occupancy.py", "\"\"\"Occupancy by space and day.\"\"\"\n\n\ndef occupancy(bookings, day) -> dict:\n    \"\"\"Hours booked per space on a day.\"\"\"\n    return {}\n"),
    ("lanternly/reports/credits.py", "\"\"\"Credits issued and used.\"\"\"\n\n\ndef credits_report(credits) -> list:\n    \"\"\"One row per credit.\"\"\"\n    return []\n"),
    ("lanternly/admin/views.py", "\"\"\"Admin screens.\"\"\"\nfrom lanternly.utils.money import fmt_cents\n\n\ndef render_invoice(invoice) -> str:\n    \"\"\"An invoice as HTML.\"\"\"\n    return fmt_cents(invoice.total)\n"),
    ("tests/test_pricing.py", "from lanternly.billing.pricing import price_for, card_fee\n\n\ndef test_price_for():\n    assert price_for\n\n\ndef test_card_fee():\n    assert card_fee(100) == 33\n"),
    ("tests/test_api.py", "from lanternly.api.bookings import create_booking\n\n\ndef test_create_booking():\n    assert create_booking\n"),
];

pub(crate) fn fixture() -> Store {
    let store = Store::open(std::path::Path::new(":memory:")).unwrap();
    let files: Vec<(&str, &str, Option<&Value>)> = FILES.iter().map(|(p, c)| (*p, *c, None)).collect();
    crate::report::observe_batch(&store, SCOPE, &files, 0.0);
    // the week's writes, by two teammates: one signature change with its
    // reason, the rest body-only
    let stamp = now();
    let writes: [(&str, &str, f64, Value, &str); 6] = [
        ("lanternly/billing/pricing.py", "becca@x.com", 300.0,
         json!([{"symbol": "price_for", "kind": "modified", "signature_before": "def price_for(booking, member) -> int",
                 "signature_after": "def price_for(booking, member, space) -> int"}]),
         "price depends on the space's rate now"),
        ("lanternly/utils/money.py", "becca@x.com", 900.0, json!([]), ""),
        ("lanternly/billing/invoices.py", "dan@x.com", 1_800.0, json!([]), ""),
        ("lanternly/api/bookings.py", "dan@x.com", 3_600.0, json!([]), ""),
        ("lanternly/core/models.py", "dan@x.com", 7_200.0, json!([]), ""),
        ("lanternly/reports/occupancy.py", "becca@x.com", 86_400.0, json!([]), ""),
    ];
    for (path, user, ago, events, why) in writes {
        let mut payload = json!({"path": path, "user": user, "agent": "claude-code", "session": format!("s-{user}"), "events": events});
        if !why.is_empty() {
            payload["why"] = json!(why);
        }
        store.ledger_append(SCOPE, "edit_reported", &payload, stamp - ago).unwrap();
    }
    store
}

pub(crate) fn input<'a>(
    identifiers: &'a [String], session: &'a str, visible: &'a std::collections::BTreeSet<String>,
) -> crate::brief::BriefInput<'a> {
    crate::brief::BriefInput {
        repo_id: "github.com/acme/lanternly", since: 0.0, budget: 0, identifiers, exclude: &[], session, resume: false,
        idle_after_s: 300.0, visible, viewer: "kevin@x.com", share_knowledge: true, verified: true,
    }
}

/// (chars, tokens) of a briefing's text, tokens as chars over four.
pub(crate) fn size(answer: &Value) -> (usize, usize) {
    let chars = answer.get("text").and_then(Value::as_str).unwrap_or("").chars().count();
    (chars, chars.div_ceil(4))
}

/// The prompt-scoped budget: the hook's own (prompt.rs, PROMPT_BRIEF_BUDGET, 3,500 before this change).
const PROMPT_BUDGET: usize = 3_500;

pub(crate) fn session_start(store: &Store, session: &str) -> Value {
    let visible = std::collections::BTreeSet::new();
    let mut i = input(&[], session, &visible);
    i.budget = 6_000;
    crate::brief::brief(store, SCOPE, &i)
}

pub(crate) fn prompt(store: &Store, session: &str, identifiers: &[&str]) -> Value {
    let visible = std::collections::BTreeSet::new();
    let ids: Vec<String> = identifiers.iter().map(|s| s.to_string()).collect();
    let mut i = input(&ids, session, &visible);
    i.budget = PROMPT_BUDGET;
    crate::brief::brief(store, SCOPE, &i)
}

#[test]
fn the_briefings_stay_inside_their_budget() {
    let store = fixture();
    let start = session_start(&store, "s1");
    let (start_chars, start_tokens) = size(&start);
    let first = prompt(&store, "s1", &["price_for", "lanternly/billing/pricing.py", "pct_of", "reports"]);
    let (first_chars, first_tokens) = size(&first);
    let second = prompt(&store, "s1", &["occupancy", "credits_report"]);
    let (second_chars, second_tokens) = size(&second);
    println!(
        "fixture briefing sizes: session start {start_chars} chars / {start_tokens} tokens; \
first prompt {first_chars} chars / {first_tokens} tokens; second prompt {second_chars} chars / {second_tokens} tokens"
    );
    for (name, answer) in [("session start", &start), ("first prompt", &first), ("second prompt", &second)] {
        println!("----- {name}\n{}", answer["text"].as_str().unwrap_or(""));
    }
    // measured before the trim (0259afe, the hook's 3,500 prompt budget):
    // session start 3,640 chars / 910 tokens, first prompt 2,986 / 747,
    // second 910 / 228. After: 2,856 / 714, 2,264 / 566, 524 / 131. Every
    // symbol's line range, the complete · unchanged marks and the sentence on
    // ranged reads (token-slope) cost 3,252 / 813, 2,478 / 620, 576 / 144:
    // carried on every message, against whole files not read.
    assert!(start_tokens <= 830, "session start {start_tokens} tokens\n{}", start["text"]);
    assert!(first_tokens <= 640, "first prompt {first_tokens} tokens\n{}", first["text"]);
    assert!(second_tokens <= 150, "second prompt {second_tokens} tokens\n{}", second["text"]);
}

/// A bigger repo, the scaling study's size class: eight packages of six
/// modules, each calling the shared helpers and its neighbour, twenty of them
/// written this week by two teammates.
pub(crate) fn big_fixture() -> Store {
    let store = Store::open(std::path::Path::new(":memory:")).unwrap();
    let dirs = ["billing", "api", "admin", "reports", "worker", "core", "members", "spaces"];
    let mut files: Vec<(String, String)> = vec![(
        "app/utils/helpers.py".to_string(),
        "\"\"\"Shared helpers.\"\"\"\n\n\ndef to_cents(amount: float) -> int:\n    \"\"\"Dollars to integer cents, rounded half up.\"\"\"\n    return int(amount * 100 + 0.5)\n\n\ndef fmt_money(cents: int) -> str:\n    \"\"\"Cents as a display string.\"\"\"\n    return str(cents)\n".to_string(),
    )];
    for dir in dirs {
        for i in 0..6 {
            let prev = if i == 0 { String::new() } else { format!("from app.{dir}.m{} import run_{dir}_{}\n", i - 1, i - 1) };
            let call_prev = if i == 0 { String::new() } else { format!("    run_{dir}_{}(x)\n", i - 1) };
            let content = format!(
                "\"\"\"The {dir} step {i}: what it does for the {dir} flow.\"\"\"\nfrom app.utils.helpers import to_cents, fmt_money\n{prev}\n\n\
def run_{dir}_{i}(x: int) -> int:\n    \"\"\"Runs {dir} step {i} on x and returns the new total in cents.\"\"\"\n{call_prev}    return to_cents(x)\n\n\n\
def show_{dir}_{i}(x: int) -> str:\n    \"\"\"The {dir} step {i} total as a display string.\"\"\"\n    return fmt_money(x)\n\n\n\
def check_{dir}_{i}(x: int) -> bool:\n    \"\"\"Whether x is a valid input for {dir} step {i}.\"\"\"\n    return x >= 0\n"
            );
            files.push((format!("app/{dir}/m{i}.py"), content));
        }
        files.push((format!("tests/test_{dir}.py"), format!("from app.{dir}.m0 import run_{dir}_0\n\n\ndef test_{dir}():\n    assert run_{dir}_0(1)\n")));
    }
    let batch: Vec<(&str, &str, Option<&Value>)> = files.iter().map(|(p, c)| (p.as_str(), c.as_str(), None)).collect();
    crate::report::observe_batch(&store, SCOPE, &batch, 0.0);
    let stamp = now();
    for (n, (path, _)) in files.iter().filter(|(p, _)| p.starts_with("app/")).take(20).enumerate() {
        let user = if n % 2 == 0 { "becca@x.com" } else { "dan@x.com" };
        let payload = json!({"path": path, "user": user, "agent": "claude-code", "session": format!("s-{user}"), "events": []});
        store.ledger_append(SCOPE, "edit_reported", &payload, stamp - 600.0 * (n as f64 + 1.0)).unwrap();
    }
    store
}

#[test]
fn the_briefings_of_a_bigger_repo_stay_inside_their_budget() {
    let store = big_fixture();
    let start = session_start(&store, "s1");
    let (start_chars, start_tokens) = size(&start);
    let shape_chars = start["text"].as_str().unwrap_or("").split("Cork briefing").next().unwrap_or("").chars().count();
    let first = prompt(&store, "s1", &["to_cents", "app/billing/m2.py"]);
    let (first_chars, first_tokens) = size(&first);
    println!(
        "big fixture briefing sizes: session start {start_chars} chars / {start_tokens} tokens (repo shape {shape_chars} chars); \
first prompt {first_chars} chars / {first_tokens} tokens"
    );
    println!("----- big session start\n{}\n----- big first prompt\n{}", start["text"].as_str().unwrap_or(""), first["text"].as_str().unwrap_or(""));
    // before the trim (0259afe): session start 6,425 chars / 1,607 tokens
    // (repo shape 1,059 chars), first prompt 3,939 / 985. After: 4,856 /
    // 1,214 (shape 504), 2,789 / 698. With line ranges and trust marks
    // (token-slope): 5,133 / 1,284, 2,666 / 667.
    assert!(shape_chars <= 900, "repo shape {shape_chars} chars");
    assert!(start_tokens <= 1_300, "session start {start_tokens} tokens");
    assert!(first_tokens <= 720, "first prompt {first_tokens} tokens");
}

#[test]
fn outcomes_change_the_next_briefing() {
    let store = fixture();
    let before = prompt(&store, "s0", &["price_for"]);
    let text = before["text"].as_str().unwrap_or("");
    // a one-file task: the module it names, then what calls it, by its callers
    assert!(text.contains("lanternly/billing/invoices.py — calls price_for; dan@x.com, 30m ago\n  def subtotal("), "{text}");
    assert!(text.contains("lanternly/api/bookings.py"), "shown or named: {text}");
    // every symbol carries its line range, to Read instead of the file
    assert!(text.contains("def price_for(booking, member, space) -> int [5-8] — "), "{text}");
    // three sessions left bookings.py unopened after being briefed it, three
    // read invoices.py anyway, two went looking for credits.py after
    // prompts naming peak_multiplier, and occupancy.py was never opened
    for s in ["a", "b", "c"] {
        crate::brieflearn::record(&store, SCOPE, s, &json!({
            "unused": ["lanternly/api/bookings.py", "lanternly/reports/occupancy.py"],
            "needed": ["lanternly/billing/invoices.py"],
        }));
    }
    for s in ["d", "e"] {
        crate::brieflearn::note_names(&store, SCOPE, s, &["peak_multiplier".to_string()]);
        crate::brieflearn::record(&store, SCOPE, s, &json!({"sought": ["lanternly/reports/credits.py"]}));
    }
    let after = prompt(&store, "s1", &["price_for"]);
    let text = after["text"].as_str().unwrap_or("");
    // the dependent nobody opens is named, not shown
    assert!(!text.contains("lanternly/api/bookings.py — "), "{text}");
    assert!(text.contains("also related, by name only (get_symbol for one): lanternly/api/bookings.py"), "{text}");
    assert_eq!(after["learned"]["dropped"], json!(["lanternly/api/bookings.py"]));
    // the one everybody reads anyway comes whole, with where each symbol is
    assert!(text.contains("def subtotal(bookings, member, spaces) -> int [5-7] — "), "{text}");
    assert_eq!(after["learned"]["full"], json!(["lanternly/billing/invoices.py"]));
    // a prompt that names the ignored module still gets it, short
    let named = prompt(&store, "s2", &["lanternly/api/bookings.py"]);
    let text = named["text"].as_str().unwrap_or("");
    assert!(text.contains("lanternly/api/bookings.py — dan@x.com"), "{text}");
    assert!(!text.contains("def create_booking"), "{text}");
    assert_eq!(named["learned"]["short"], json!(["lanternly/api/bookings.py"]));
    // what earlier sessions went looking for is briefed to the next prompt naming it
    let sought = prompt(&store, "s3", &["peak_multiplier"]);
    let text = sought["text"].as_str().unwrap_or("");
    assert!(text.contains("lanternly/reports/credits.py — matches sought for peak_multiplier before"), "{text}");
    assert_eq!(sought["learned"]["added"], json!(["lanternly/reports/credits.py"]));
    // and the session start shortens the module nobody opens
    let start = session_start(&store, "s4");
    let text = start["text"].as_str().unwrap_or("");
    assert!(text.contains("lanternly/reports/occupancy.py — becca@x.com, 24h ago") && !text.contains("def occupancy("), "{text}");
}

#[test]
fn the_trimmed_briefing_keeps_every_fact_it_carried() {
    let store = fixture();
    let start = session_start(&store, "s1");
    let text = start["text"].as_str().unwrap_or("");
    // the shape, the newest change with its reason, the exact signatures
    assert!(text.contains("Repo shape"), "{text}");
    assert!(text.contains("changed: def price_for(booking, member) -> int → def price_for(booking, member, space) -> int"), "{text}");
    assert!(text.contains("why (becca@x.com): price depends on the space's rate now"), "{text}");
    assert!(text.contains("def pct_of(cents: int, pct: float) -> int"), "{text}");
    let first = prompt(&store, "s1", &["price_for", "pct_of", "ghost_fn"]);
    let text = first["text"].as_str().unwrap_or("");
    assert!(text.contains("def price_for(booking, member, space) -> int [5-8] — Cents the member pays for this booking."), "{text}");
    // the callers, and where none are
    assert!(text.contains("price_for ← called from lanternly/"), "{text}");
    assert!(text.contains("no code in the map is named ghost_fn"), "{text}");
}

#[test]
fn the_session_start_names_the_repo_id_once() {
    // every Cork tool takes repo_id; v76's session start dropped it and
    // agents went hunting for it (18 commands a run in the scaling study)
    let store = fixture();
    let start = session_start(&store, "s1");
    let text = start["text"].as_str().unwrap_or("");
    assert_eq!(text.matches("github.com/acme/lanternly").count(), 1, "{text}");
    assert!(text.contains("Cork briefing (repo_id `github.com/acme/lanternly`) — "), "{text}");
    // the prompt briefings, which ride every later message, do not repeat it
    let first = prompt(&store, "s1", &["price_for"]);
    assert!(!first["text"].as_str().unwrap_or("").contains("github.com/acme/lanternly"));
    // a repo with nothing changed this week names it too
    let quiet = Store::open(std::path::Path::new(":memory:")).unwrap();
    let files: Vec<(&str, &str, Option<&Value>)> = FILES.iter().map(|(p, c)| (*p, *c, None)).collect();
    crate::report::observe_batch(&quiet, SCOPE, &files, 0.0);
    let start = session_start(&quiet, "s2");
    let text = start["text"].as_str().unwrap_or("");
    assert!(text.contains("Cork briefing (repo_id `github.com/acme/lanternly`) — no module changed recently."), "{text}");
}

/// A prompt's step keeps what its briefing handed the agent by counts and
/// names (main.rs `prompt_exchange`): never the prompt, never a docstring or
/// a line of the code, and inside the step's bytes.
#[test]
fn a_prompt_step_keeps_the_briefing_by_counts_and_names() {
    let store = fixture();
    let mut answer = prompt(&store, "s1", &["price_for"]);
    let exchange = answer.as_object_mut().and_then(|m| m.remove("exchange"));
    assert!(exchange.as_ref().is_some_and(|x| x["symbol_names"] == json!(["price_for"])), "{exchange:?}");
    let x = crate::presence::bound_exchange(&crate::prompt_exchange(&answer, exchange, 2));
    assert!(x["modules"].as_u64().unwrap_or(0) >= 1, "{x}");
    assert!(x["module_names"].as_array().is_some_and(|m| m.iter().any(|p| p == "lanternly/billing/pricing.py")), "{x}");
    assert_eq!(x["vectors"], 2);
    assert_eq!(x["tokens"], json!(size(&answer).1));
    assert!(x["callers"].as_u64().unwrap_or(0) >= 1, "price_for's callers were expanded: {x}");
    let said = serde_json::to_string(&x).unwrap();
    assert!(said.len() <= crate::presence::MAX_X_BYTES);
    for leaked in ["Cents the member pays", "base = space.rate", "space's rate now"] {
        assert!(!said.contains(leaked), "{leaked} is in the step: {said}");
    }
    println!("a prompt step's exchange: {} bytes: {said}", said.len());
}

/// Every symbol carries the range to Read instead of the file, a module
/// whose every symbol is listed says it is complete and unchanged, and the
/// scope line says what the page covers rather than what it missed.
#[test]
fn every_symbol_carries_its_line_range_and_complete_modules_say_so() {
    let store = fixture();
    let start = session_start(&store, "s1");
    let text = start["text"].as_str().unwrap_or("");
    assert!(text.contains("To edit, Read only the range you need (Read with offset/limit)"), "{text}");
    assert!(text.contains("  def pct_of(cents: int, pct: float) -> int [4-6] — "), "{text}");
    assert!(text.contains("lanternly/utils/money.py — becca@x.com, 15m ago · complete · unchanged\n"), "{text}");
    let first = prompt(&store, "s1", &["price_for", "ghost_fn"]);
    let text = first["text"].as_str().unwrap_or("");
    // a one-file task: the symbol it names, then everything that calls it
    assert!(text.contains("  def price_for(booking, member, space) -> int [5-8] — "), "{text}");
    assert!(text.contains("lanternly/api/bookings.py — calls price_for; dan@x.com, 60m ago\n  def create_booking("), "{text}");
    assert!(text.contains("\nScope — every dependent of price_for within 2 hops is listed here; no blast_radius call needed; "), "{text}");
    // what the page had no room for is named, as scope; never as a gap
    assert!(text.contains("also related, by name only (get_symbol for one): tests/test_api.py"), "{text}");
    assert!(text.contains("no code in the map is named ghost_fn"), "{text}");
    assert!(!text.contains("Coverage") && !text.contains("not expanded") && !text.contains("matched nothing"), "{text}");
    // a module only indexed, never written, dates its outline by its parse
    assert!(super::outline_mark(3.0 * 3600.0) == "complete · unchanged since 3h ago");
}

/// Agent #2's session start leads with agent #1's map: the files it read
/// and changed, from its step log (trail.rs), never its closing words.
#[test]
fn the_next_session_starts_with_the_last_agents_map() {
    let store = fixture();
    let scope = SCOPE;
    for (kind, path) in [("reading", "lanternly/billing/pricing.py"), ("reading", "lanternly/utils/money.py"), ("edit", "lanternly/billing/pricing.py")] {
        crate::presence::note_last_action(&store, scope, "becca@x.com", "agent-1", kind, path, "claude-code", "", &crate::presence::Effort::default(), "", "", "", "");
    }
    crate::trail::record(&store, scope, "becca@x.com", "agent-1", false);
    let start = session_start(&store, "agent-2");
    let text = start["text"].as_str().unwrap_or("");
    assert!(text.starts_with("Map from the last agent here — becca@x.com's session (started "), "{text}");
    assert!(text.contains("\n  read: lanternly/utils/money.py\n  changed: lanternly/billing/pricing.py\nRepo shape"), "{text}");
    assert_eq!(start["map"], json!(true));
}

