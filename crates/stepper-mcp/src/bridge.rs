use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Build the `mcp__<server>__<tool>` namespaced tool name, sanitized to the
/// provider name charset (`[a-zA-Z0-9_-]`) and capped at 64 chars (a hash suffix
/// replaces the overflowing tool segment, keeping names unique).
pub fn namespaced_name(server: &str, tool: &str) -> String {
    let full = sanitize(&format!("mcp__{server}__{tool}"));
    if full.len() <= 64 {
        return full;
    }
    let server = sanitize(server);
    let hash = short_hash(tool);
    let prefix: String = format!("mcp__{server}__").chars().take(64 - hash.len()).collect();
    format!("{prefix}{hash}")
}

/// Claim a collision-free namespaced name for `(server, tool)` against the
/// names already registered this session (`name -> claiming pair`). Sanitizing
/// to `[a-zA-Z0-9_-]` is not injective (`a.b`/`a_b`, literal `__` in a tool
/// name), so a distinct pair whose plain encoding is already taken is
/// disambiguated with a pair-hash name; the exact same pair listed twice is
/// rejected (`None`) — a tool name can never silently shadow another server's
/// tool.
pub fn claim_namespaced_name(
    taken: &mut HashMap<String, (String, String)>,
    server: &str,
    tool: &str,
) -> Option<String> {
    let pair = (server.to_string(), tool.to_string());
    let plain = namespaced_name(server, tool);
    match taken.get(&plain) {
        None => {
            taken.insert(plain.clone(), pair);
            return Some(plain);
        }
        Some(existing) if *existing == pair => {
            eprintln!(
                "mcp: server '{server}' tool '{tool}' skipped: listed twice as '{plain}'"
            );
            return None;
        }
        Some(_) => {}
    }
    let fallback = disambiguated_name(server, tool);
    if taken.contains_key(&fallback) {
        eprintln!(
            "mcp: server '{server}' tool '{tool}' skipped: namespaced name '{plain}' already registered"
        );
        return None;
    }
    taken.insert(fallback.clone(), pair);
    eprintln!(
        "mcp: server '{server}' tool '{tool}' collides with an already-registered tool on '{plain}'; registered as '{fallback}'"
    );
    Some(fallback)
}

