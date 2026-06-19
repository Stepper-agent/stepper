use serde::{Deserialize, Serialize};

/// Normalized token accounting, unified across the OpenAI single-usage shape and
/// the Anthropic split-usage shape (input/cache arrive in `message_start`, the
/// final output count arrives in `message_delta`).
///
/// The four fields are **mutually exclusive**: `input` is the *uncached* prompt
/// tokens only (dialects that fold cached tokens into their prompt count, like
/// OpenAI, subtract them out at the wire), so the full prompt is
/// `input + cache_read + cache_write` and the conversation footprint is
/// `context_tokens()`. Cost can therefore price each field independently.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Usage {
    /// Field-wise max. Within a single stream every field is either reported
    /// once (OpenAI) or grows monotonically (Anthropic output deltas), so taking
    /// the max yields the final figure without double-counting partial frames.
    pub fn merge(&mut self, other: &Usage) {
        self.input = self.input.max(other.input);
        self.output = self.output.max(other.output);
        self.cache_read = self.cache_read.max(other.cache_read);
        self.cache_write = self.cache_write.max(other.cache_write);
    }

    /// Field-wise saturating sum. Used to ACCUMULATE usage across separate API
    /// calls (each ReAct step / layer is its own request), unlike `merge` (max),
    /// which folds partial frames *within* one stream.
    pub fn add(&mut self, other: &Usage) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_write = self.cache_write.saturating_add(other.cache_write);
    }

    pub fn total(&self) -> u64 {
        self.input + self.output
    }

    /// Total tokens this exchange occupies in the context window: the full prompt
    /// (uncached `input` + cached `cache_read` + freshly cached `cache_write`)
    /// plus the reply. Unlike `total()` this includes cached prompt tokens, so a
    /// heavily-cached request (Anthropic) is measured at its real size rather than
    /// collapsing to just the uncached delta — which is what lets compaction fire
    /// before the real prompt overflows the window.
    pub fn context_tokens(&self) -> u64 {
        self.input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
            .saturating_add(self.output)
    }
}

#[cfg(test)]
mod tests {
    use super::Usage;

    #[test]
    fn context_tokens_counts_the_full_cached_prompt_not_just_the_uncached_delta() {
        // A heavily-cached Anthropic request: 50 uncached + 40_000 cached prompt,
        // 20 output. `total()` (input+output) collapses to 70 — which would make
        // compaction think the window is nearly empty. `context_tokens()` sees the
        // real ~40_070 footprint.
        let u = Usage { input: 50, output: 20, cache_read: 40_000, cache_write: 0 };
        assert_eq!(u.total(), 70);
        assert_eq!(u.context_tokens(), 40_070);
    }

    #[test]
    fn add_sums_fields_across_calls_unlike_merge_which_takes_max() {
        let a = Usage { input: 800, output: 200, cache_read: 10, cache_write: 5 };
        let b = Usage { input: 800, output: 300, cache_read: 0, cache_write: 0 };

        let mut summed = a;
        summed.add(&b);
        assert_eq!(summed.input, 1600, "add accumulates across separate requests");
        assert_eq!(summed.output, 500);
        assert_eq!(summed.cache_read, 10);
        assert_eq!(summed.cache_write, 5);

        let mut merged = a;
        merged.merge(&b);
        assert_eq!(merged.input, 800, "merge keeps the max (intra-stream folding)");
        assert_eq!(merged.output, 300);
    }

    #[test]
    fn add_saturates_instead_of_overflowing() {
        let mut a = Usage { input: u64::MAX, output: 0, cache_read: 0, cache_write: 0 };
        a.add(&Usage { input: 10, output: 0, cache_read: 0, cache_write: 0 });
        assert_eq!(a.input, u64::MAX);
    }
}
