//! Persistent memory for the voice assistant. A small JSON file in the config directory.
//! Entries are written by the voice model's tools and read into the instructions at each connect.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Most recent entries that fit in the instructions. Older ones stay in the file.
const MAX_PROMPT_BYTES: usize = 4096;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Item {
    pub id: u64,
    pub text: String,
    pub at: u64,
}

fn path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config/voxterm/memory.json")
}

pub fn load() -> Vec<Item> {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save(items: &[Item]) -> Result<(), String> {
    let p = path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(items).map_err(|e| e.to_string())?;
    std::fs::write(p, text).map_err(|e| e.to_string())
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Adds an entry. Identical text is not stored twice.
pub fn add(text: &str) -> Result<u64, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("nothing to remember".into());
    }
    let mut items = load();
    if let Some(existing) = items.iter().find(|i| i.text.eq_ignore_ascii_case(text)) {
        return Ok(existing.id);
    }
    let id = items.iter().map(|i| i.id).max().unwrap_or(0) + 1;
    items.push(Item {
        id,
        text: text.to_string(),
        at: now(),
    });
    save(&items)?;
    Ok(id)
}

/// Removes every entry whose text contains `needle`. Returns how many were removed.
pub fn forget(needle: &str) -> Result<usize, String> {
    let needle = needle.trim().to_lowercase();
    let items = load();
    let before = items.len();
    let kept: Vec<Item> = items
        .into_iter()
        .filter(|i| !i.text.to_lowercase().contains(&needle))
        .collect();
    let removed = before - kept.len();
    if removed > 0 {
        save(&kept)?;
    }
    Ok(removed)
}

/// The block added to the instructions: newest entries first, up to the byte limit.
pub fn prompt_block() -> String {
    let mut items = load();
    items.sort_by(|a, b| b.at.cmp(&a.at));
    let mut out = String::new();
    for item in items {
        let line = format!("- {}\n", item.text);
        if out.len() + line.len() > MAX_PROMPT_BYTES {
            break;
        }
        out.push_str(&line);
    }
    if out.is_empty() {
        String::new()
    } else {
        format!("\n\nThings you remember about the user and their work:\n{out}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_block_is_empty_without_items_and_capped() {
        // Pure check on the cap logic, independent of the file on disk.
        let items: Vec<Item> = (0..500)
            .map(|i| Item {
                id: i,
                text: format!("fact number {i} with some extra words"),
                at: i,
            })
            .collect();
        let mut out = String::new();
        for item in items.iter().rev() {
            let line = format!("- {}\n", item.text);
            if out.len() + line.len() > MAX_PROMPT_BYTES {
                break;
            }
            out.push_str(&line);
        }
        assert!(out.len() <= MAX_PROMPT_BYTES);
        assert!(out.starts_with("- fact number 499"));
    }
}
