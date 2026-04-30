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

    // === NEW: Extract version from directory name ===
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

    // === NEW: Collect all unique abbreviations first ===
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
                offset: first.total_offset() as i32,
                is_dst: first.dst_offset != 0,
                abbrev_idx: first_idx,
            });

            for (ts, ft) in &set.rest {
                let idx = *abbrev_to_idx.get(&ft.name).unwrap();
                transitions.push(Transition {
                    timestamp: *ts,
                    offset: ft.total_offset() as i32,
                    is_dst: ft.dst_offset != 0,
                    abbrev_idx: idx,
                });
            }

            // === Reliably detect perpetual repeating rules (unchanged from original) ===
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

    // Deduplication (unchanged)
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
        "// This file is auto-generated by tzdb-generator.\n\
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

    // Updated struct
    output.push_str("#[derive(Debug, Clone, Copy, PartialEq, Eq)]\n");
    output.push_str(
        "pub struct Transition { pub timestamp: i64, pub offset: i32, pub is_dst: bool, pub abbrev_idx: u16 }\n\n",
    );

    output.push_str("#[derive(Debug, Clone, Copy)]\n");
    output.push_str("pub struct OffsetInfo {\n");
    output.push_str("    pub offset: i32,\n");
    output.push_str("    pub is_dst: bool,\n");
    output.push_str("    pub abbrev: &'static str,\n");
    output.push_str("}\n\n");

    // get_tz_data (uses whichever TZ_ENTRIES the cfg selected)
    output.push_str("pub fn get_tz_data(name: &str) -> Option<(&str, &'static [Transition], Option<usize>)> {\n");
    output.push_str("    let idx = TZ_ENTRIES.partition_point(|(n, _, _)| *n < name);\n");
    output.push_str("    if idx < TZ_ENTRIES.len() && TZ_ENTRIES[idx].0 == name {\n");
    output.push_str("        Some(TZ_ENTRIES[idx])\n");
    output.push_str("    } else { None }\n");
    output.push_str("}\n\n");

    // abbrev helper
    output.push_str("#[inline(always)]\n");
    output.push_str("pub fn abbrev(idx: u16) -> &'static str {\n");
    output.push_str("    ABBREVS[idx as usize]\n");
    output.push_str("}\n\n");

    // resolve_far_future
    output.push_str("#[inline]\n");
    output.push_str("fn resolve_far_future(\n");
    output.push_str("    transitions: &[Transition],\n");
    output.push_str("    repeating_tail_start: Option<usize>,\n");
    output.push_str("    unix_timestamp: i64,\n");
    output.push_str(") -> Option<OffsetInfo> {\n");
    output.push_str("    if let Some(start) = repeating_tail_start {\n");
    output.push_str("        let cycle = &transitions[start..];\n");
    output.push_str("        if cycle.len() < 2 {\n");
    output.push_str("            return last_transition(transitions);\n");
    output.push_str("        }\n");
    output.push_str("        let first = &cycle[0];\n");
    output.push_str("        let last = &cycle[cycle.len() - 1];\n");
    output.push_str("        let period = last.timestamp - first.timestamp;\n");
    output.push_str("        if period <= 0 {\n");
    output.push_str("            return last_transition(transitions);\n");
    output.push_str("        }\n");
    output.push_str("        let elapsed = unix_timestamp - first.timestamp;\n");
    output.push_str("        if elapsed < 0 {\n");
    output.push_str("            return last_transition(transitions);\n");
    output.push_str("        }\n");
    output.push_str("        let position_in_cycle = elapsed % period;\n");
    output.push_str("        let idx = cycle.partition_point(|t| {\n");
    output.push_str("            (t.timestamp - first.timestamp) <= position_in_cycle\n");
    output.push_str("        });\n");
    output.push_str("        let chosen = if idx == 0 {\n");
    output.push_str("            &cycle[0]\n");
    output.push_str("        } else {\n");
    output.push_str("            &cycle[idx - 1]\n");
    output.push_str("        };\n");
    output.push_str("        Some(OffsetInfo {\n");
    output.push_str("            offset: chosen.offset,\n");
    output.push_str("            is_dst: chosen.is_dst,\n");
    output.push_str("            abbrev: abbrev(chosen.abbrev_idx),\n");
    output.push_str("        })\n");
    output.push_str("    } else {\n");
    output.push_str("        last_transition(transitions)\n");
    output.push_str("    }\n");
    output.push_str("}\n\n");

    // last_transition
    output.push_str("#[inline(always)]\n");
    output.push_str("fn last_transition(transitions: &[Transition]) -> Option<OffsetInfo> {\n");
    output.push_str("    transitions.last().map(|t| OffsetInfo {\n");
    output.push_str("        offset: t.offset,\n");
    output.push_str("        is_dst: t.is_dst,\n");
    output.push_str("        abbrev: abbrev(t.abbrev_idx),\n");
    output.push_str("    })\n");
    output.push_str("}\n\n");

    // offset_at
    output.push_str("/// Returns the UTC offset (in seconds) for the given IANA timezone\n");
    output
        .push_str("/// at the specified Unix timestamp. Returns `None` if the zone is unknown.\n");
    output.push_str("pub fn offset_at(name: &str, unix_timestamp: i64) -> Option<i32> {\n");
    output.push_str("    let (_, transitions, repeating_tail_start) = get_tz_data(name)?;\n");
    output.push_str(
        "    let idx = transitions.partition_point(|t| t.timestamp <= unix_timestamp);\n",
    );
    output.push_str("    if idx < transitions.len() {\n");
    output.push_str("        if idx == 0 {\n");
    output.push_str("            Some(transitions[0].offset)\n");
    output.push_str("        } else {\n");
    output.push_str("            Some(transitions[idx - 1].offset)\n");
    output.push_str("        }\n");
    output.push_str("    } else {\n");
    output.push_str(
        "        resolve_far_future(transitions, repeating_tail_start, unix_timestamp)\n",
    );
    output.push_str("            .map(|info| info.offset)\n");
    output.push_str("    }\n");
    output.push_str("}\n\n");

    // offset_info_at
    output.push_str("/// Returns detailed offset information for the given IANA timezone\n");
    output.push_str("/// at the specified Unix timestamp.\n");
    output.push_str(
        "pub fn offset_info_at(name: &str, unix_timestamp: i64) -> Option<OffsetInfo> {\n",
    );
    output.push_str("    let (_, transitions, repeating_tail_start) = get_tz_data(name)?;\n");
    output.push_str(
        "    let idx = transitions.partition_point(|t| t.timestamp <= unix_timestamp);\n",
    );
    output.push_str("    if idx < transitions.len() {\n");
    output.push_str("        let t = if idx == 0 {\n");
    output.push_str("            &transitions[0]\n");
    output.push_str("        } else {\n");
    output.push_str("            &transitions[idx - 1]\n");
    output.push_str("        };\n");
    output.push_str("        Some(OffsetInfo {\n");
    output.push_str("            offset: t.offset,\n");
    output.push_str("            is_dst: t.is_dst,\n");
    output.push_str("            abbrev: abbrev(t.abbrev_idx),\n");
    output.push_str("        })\n");
    output.push_str("    } else {\n");
    output.push_str(
        "        resolve_far_future(transitions, repeating_tail_start, unix_timestamp)\n",
    );
    output.push_str("    }\n");
    output.push_str("}\n\n");

    // === DATA_N arrays (only when `tz` feature is enabled) ===
    for (trans, &id) in &unique {
        let name = &data_names[id];
        output.push_str("#[cfg(feature = \"tz\")]\n");
        output.push_str(&format!("static {}: &[Transition] = &[\n", name));
        for t in trans {
            output.push_str(&format!(
                "    Transition {{ timestamp: {}, offset: {}, is_dst: {}, abbrev_idx: {} }},\n",
                t.timestamp, t.offset, t.is_dst, t.abbrev_idx
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
            "    Transition {{ timestamp: {}, offset: {}, is_dst: {}, abbrev_idx: {} }},\n",
            t.timestamp, t.offset, t.is_dst, t.abbrev_idx
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
