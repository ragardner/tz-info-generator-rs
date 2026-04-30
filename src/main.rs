use parse_zoneinfo::{
    line::{Line, Year},
    table::{Saving, TableBuilder},
    transitions::TableTransitions,
};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

mod tzdb;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Transition {
    pub timestamp: i64,
    pub local_timestamp: i64, // local wall-clock time when this transition occurs
    pub offset: i32,
    pub is_dst: bool,
    pub abbrev_idx: u16,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: cargo run -- <path-to-tzdata2026a>");
        std::process::exit(1);
    }
    let tzdata_dir = Path::new(&args[1]);

    // === Extract version from directory name ===
    let dir_name = tzdata_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    let version = dir_name
        .find(|c: char| c.is_ascii_digit())
        .map(|idx| &dir_name[idx..])
        .unwrap_or("unknown")
        .to_string();

    let zone_files = [
        "africa",
        "antarctica",
        "asia",
        "australasia",
        "backward",
        "backzone",
        "etcetera",
        "europe",
        "northamerica",
        "southamerica",
    ];

    let mut builder = TableBuilder::new();

    for entry in fs::read_dir(tzdata_dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_file() {
            let filename = path.file_name().unwrap().to_str().unwrap();
            if !zone_files.contains(&filename) {
                continue;
            }
            let content = fs::read_to_string(&path).unwrap();
            for raw_line in content.lines() {
                let content = raw_line.split('#').next().unwrap().trim_end();
                if content.trim().is_empty() {
                    continue;
                }
                if let Ok(parsed) = Line::new(content) {
                    if let Err(e) = builder.add_line(parsed) {
                        eprintln!("Warning: failed to add line from {}: {}", filename, e);
                    }
                }
            }
        }
    }

    let table = builder.build();

    // === Collect all unique abbreviations first ===
    let mut abbrev_set: HashSet<String> = HashSet::new();
    for name in table
        .zonesets
        .keys()
        .chain(table.links.keys())
        .map(|s| s.as_str())
    {
        if let Some(set) = table.timespans(name) {
            abbrev_set.insert(set.first.name.clone());
            for (_, ft) in &set.rest {
                abbrev_set.insert(ft.name.clone());
            }
        }
    }
    let abbrevs: Vec<String> = abbrev_set.into_iter().collect();
    let abbrev_to_idx: HashMap<String, u16> = abbrevs
        .iter()
        .enumerate()
        .map(|(i, s)| (s.clone(), i as u16))
        .collect();

    println!("Unique abbreviations found: {}", abbrevs.len());

    let mut name_to_data: HashMap<String, (Vec<Transition>, Option<usize>)> = HashMap::new();

    for name in table
        .zonesets
        .keys()
        .chain(table.links.keys())
        .map(|s| s.as_str())
    {
        if let Some(set) = table.timespans(name) {
            let mut transitions = Vec::new();

            let first = &set.first;
            let first_idx = *abbrev_to_idx.get(&first.name).unwrap();
            transitions.push(Transition {
                timestamp: i64::MIN,
                local_timestamp: i64::MIN,
                offset: first.total_offset() as i32,
                is_dst: first.dst_offset != 0,
                abbrev_idx: first_idx,
            });

            let mut prev_offset = first.total_offset() as i64;

            for (ts, ft) in &set.rest {
                let local_ts = *ts + prev_offset;

                let idx = *abbrev_to_idx.get(&ft.name).unwrap();
                transitions.push(Transition {
                    timestamp: *ts,
                    local_timestamp: local_ts,
                    offset: ft.total_offset() as i32,
                    is_dst: ft.dst_offset != 0,
                    abbrev_idx: idx,
                });

                prev_offset = ft.total_offset() as i64;
            }

            // === detect perpetual repeating rules ===
            let repeating_tail_start = {
                if let Some(zoneset) = table.get_zoneset(name) {
                    if let Some(last_zone) = zoneset.last() {
                        if let Saving::Multiple(ref rules_name) = last_zone.saving {
                            if let Some(rules) = table.rulesets.get(rules_name) {
                                if rules
                                    .iter()
                                    .any(|r| matches!(r.to_year, Some(Year::Maximum) | None))
                                {
                                    Some(transitions.len().saturating_sub(2))
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            };

            name_to_data.insert(name.to_string(), (transitions, repeating_tail_start));
        }
    }

    // Deduplication
    let mut unique: HashMap<Vec<Transition>, usize> = HashMap::new();
    let mut data_counter = 0usize;
    let mut data_names: Vec<String> = Vec::new();

    for (_, (trans, _)) in &name_to_data {
        if !unique.contains_key(trans) {
            unique.insert(trans.clone(), data_counter);
            data_names.push(format!("DATA_{}", data_counter));
            data_counter += 1;
        }
    }

    let mut entries: Vec<(String, String, Option<usize>)> = Vec::new();
    for (name, (trans, repeating_start)) in &name_to_data {
        if let Some(&id) = unique.get(trans) {
            entries.push((name.clone(), data_names[id].clone(), *repeating_start));
        }
    }
    entries.sort_by_key(|(name, _, _)| name.clone());

    // === Find UTC's DATA_N for the minimal (no "tz" feature) version ===
    let utc_data_name = entries
        .iter()
        .find(|(name, _, _)| name == "UTC")
        .map(|(_, dn, _)| dn.clone())
        .or_else(|| {
            entries
                .iter()
                .find(|(name, _, _)| name == "Etc/UTC")
                .map(|(_, dn, _)| dn.clone())
        })
        .unwrap_or_else(|| {
            // Fallback: use the first DATA_N (should never happen in practice)
            data_names
                .first()
                .cloned()
                .unwrap_or_else(|| "DATA_0".to_string())
        });

    let minimal_entries: Vec<(String, String, Option<usize>)> = entries
        .iter()
        .filter(|(_, data_name, _)| data_name == &utc_data_name)
        .cloned()
        .collect();

    println!(
        "UTC-equivalent zones (minimal mode): {} zones share {}",
        minimal_entries.len(),
        utc_data_name
    );

    // === Generate tzdb.rs with conditional TZ_ENTRIES ===
    let mut output = String::new();

    output.push_str("#![allow(clippy::large_enum_variant)]\n");
    output.push_str("#![allow(clippy::too_many_lines)]\n");
    output.push_str("#![cfg_attr(rustfmt, rustfmt::skip)]\n\n");

    // === Feature documentation ===
    output.push_str(
        "// This file is auto-generated.\n\
         //\n\
         // The `tz` feature controls whether the full IANA timezone database is included:\n\
         //   - `features = [\"tz\"]` (default for most users): Full support (~600 zones,\n\
         //     hundreds of unique transition tables). Binary size impact: several hundred KB to low MB.\n\
         //   - Without the feature: Only UTC + zones with *identical* transition data to UTC\n\
         //     are supported (typically 5-15 entries). This keeps the binary as small as possible\n\
         //     for applications that only need basic UTC/offset handling.\n\
         //\n\
         // Both modes expose the same public API (`offset_at`, `offset_info_at`, etc.).\n\
         // `TZ_ENTRIES` is conditionally compiled — the compiler picks the right version.\n\n",
    );

    output.push_str(&format!("pub static VERSION: &str = \"{}\";\n\n", version));

    // ABBREVS table (always included - very small)
    output.push_str(&format!(
        "pub static ABBREVS: [&'static str; {}] = [\n",
        abbrevs.len()
    ));
    for abbr in &abbrevs {
        output.push_str(&format!("    \"{}\",\n", abbr));
    }
    output.push_str("];\n\n");

    // Transition struct
    output.push_str("#[derive(Debug, Clone, Copy, PartialEq, Eq)]\n");
    output.push_str(
        "pub struct Transition { pub local_timestamp: i64, pub offset: i32, pub abbrev_idx: u16 }\n\n",
    );

    // === get_tz_data (already present - keep this) ===
    output.push_str("pub fn get_tz_data(name: &str) -> Option<(&str, &'static [Transition], Option<usize>)> {\n");
    output.push_str("    let idx = TZ_ENTRIES.partition_point(|(n, _, _)| *n < name);\n");
    output.push_str("    if idx < TZ_ENTRIES.len() && TZ_ENTRIES[idx].0 == name {\n");
    output.push_str("        Some(TZ_ENTRIES[idx])\n");
    output.push_str("    } else { None }\n");
    output.push_str("}\n\n");

    output.push_str(
        r#"
#[derive(Debug, Clone, Copy)]
pub struct OffsetInfo {
    pub offset: i32,
    pub abbrev: &'static str,
    pub is_gap: bool,
    pub gap_size: i64,
}

#[inline(always)]
pub fn abbrev(idx: u16) -> &'static str {
    ABBREVS[idx as usize]
}

#[inline(always)]
fn last_transition(transitions: &[Transition]) -> Option<OffsetInfo> {
    transitions.last().map(|t| OffsetInfo {
        offset: t.offset,
        abbrev: abbrev(t.abbrev_idx),
        is_gap: false,
        gap_size: 0,
    })
}

fn resolve_far_future_local(
    transitions: &[Transition],
    repeating_tail_start: Option<usize>,
    local_unix: i64,
) -> Option<OffsetInfo> {
    if let Some(start) = repeating_tail_start {
        let cycle = &transitions[start..];
        if cycle.len() < 2 {
            return last_transition(transitions);
        }
        let first = &cycle[0];
        let last = &cycle[cycle.len() - 1];
        let period = last.local_timestamp - first.local_timestamp;
        if period <= 0 {
            return last_transition(transitions);
        }
        let elapsed = local_unix - first.local_timestamp;
        if elapsed < 0 {
            return last_transition(transitions);
        }
        let position_in_cycle = elapsed % period;
        let idx = cycle.partition_point(|t| {
            (t.local_timestamp - first.local_timestamp) <= position_in_cycle
        });
        if idx == 0 {
            let t = &cycle[0];
            return Some(OffsetInfo {
                offset: t.offset,
                abbrev: abbrev(t.abbrev_idx),
                is_gap: false,
                gap_size: 0,
            });
        }
        if idx >= cycle.len() {
            return last_transition(cycle);
        }

        let prev = &cycle[idx - 1];

        // detect gap immediately after a spring-forward that starts this segment
        if idx >= 2 {
            let pprev = &cycle[idx - 2];
            let off_diff = (prev.offset - pprev.offset) as i64;
            if off_diff > 0 {
                let window_start = prev.local_timestamp;
                let window_size = off_diff;
                let window_end = window_start + window_size;
                let query_local = first.local_timestamp + position_in_cycle;
                if query_local >= window_start && query_local < window_end {
                    return Some(OffsetInfo {
                        offset: prev.offset,           // post-jump offset
                        abbrev: abbrev(prev.abbrev_idx),
                        is_gap: true,
                        gap_size: off_diff,
                    });
                }
            }
        }

        if idx < cycle.len() {
            let nxt = &cycle[idx];
            let off_diff = (nxt.offset - prev.offset) as i64;
            if off_diff != 0 {
                let window_start = prev.local_timestamp;
                let window_size = off_diff.abs();
                let window_end = window_start + window_size;
                let query_local = first.local_timestamp + position_in_cycle;
                if query_local >= window_start && query_local < window_end {
                    if off_diff > 0 {
                        return Some(OffsetInfo {
                            offset: nxt.offset,        // post-jump offset
                            abbrev: abbrev(nxt.abbrev_idx),
                            is_gap: true,
                            gap_size: off_diff,
                        });
                    } else {
                        return Some(OffsetInfo {
                            offset: prev.offset,
                            abbrev: abbrev(prev.abbrev_idx),
                            is_gap: false,
                            gap_size: 0,
                        });
                    }
                }
            }
        }

        let t = &cycle[idx - 1];
        Some(OffsetInfo {
            offset: t.offset,
            abbrev: abbrev(t.abbrev_idx),
            is_gap: false,
            gap_size: 0,
        })
    } else {
        last_transition(transitions)
    }
}

/// Returns detailed offset information for the given IANA timezone
/// at the specified **local** Unix timestamp.
///
/// When `is_gap` is true, the requested local time does not exist
/// (spring-forward gap). Use `gap_size` to correctly shift the time forward
/// (add gap_size to the provisional local unix, then subtract the returned offset).
pub fn offset_info_at_local(name: &str, local_unix: i64) -> Option<OffsetInfo> {
    let (_, transitions, repeating_tail_start) = get_tz_data(name)?;
    let idx = transitions.partition_point(|t| t.local_timestamp <= local_unix);
    if idx == 0 {
        let t = &transitions[0];
        return Some(OffsetInfo {
            offset: t.offset,
            abbrev: abbrev(t.abbrev_idx),
            is_gap: false,
            gap_size: 0,
        });
    }
    if idx >= transitions.len() {
        return resolve_far_future_local(transitions, repeating_tail_start, local_unix);
    }

    let prev = &transitions[idx - 1];

    // detect gap right after a spring-forward transition that starts this segment
    if idx >= 2 {
        let pprev = &transitions[idx - 2];
        let off_diff = (prev.offset - pprev.offset) as i64;
        if off_diff > 0 {
            let window_start = prev.local_timestamp;
            let window_size = off_diff;
            let window_end = window_start + window_size;
            if local_unix >= window_start && local_unix < window_end {
                return Some(OffsetInfo {
                    offset: prev.offset,           // post-jump offset (EDT etc.)
                    abbrev: abbrev(prev.abbrev_idx),
                    is_gap: true,
                    gap_size: off_diff,
                });
            }
        }
    }

    // prev ↔ nxt check (for overlaps and any remaining cases)
    if idx < transitions.len() {
        let nxt = &transitions[idx];
        let off_diff = (nxt.offset - prev.offset) as i64;
        if off_diff != 0 {
            let window_start = prev.local_timestamp;
            let window_size = off_diff.abs();
            let window_end = window_start + window_size;
            if local_unix >= window_start && local_unix < window_end {
                if off_diff > 0 {
                    return Some(OffsetInfo {
                        offset: nxt.offset,        // post-jump offset
                        abbrev: abbrev(nxt.abbrev_idx),
                        is_gap: true,
                        gap_size: off_diff,
                    });
                } else {
                    return Some(OffsetInfo {
                        offset: prev.offset,
                        abbrev: abbrev(prev.abbrev_idx),
                        is_gap: false,
                        gap_size: 0,
                    });
                }
            }
        }
    }

    Some(OffsetInfo {
        offset: prev.offset,
        abbrev: abbrev(prev.abbrev_idx),
        is_gap: false,
        gap_size: 0,
    })
}
"#,
    );

    // === DATA_N arrays (only when `tz` feature is enabled) ===
    for (trans, &id) in &unique {
        let name = &data_names[id];
        output.push_str("#[cfg(feature = \"tz\")]\n");
        output.push_str(&format!("static {}: &[Transition] = &[\n", name));
        for t in trans {
            output.push_str(&format!(
                "    Transition {{ local_timestamp: {}, offset: {}, abbrev_idx: {} }},\n",
                t.local_timestamp, t.offset, t.abbrev_idx
            ));
        }
        output.push_str("];\n\n");
    }

    // === DATA_0 for minimal mode (when `tz` feature is disabled) ===
    // We reuse DATA_0 under the opposite cfg so that TZ_ENTRIES can reference
    // a DATA_N name in both modes. The two definitions are mutually exclusive.
    let utc_trans = if let Some((trans, _)) = name_to_data.get("UTC") {
        trans.clone()
    } else if let Some((trans, _)) = name_to_data.get("Etc/UTC") {
        trans.clone()
    } else {
        vec![]
    };

    output.push_str("#[cfg(not(feature = \"tz\"))]\n");
    output.push_str("static DATA_0: &[Transition] = &[\n");
    for t in &utc_trans {
        output.push_str(&format!(
            "    Transition {{ local_timestamp: {}, offset: {}, abbrev_idx: {} }},\n",
            t.local_timestamp, t.offset, t.abbrev_idx
        ));
    }
    output.push_str("];\n\n");

    // === TZ_ENTRIES (single name, conditionally compiled) ===
    // Full version
    output.push_str("#[cfg(feature = \"tz\")]\n");
    output.push_str(
        "pub(crate) static TZ_ENTRIES: &[(&str, &'static [Transition], Option<usize>)] = &[\n",
    );
    for (name, data_name, repeating_start) in &entries {
        let start_str = match repeating_start {
            Some(idx) => format!("Some({})", idx),
            None => "None".to_string(),
        };
        output.push_str(&format!(
            "    (\"{}\", {}, {}),\n",
            name, data_name, start_str
        ));
    }
    output.push_str("];\n\n");

    // Minimal version (reuses DATA_0 so everything stays DATA_N / TZ_ENTRIES)
    output.push_str("#[cfg(not(feature = \"tz\"))]\n");
    output.push_str(
        "pub(crate) static TZ_ENTRIES: &[(&str, &'static [Transition], Option<usize>)] = &[\n",
    );
    for (name, _data_name, repeating_start) in &minimal_entries {
        let start_str = match repeating_start {
            Some(idx) => format!("Some({})", idx),
            None => "None".to_string(),
        };
        output.push_str(&format!("    (\"{}\", DATA_0, {}),\n", name, start_str));
    }
    output.push_str("];\n");

    fs::write("src/tzdb.rs", output).unwrap();

    // Debug
    if let Some((trans, _)) = name_to_data.get("Africa/Accra") {
        println!("DEBUG: Africa/Accra has {} transitions", trans.len());
    }
    if let Some((trans, _)) = name_to_data.get("America/New_York") {
        println!("DEBUG: America/New_York has {} transitions", trans.len());
    }
    if let Some((trans, _)) = name_to_data.get("Europe/London") {
        println!("DEBUG: Europe/London has {} transitions", trans.len());
    }

    println!(
        "✅ Generated src/tzdb.rs (version {}) with {} zones ({} unique tables, {} abbreviations)",
        version,
        entries.len(),
        unique.len(),
        abbrevs.len()
    );
    println!(
        "   Minimal mode: {} zones → all point to DATA_0 (UTC-equivalent)",
        minimal_entries.len()
    );
}
