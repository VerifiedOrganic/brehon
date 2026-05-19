//! Dynamic agent name generation.
//!
//! Generates memorable names like "swift-fox-42" for workers and reviewers.

use std::collections::HashSet;

const ADJECTIVES: &[&str] = &[
    "swift", "bright", "calm", "bold", "keen", "warm", "sharp", "quick", "cool", "fair", "kind",
    "neat", "wise", "glad", "pure", "safe", "deep", "true", "free", "soft", "firm", "rich", "dark",
    "wild",
];

const ANIMALS: &[&str] = &[
    "fox", "owl", "elk", "jay", "ram", "bee", "ant", "emu", "yak", "cod", "bat", "cat", "dog",
    "hen", "pig", "rat", "ape", "cow", "doe", "ewe", "gnu", "kit", "pup", "cub",
];

pub fn generate_names(count: usize) -> Vec<String> {
    use std::time::{SystemTime, UNIX_EPOCH};

    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;

    let mut state = seed;
    let mut used = HashSet::new();
    let mut names = Vec::with_capacity(count);

    for _ in 0..count {
        let mut attempts = 0;
        loop {
            // Simple xorshift64 PRNG
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;

            let adj_idx = (state as usize) % ADJECTIVES.len();
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let animal_idx = (state as usize) % ANIMALS.len();
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let num = 10 + (state % 90) as u8;

            let name = format!("{}-{}-{}", ADJECTIVES[adj_idx], ANIMALS[animal_idx], num);
            if used.insert(name.clone()) {
                names.push(name);
                break;
            }
            attempts += 1;
            if attempts > 100 {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let fallback = format!("agent-{}", 1000 + (state % 9000) as u16);
                names.push(fallback);
                break;
            }
        }
    }

    names
}
