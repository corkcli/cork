//! What a model's tokens cost: the providers' own list prices, per counter.
//!
//! Every rate here was read off the provider's pricing page on 2026-10-17
//! (Standard tier, USD per million tokens):
//! - Anthropic: https://platform.claude.com/docs/en/about-claude/pricing
//!   (5-minute cache writes 1.25x input, 1-hour writes 2x, cache hits 0.1x
//!   except Opus 5.5 / Sonnet 5.5 at 0.05x and Fable 5.1 / Mythos 5.1 at
//!   0.025x; fast mode is 2x on Opus 5.5 / 5 / 4.8; US-only inference
//!   (`inference_geo: us`) is 1.1x on 4.6 and later; Haiku 5.5 prompts over
//!   100K tokens are priced at 5x).
//! - OpenAI: https://developers.openai.com/api/docs/pricing and the model
//!   pages under https://developers.openai.com/api/docs/models/ (cached
//!   input as listed; cache writes 1.25x input on GPT-5.6 and later, none
//!   before; prompts over 272K input tokens 2x input and cache, 1.5x output
//!   where the page lists a long-context price; Fast mode 2x; reasoning
//!   tokens are output tokens and are already inside `output_tokens`).
//!
//! A model id is matched after normalising away what providers add around
//! it (a date, a Bedrock/Vertex wrapper, an OpenRouter-style `vendor/`
//! prefix, Claude's dotted version spelling): exactly, else by the longest
//! table id it extends at a `-` boundary, so `claude-haiku-4-5-20251001`
//! is Haiku 4.5 and `gpt-5.6-luna` is never `gpt-5`. An id that matches
//! nothing is priced at [`DEFAULT`] and reported as unpriced, never free.

/// One model's list prices, $/Mtok.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    /// a cache write that lives 5 minutes (OpenAI: its only kind of write)
    pub write_5m: f64,
    /// a cache write that lives an hour (Anthropic only; OpenAI: = write_5m)
    pub write_1h: f64,
    /// prompts over this many tokens (input + cache read + cache write) are
    /// priced at the long multipliers for the whole request; 0 = no tier
    pub long_over: i64,
    /// (input and cache, output) multipliers past `long_over`
    pub long_x: (f64, f64),
}

const fn anthropic(input: f64, output: f64, cache_read: f64) -> Price {
    Price { input, output, cache_read, write_5m: input * 1.25, write_1h: input * 2.0, long_over: 0, long_x: (1.0, 1.0) }
}

/// An OpenAI model with no cache-write charge (GPT-5.5 and earlier): a
/// written token is billed as the input it is.
const fn openai(input: f64, output: f64, cache_read: f64) -> Price {
    Price { input, output, cache_read, write_5m: input, write_1h: input, long_over: 0, long_x: (1.0, 1.0) }
}

/// GPT-5.6 and later: writes at 1.25x input, and the 272K long-context tier.
const fn openai_56(input: f64, output: f64, cache_read: f64) -> Price {
    Price { input, output, cache_read, write_5m: input * 1.25, write_1h: input * 1.25, long_over: 272_000, long_x: (2.0, 1.5) }
}

/// A pre-5.6 OpenAI model whose page lists a 272K long-context price.
const fn openai_long(input: f64, output: f64, cache_read: f64) -> Price {
    Price { long_over: 272_000, long_x: (2.0, 1.5), ..openai(input, output, cache_read) }
}

const HAIKU_5_5: Price = Price { long_over: 100_000, long_x: (5.0, 5.0), ..anthropic(0.10, 0.50, 0.01) };

