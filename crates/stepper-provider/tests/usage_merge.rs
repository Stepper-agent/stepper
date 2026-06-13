//! Field-wise `Usage::merge` contract, including cache fields and the Anthropic
//! split-usage shape where `message_start` carries input+cache and a later
//! `message_delta` carries the final output count.

use stepper_provider::Usage;

#[test]
fn merge_takes_the_max_of_each_field_independently() {
    let mut acc = Usage {
        input: 100,
        output: 20,
        cache_read: 5,
        cache_write: 9,
    };
    acc.merge(&Usage {
        input: 80,
        output: 60,
        cache_read: 30,
        cache_write: 1,
    });
    assert_eq!(
        acc,
        Usage {
            input: 100,
            output: 60,
            cache_read: 30,
            cache_write: 9,
        }
    );
}

#[test]
fn anthropic_split_usage_absorbs_message_start_then_message_delta() {
    let mut acc = Usage::default();
    acc.merge(&Usage {
        input: 1200,
        output: 0,
        cache_read: 400,
        cache_write: 64,
    });
    acc.merge(&Usage {
        input: 0,
        output: 350,
        cache_read: 0,
        cache_write: 0,
    });
    assert_eq!(
        acc,
        Usage {
            input: 1200,
            output: 350,
            cache_read: 400,
            cache_write: 64,
        }
    );
}

#[test]
fn merge_with_default_is_idempotent() {
    let original = Usage {
        input: 42,
        output: 13,
        cache_read: 7,
        cache_write: 3,
    };
    let mut acc = original;
    acc.merge(&Usage::default());
    assert_eq!(acc, original);
}

#[test]
fn total_sums_input_and_output_only_excluding_cache() {
    let usage = Usage {
        input: 100,
        output: 250,
        cache_read: 9999,
        cache_write: 8888,
    };
    assert_eq!(usage.total(), 350);
}

#[test]
fn repeated_merges_never_decrease_any_field() {
    let mut acc = Usage::default();
    for frame in [
        Usage { input: 10, output: 5, cache_read: 0, cache_write: 0 },
        Usage { input: 10, output: 5, cache_read: 0, cache_write: 0 },
        Usage { input: 10, output: 3, cache_read: 0, cache_write: 0 },
    ] {
        acc.merge(&frame);
    }
    assert_eq!(acc.output, 5, "a smaller later frame must not lower the max");
    assert_eq!(acc.input, 10);
}