/// Collision fallback: `mcp__<server>__<16-hex hash of the raw (server, tool)
/// pair>`, capped at 64 like the overflow path. Distinct raw pairs hash to
/// distinct names even when their sanitized plain encodings coincide.
fn disambiguated_name(server: &str, tool: &str) -> String {
    let sanitized = sanitize(server);
    let hash = short_hash(&format!("{server}\0{tool}"));
    let prefix: String = format!("mcp__{sanitized}__")
        .chars()
        .take(64 - hash.len())
        .collect();
    format!("{prefix}{hash}")
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn short_hash(s: &str) -> String {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    // Full 64-bit hash (16 hex) — collisions across a server's overflow-length
    // tool names are then astronomically unlikely.
    format!("{:016x}", h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use stepper_provider::ToolSpec;

    #[test]
    fn namespaces_and_stays_valid() {
        let name = namespaced_name("context7", "resolve-library-id");
        assert_eq!(name, "mcp__context7__resolve-library-id");
        assert!(ToolSpec::name_is_valid(&name));
    }

    #[test]
    fn sanitizes_invalid_chars() {
        let name = namespaced_name("my.server", "do:thing");
        assert!(ToolSpec::name_is_valid(&name));
        assert_eq!(name, "mcp__my_server__do_thing");
    }

    #[test]
    fn caps_long_names_at_64_and_stays_valid() {
        let tool = "a".repeat(120);
        let name = namespaced_name("server", &tool);
        assert!(name.len() <= 64);
        assert!(ToolSpec::name_is_valid(&name));
    }

    #[test]
    fn sanitizes_every_illegal_char_to_underscore() {
        let name = namespaced_name("a b/c.d", "x@y!z#w");
        assert_eq!(name, "mcp__a_b_c_d__x_y_z_w");
        assert!(ToolSpec::name_is_valid(&name));
    }

    #[test]
    fn preserves_legal_alnum_dash_underscore_charset() {
        let name = namespaced_name("My-Server_9", "Tool-Name_2");
        assert_eq!(name, "mcp__My-Server_9__Tool-Name_2");
        assert!(ToolSpec::name_is_valid(&name));
    }

    #[test]
    fn long_name_uses_16_hex_hash_suffix_and_keeps_server_prefix() {
        let tool = "z".repeat(200);
        let name = namespaced_name("srv", &tool);
        assert!(name.len() <= 64);
        assert_eq!(name, format!("mcp__srv__{}", &name[name.len() - 16..]));
        let hash = &name[name.len() - 16..];
        assert!(
            hash.chars().all(|c| c.is_ascii_hexdigit()),
            "hash suffix must be 16 hex digits, got {hash}"
        );
        assert!(ToolSpec::name_is_valid(&name));
    }

    #[test]
    fn very_long_server_prefix_is_truncated_to_fit_64() {
        let server = "s".repeat(80);
        let tool = "t".repeat(80);
        let name = namespaced_name(&server, &tool);
        assert_eq!(name.len(), 64);
        let hash = &name[name.len() - 16..];
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(name.starts_with("mcp__sssss"));
        assert!(ToolSpec::name_is_valid(&name));
    }

    #[test]
    fn long_name_hashing_is_deterministic() {
        let tool = "deterministic-".repeat(20);
        let first = namespaced_name("server", &tool);
        let second = namespaced_name("server", &tool);
        assert_eq!(first, second);
    }

    #[test]
    fn distinct_overflow_tools_hash_to_distinct_names() {
        let a = namespaced_name("server", &format!("alpha-{}", "x".repeat(120)));
        let b = namespaced_name("server", &format!("omega-{}", "x".repeat(120)));
        assert_ne!(a, b);
        assert!(a.len() <= 64);
        assert!(b.len() <= 64);
        assert_eq!(&a[..10], &b[..10]);
        assert!(ToolSpec::name_is_valid(&a));
        assert!(ToolSpec::name_is_valid(&b));
    }

    #[test]
    fn boundary_64_char_name_is_not_hashed() {
        let tool = "t".repeat(64 - "mcp__s__".len());
        let name = namespaced_name("s", &tool);
        assert_eq!(name.len(), 64);
        assert_eq!(name, format!("mcp__s__{tool}"));
        assert!(ToolSpec::name_is_valid(&name));
    }

    #[test]
    fn underscore_segments_do_not_collide_across_the_separator() {
        let mut taken = HashMap::new();
        let first = claim_namespaced_name(&mut taken, "a_b", "c").expect("first claim");
        let second = claim_namespaced_name(&mut taken, "a", "b_c").expect("second claim");
        assert_eq!(first, "mcp__a_b__c");
        assert_eq!(second, "mcp__a__b_c");
        assert_ne!(first, second);
    }

    #[test]
    fn literal_double_underscore_collision_is_disambiguated() {
        assert_eq!(namespaced_name("a", "b__c"), namespaced_name("a__b", "c"));
        let mut taken = HashMap::new();
        let first = claim_namespaced_name(&mut taken, "a", "b__c").expect("first claim");
        let second = claim_namespaced_name(&mut taken, "a__b", "c").expect("second claim");
        assert_eq!(first, "mcp__a__b__c");
        assert_ne!(second, first, "a colliding pair must get a distinct name");
        assert!(second.starts_with("mcp__a__b__"));
        assert!(second.len() <= 64);
        assert!(ToolSpec::name_is_valid(&second));
        assert!(
            second[second.len() - 16..].chars().all(|c| c.is_ascii_hexdigit()),
            "fallback carries a 16-hex pair hash, got {second}"
        );
    }

    #[test]
    fn sanitize_collision_across_servers_is_disambiguated() {
        assert_eq!(namespaced_name("a.b", "t"), namespaced_name("a_b", "t"));
        let mut taken = HashMap::new();
        let first = claim_namespaced_name(&mut taken, "a.b", "t").expect("first claim");
        let second = claim_namespaced_name(&mut taken, "a_b", "t").expect("second claim");
        assert_eq!(first, "mcp__a_b__t");
        assert_ne!(second, first);
        assert!(ToolSpec::name_is_valid(&second));
    }

    #[test]
    fn the_same_pair_listed_twice_is_rejected_not_disambiguated() {
        let mut taken = HashMap::new();
        assert!(claim_namespaced_name(&mut taken, "srv", "dup").is_some());
        assert_eq!(
            claim_namespaced_name(&mut taken, "srv", "dup"),
            None,
            "a duplicate listing of the same (server, tool) must be skipped"
        );
    }

    #[test]
    fn clean_names_are_claimed_unchanged() {
        let mut taken = HashMap::new();
        let name =
            claim_namespaced_name(&mut taken, "context7", "resolve-library-id").expect("claim");
        assert_eq!(name, "mcp__context7__resolve-library-id");
    }

    #[test]
    fn disambiguated_name_is_deterministic_and_pair_sensitive() {
        let a1 = disambiguated_name("a", "b__c");
        let a2 = disambiguated_name("a", "b__c");
        let b = disambiguated_name("a__b", "c");
        assert_eq!(a1, a2);
        assert_ne!(a1, b);
        assert!(ToolSpec::name_is_valid(&a1));
        assert!(ToolSpec::name_is_valid(&b));
    }

    #[test]
    fn disambiguated_name_with_a_long_server_stays_within_64() {
        let server = "s".repeat(80);
        let name = disambiguated_name(&server, "tool");
        assert_eq!(name.len(), 64);
        assert!(ToolSpec::name_is_valid(&name));
    }

    #[test]
    fn boundary_65_char_name_is_hashed_into_distinct_shape() {
        let plain_tool = "t".repeat(64 - "mcp__s__".len());
        let over_tool = format!("{plain_tool}t");
        let plain = namespaced_name("s", &plain_tool);
        let over = namespaced_name("s", &over_tool);
        assert_eq!(plain.len(), 64);
        assert_eq!(over, format!("mcp__s__{}", &over[over.len() - 16..]));
        assert_ne!(plain, over);
        assert!(over[over.len() - 16..].chars().all(|c| c.is_ascii_hexdigit()));
        assert!(ToolSpec::name_is_valid(&over));
    }
}