/// Normalised model id -> list price. Order does not matter: the longest
/// matching id wins.
const TABLE: &[(&str, Price)] = &[
    // Anthropic
    ("claude-fable-5-1", anthropic(10.0, 50.0, 0.25)),
    ("claude-mythos-5-1", anthropic(10.0, 50.0, 0.25)),
    ("claude-fable-5", anthropic(10.0, 50.0, 1.0)),
    ("claude-mythos-5", anthropic(10.0, 50.0, 1.0)),
    ("claude-opus-5-5", anthropic(4.0, 20.0, 0.20)),
    ("claude-opus-5", anthropic(5.0, 25.0, 0.50)),
    ("claude-opus-4-8", anthropic(5.0, 25.0, 0.50)),
    ("claude-opus-4-7", anthropic(5.0, 25.0, 0.50)),
    ("claude-opus-4-6", anthropic(5.0, 25.0, 0.50)),
    ("claude-opus-4-5", anthropic(5.0, 25.0, 0.50)),
    ("claude-opus-4-1", anthropic(15.0, 75.0, 1.50)),
    ("claude-opus-4", anthropic(15.0, 75.0, 1.50)),
    ("claude-sonnet-5-5", anthropic(2.0, 10.0, 0.10)),
    ("claude-sonnet-5", anthropic(2.0, 10.0, 0.20)),
    ("claude-sonnet-4-6", anthropic(3.0, 15.0, 0.30)),
    ("claude-sonnet-4-5", anthropic(3.0, 15.0, 0.30)),
    ("claude-sonnet-4", anthropic(3.0, 15.0, 0.30)),
    ("claude-haiku-5-5", HAIKU_5_5),
    ("claude-haiku-4-5", anthropic(1.0, 5.0, 0.10)),
    ("claude-3-5-haiku", anthropic(0.80, 4.0, 0.08)),
    // a Claude id newer than this table: its family's newest listed price
    ("claude-fable", anthropic(10.0, 50.0, 0.25)),
    ("claude-mythos", anthropic(10.0, 50.0, 0.25)),
    ("claude-opus", anthropic(4.0, 20.0, 0.20)),
    ("claude-sonnet", anthropic(2.0, 10.0, 0.10)),
    ("claude-haiku", HAIKU_5_5),
    // OpenAI
    ("gpt-6-astra", openai_56(10.0, 50.0, 1.0)),
    ("gpt-6.1-sol", openai_56(2.0, 10.0, 0.10)),
    ("gpt-6-sol", openai_56(2.0, 10.0, 0.20)),
    ("gpt-6-luna", openai_56(0.10, 0.50, 0.01)),
    ("gpt-5.6-sol", openai_56(4.0, 20.0, 0.40)),
    ("gpt-5.6-terra", openai_56(2.0, 12.0, 0.20)),
    ("gpt-5.6-luna", openai_56(0.20, 1.20, 0.02)),
    ("gpt-5.5", openai_long(5.0, 30.0, 0.50)),
    ("gpt-5.5-pro", openai_long(30.0, 180.0, 30.0)),
    ("gpt-5.4", openai_long(2.5, 15.0, 0.25)),
    ("gpt-5.4-pro", openai_long(30.0, 180.0, 30.0)),
    ("gpt-5.4-mini", openai(0.75, 4.5, 0.075)),
    ("gpt-5.4-nano", openai(0.20, 1.25, 0.02)),
    ("gpt-5.3-codex", openai(1.75, 14.0, 0.175)),
    ("gpt-5.2", openai(1.75, 14.0, 0.175)),
    ("gpt-5.2-codex", openai(1.75, 14.0, 0.175)),
    ("gpt-5.2-pro", openai(21.0, 168.0, 21.0)),
    ("gpt-5.1", openai(1.25, 10.0, 0.125)),
    ("gpt-5.1-codex", openai(1.25, 10.0, 0.125)),
    ("gpt-5.1-codex-max", openai(1.25, 10.0, 0.125)),
    ("gpt-5.1-codex-mini", openai(0.25, 2.0, 0.025)),
    ("gpt-5", openai(1.25, 10.0, 0.125)),
    ("gpt-5-codex", openai(1.25, 10.0, 0.125)),
    ("gpt-5-mini", openai(0.25, 2.0, 0.025)),
    ("gpt-5-nano", openai(0.05, 0.40, 0.005)),
    ("gpt-5-pro", openai(15.0, 120.0, 15.0)),
    ("codex-mini-latest", openai(1.50, 6.0, 0.375)),
];

/// An id nothing above matches: a mid-tier model's shape ($3 in / $15 out,
/// cache reads a tenth of input), deliberate and documented rather than
/// silently free. Costs that use it say so (`unpriced_models`).
pub const DEFAULT: Price = anthropic(3.0, 15.0, 0.30);

/// Fast mode (Anthropic `speed: fast`, OpenAI Fast): every counter at 2x.
pub const FAST_X: f64 = 2.0;
/// US-only inference (Anthropic `inference_geo: us`): every counter at 1.1x.
pub const US_X: f64 = 1.1;

