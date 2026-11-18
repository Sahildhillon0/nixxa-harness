//! What a step cost, from the official price table
//! (<https://platform.claude.com/docs/en/about-claude/pricing>, read 2026-11-18).
//!
//! A model that isn't listed costs `None`: never a guess.

/// Prices in thousandths of a micro-dollar per token (`$1 / MTok` = 1 µ$/token = 1000).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rates {
    input: u64,
    cache_write_5m: u64,
    cache_write_1h: u64,
    cache_read: u64,
    output: u64,
}

const fn rates(input: u64, cache_write_5m: u64, cache_write_1h: u64, cache_read: u64, output: u64) -> Rates {
    Rates { input, cache_write_5m, cache_write_1h, cache_read, output }
}

/// Claude Haiku 5.5 charges more once a prompt exceeds this many tokens.
const HAIKU_5_5_THRESHOLD: u64 = 100_000;

fn table(model: &str, prompt_tokens: u64) -> Option<Rates> {
    Some(match model {
        "claude-fable-5-2" => rates(8_000, 10_000, 16_000, 200, 40_000),
        "claude-fable-5-1" => rates(10_000, 12_500, 20_000, 250, 50_000),
        "claude-fable-5" => rates(10_000, 12_500, 20_000, 1_000, 50_000),
        "claude-opus-5-5" => rates(4_000, 5_000, 8_000, 200, 20_000),
        "claude-opus-5" | "claude-opus-4-8" | "claude-opus-4-7" | "claude-opus-4-6" => {
            rates(5_000, 6_250, 10_000, 500, 25_000)
        }
        "claude-sonnet-5-5" => rates(2_000, 2_500, 4_000, 100, 10_000),
        "claude-sonnet-5" => rates(2_000, 2_500, 4_000, 200, 10_000),
        "claude-sonnet-4-6" => rates(3_000, 3_750, 6_000, 300, 15_000),
        "claude-haiku-5-5" if prompt_tokens > HAIKU_5_5_THRESHOLD => rates(500, 625, 1_000, 50, 2_500),
        "claude-haiku-5-5" => rates(100, 125, 200, 10, 500),
        _ => return None,
    })
}

/// Token counts of one step, as the API reports them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Tokens {
    /// Uncached input.
    pub(crate) input: u64,
    pub(crate) cache_write_5m: u64,
    pub(crate) cache_write_1h: u64,
    pub(crate) cache_read: u64,
    pub(crate) output: u64,
}

impl Tokens {
    /// Every input token, cached or not.
    pub(crate) fn total_input(&self) -> u64 {
        self.input + self.cache_write_5m + self.cache_write_1h + self.cache_read
    }
}

/// The cost in micro-dollars, or `None` if the model's price isn't known.
pub(crate) fn cost_micro_usd(model: &str, tokens: &Tokens) -> Option<u64> {
    let rates = table(model, tokens.total_input())?;
    let milli_micro = tokens.input * rates.input
        + tokens.cache_write_5m * rates.cache_write_5m
        + tokens.cache_write_1h * rates.cache_write_1h
        + tokens.cache_read * rates.cache_read
        + tokens.output * rates.output;
    Some(milli_micro.div_ceil(1000))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sonnet_5_5_matches_the_price_page() {
        // 1M input at $2 + 1M output at $10 = $12.
        let tokens = Tokens { input: 1_000_000, output: 1_000_000, ..Tokens::default() };
        assert_eq!(cost_micro_usd("claude-sonnet-5-5", &tokens), Some(12_000_000));
        // Cache reads at $0.10 / MTok, 5m writes at $2.50 / MTok.
        let cached = Tokens { cache_read: 1_000_000, cache_write_5m: 1_000_000, ..Tokens::default() };
        assert_eq!(cost_micro_usd("claude-sonnet-5-5", &cached), Some(2_600_000));
    }

    #[test]
    fn small_steps_round_up_to_a_micro_dollar() {
        let tokens = Tokens { input: 1, ..Tokens::default() };
        assert_eq!(cost_micro_usd("claude-haiku-5-5", &tokens), Some(1));
    }

    #[test]
    fn haiku_5_5_switches_price_over_100k_prompt_tokens() {
        let small = Tokens { input: 100_000, ..Tokens::default() };
        let large = Tokens { input: 100_001, ..Tokens::default() };
        assert_eq!(cost_micro_usd("claude-haiku-5-5", &small), Some(10_000));
        assert_eq!(cost_micro_usd("claude-haiku-5-5", &large), Some(50_001));
    }

    #[test]
    fn unknown_models_have_no_price() {
        assert_eq!(cost_micro_usd("claude-sonnet-9", &Tokens { input: 10, ..Tokens::default() }), None);
    }
}