/// The id as the table spells it: lowercase, no vendor/region wrapper, no
/// date or version suffix, Claude's dots as dashes.
pub fn normalize(model: &str) -> String {
    let mut id = model.trim().to_ascii_lowercase();
    // `anthropic/claude-opus-4.6`, `openai/gpt-5`
    if let Some((_, rest)) = id.rsplit_once('/') {
        id = rest.to_string();
    }
    // `claude-opus-5-5[1m]`
    if let Some(at) = id.find('[') {
        id.truncate(at);
    }
    // Vertex `claude-haiku-4-5@20251001`
    if let Some(at) = id.find('@') {
        id.truncate(at);
    }
    // Bedrock `us.anthropic.claude-…-v1:0`
    if let Some(at) = id.find("anthropic.") {
        id = id[at + "anthropic.".len()..].to_string();
    }
    if let Some(at) = id.rfind("-v") {
        let tail = &id[at + 2..];
        if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit() || c == ':') {
            id.truncate(at);
        }
    }
    // a dated snapshot: `-20251001`, `-2025-08-07`
    let strip_date = |id: &mut String| {
        let bytes = id.as_bytes();
        let n = bytes.len();
        if n > 9 && bytes[n - 9] == b'-' && bytes[n - 8..].iter().all(u8::is_ascii_digit) {
            id.truncate(n - 9);
        } else if n > 11
            && bytes[n - 11] == b'-'
            && bytes[n - 6] == b'-'
            && bytes[n - 3] == b'-'
            && [&bytes[n - 10..n - 6], &bytes[n - 5..n - 3], &bytes[n - 2..]].iter().all(|p| p.iter().all(u8::is_ascii_digit))
        {
            id.truncate(n - 11);
        }
    };
    strip_date(&mut id);
    if id.starts_with("claude") {
        id = id.replace('.', "-");
    }
    id
}

/// The list price for a model id, and whether the table knew it.
pub fn lookup(model: &str) -> (Price, bool) {
    let id = normalize(model);
    let mut best: Option<(&str, Price)> = None;
    for (key, price) in TABLE {
        let fits = id == *key || (id.starts_with(key) && id.as_bytes().get(key.len()) == Some(&b'-'));
        if fits && best.is_none_or(|(b, _)| key.len() > b.len()) {
            best = Some((key, *price));
        }
    }
    match best {
        Some((_, price)) => (price, true),
        None => (DEFAULT, false),
    }
}

pub fn price(model: &str) -> Price {
    lookup(model).0
}

/// How one bill (a ledger row, or a hook's usage entry) was served: the
/// multiplier that applies to every counter, and whether its prompts were
/// past the model's long-context line.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Serving {
    pub fast: bool,
    pub us: bool,
    /// the smallest prompt size every message of the bill was over (the
    /// hook buckets messages by 100K and 272K), 0 = none
    pub prompt_over: i64,
}

impl Serving {
    pub fn of(payload: &serde_json::Value) -> Serving {
        let text = |k: &str| payload.get(k).and_then(serde_json::Value::as_str).unwrap_or("");
        Serving {
            fast: text("speed") == "fast",
            us: text("inference_geo") == "us",
            prompt_over: payload.get("prompt_over").and_then(serde_json::Value::as_i64).unwrap_or(0),
        }
    }
}

/// Tokens on one bill, by counter. `cache_write_1h` is the part of
/// `cache_write` that was written for an hour.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Counts {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub cache_write_1h: i64,
}

/// What `counts` cost on `model`, served as `serving`, in USD.
pub fn usd(model: &str, counts: &Counts, serving: &Serving) -> f64 {
    let p = price(model);
    let (mut in_x, mut out_x) = (1.0, 1.0);
    if p.long_over > 0 && serving.prompt_over >= p.long_over {
        in_x = p.long_x.0;
        out_x = p.long_x.1;
    }
    let mut x = 1.0;
    if serving.fast {
        x *= FAST_X;
    }
    if serving.us {
        x *= US_X;
    }
    let hour = counts.cache_write_1h.clamp(0, counts.cache_write.max(0));
    let five = counts.cache_write.max(0) - hour;
    let usd = counts.input as f64 * p.input * in_x
        + counts.output as f64 * p.output * out_x
        + counts.cache_read as f64 * p.cache_read * in_x
        + five as f64 * p.write_5m * in_x
        + hour as f64 * p.write_1h * in_x;
    usd * x / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rates(model: &str) -> (f64, f64, f64, f64, f64) {
        let p = price(model);
        (p.input, p.output, p.cache_read, p.write_5m, p.write_1h)
    }

    #[test]
    fn ids_are_matched_through_dates_wrappers_and_spellings() {
        for (id, want) in [
            ("claude-haiku-4-5-20251001", "claude-haiku-4-5"),
            ("claude-opus-4-5-20251101", "claude-opus-4-5"),
            ("claude-haiku-4-5@20251001", "claude-haiku-4-5"),
            ("anthropic.claude-opus-5-5", "claude-opus-5-5"),
            ("us.anthropic.claude-haiku-4-5-20251001-v1:0", "claude-haiku-4-5"),
            ("anthropic.claude-opus-4-6-v1", "claude-opus-4-6"),
            ("anthropic/claude-opus-4.6", "claude-opus-4-6"),
            ("claude-opus-5-5[1m]", "claude-opus-5-5"),
            ("Claude-Sonnet-5", "claude-sonnet-5"),
            ("gpt-5-2025-08-07", "gpt-5"),
            ("openai/gpt-6-luna", "gpt-6-luna"),
            ("gpt-5.6-luna", "gpt-5.6-luna"),
        ] {
            assert_eq!(normalize(id), want, "{id}");
        }
    }

    #[test]
    fn the_longest_listed_id_wins_and_a_dot_is_not_a_boundary() {
        assert_eq!(rates("claude-opus-5-5"), (4.0, 20.0, 0.20, 5.0, 8.0));
        assert_eq!(rates("claude-opus-5"), (5.0, 25.0, 0.50, 6.25, 10.0));
        assert_eq!(rates("claude-opus-4-1-20250805"), (15.0, 75.0, 1.50, 18.75, 30.0));
        assert_eq!(rates("claude-opus-4-20250514"), (15.0, 75.0, 1.50, 18.75, 30.0));
        assert_eq!(rates("claude-sonnet-5"), (2.0, 10.0, 0.20, 2.5, 4.0));
        assert_eq!(rates("claude-sonnet-5-5"), (2.0, 10.0, 0.10, 2.5, 4.0));
        assert_eq!(rates("claude-sonnet-4-5-20250929"), (3.0, 15.0, 0.30, 3.75, 6.0));
        assert_eq!(rates("claude-haiku-4-5-20251001"), (1.0, 5.0, 0.10, 1.25, 2.0));
        assert_eq!(rates("claude-fable-5-1"), (10.0, 50.0, 0.25, 12.5, 20.0));
        assert_eq!(rates("claude-fable-5"), (10.0, 50.0, 1.0, 12.5, 20.0));
        assert_eq!(rates("gpt-6-luna"), (0.10, 0.50, 0.01, 0.125, 0.125));
        assert_eq!(rates("gpt-5.6-luna"), (0.20, 1.20, 0.02, 0.25, 0.25));
        assert_eq!(rates("gpt-5-codex"), (1.25, 10.0, 0.125, 1.25, 1.25));
        assert_eq!(rates("gpt-5.1-codex-mini"), (0.25, 2.0, 0.025, 0.25, 0.25));
        assert!(lookup("gpt-6-luna").1 && lookup("claude-haiku-4-5-20251001").1);
        // a newer Claude than the table: its family, still known
        assert_eq!(rates("claude-opus-6"), rates("claude-opus-5-5"));
        // nothing like it: the default, and said to be unpriced
        assert_eq!(lookup("gpt-reserve"), (DEFAULT, false));
        assert_eq!(lookup("gpt-5.7-luna"), (DEFAULT, false), "a dot is not a boundary: never gpt-5");
        assert_eq!(lookup("<synthetic>"), (DEFAULT, false));
    }

    #[test]
    fn a_bill_is_priced_per_counter_with_its_write_ttls_and_serving() {
        // claude -p on Haiku 4.5 (2026-10-17), against its total_cost_usd 0.0164123:
        // 10 in, 54 out, 13,863 cache read, 7,373 written for an hour
        let counts = Counts { input: 10, output: 54, cache_read: 13_863, cache_write: 7_373, cache_write_1h: 7_373 };
        assert!((usd("claude-haiku-4-5-20251001", &counts, &Serving::default()) - 0.0164123).abs() < 1e-9);
        // a Haiku 4.5 subagent's 5-minute writes, against its costUSD 0.0194473
        let sub = Counts { input: 18, output: 339, cache_read: 12_143, cache_write: 13_216, cache_write_1h: 0 };
        assert!((usd("claude-haiku-4-5-20251001", &sub, &Serving::default()) - 0.0194473).abs() < 1e-9);
        // fast mode doubles everything; US-only adds a tenth on top
        let plain = usd("claude-opus-5-5", &counts, &Serving::default());
        let fast_us = usd("claude-opus-5-5", &counts, &Serving { fast: true, us: true, prompt_over: 0 });
        assert!((fast_us - plain * 2.2).abs() < 1e-12);
        // Haiku 5.5 past 100K is 5x; gpt-6-luna past 272K is 2x in, 1.5x out; a 100K bucket is not 272K
        let one = Counts { input: 1_000_000, output: 1_000_000, ..Counts::default() };
        assert!((usd("claude-haiku-5-5", &one, &Serving { prompt_over: 100_000, ..Serving::default() }) - 3.0).abs() < 1e-9);
        assert!((usd("gpt-6-luna", &one, &Serving { prompt_over: 272_000, ..Serving::default() }) - 0.95).abs() < 1e-9);
        assert!((usd("gpt-6-luna", &one, &Serving { prompt_over: 100_000, ..Serving::default() }) - 0.60).abs() < 1e-9);
    }
}
