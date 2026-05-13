//! TZDB code generator
//! Place the tz data folder in your project root
//! then in a terminal do: cargo run -- <your tz data folder name>
//! for example:
//! cargo run -- tzdata2026a

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repeating {
    None,
    Cycle {
        start: usize,
        len: usize,
        period: i64,
    },
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

    let mut name_to_data: HashMap<String, (Vec<Transition>, Repeating)> = HashMap::new();

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

            // === detect repeating behavior intelligently ===
            let repeating = {
                // 1. Metadata check: does this zone have a perpetual rule?
                let has_perpetual_rule = if let Some(zoneset) = table.get_zoneset(name) {
                    if let Some(last_zone) = zoneset.last() {
                        if let Saving::Multiple(ref rules_name) = last_zone.saving {
                            if let Some(rules) = table.rulesets.get(rules_name) {
                                rules
                                    .iter()
                                    .any(|r| matches!(r.to_year, Some(Year::Maximum) | None))
                            } else {
                                false
                            }
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else {
                    false
                };

                let last_ts = transitions.last().map(|t| t.local_timestamp).unwrap_or(0);
                const REPEATING_TAIL_CUTOFF: i64 = 2_208_988_800; // ~2040-01-01

                const VALIDATION_WINDOW: usize = 8;

                if !has_perpetual_rule
                    || last_ts <= REPEATING_TAIL_CUTOFF
                    || transitions.len() < VALIDATION_WINDOW
                {
                    Repeating::None
                } else {
                    // Use the last 8 transitions purely for validation.
                    // We require a stable pattern across three consecutive periods.
                    // This is intentionally conservative.
                    let validation_start = transitions.len().saturating_sub(VALIDATION_WINDOW);

                    // Compute three consecutive periods inside the validation window.
                    let p0 = transitions[validation_start + 2].local_timestamp
                        - transitions[validation_start].local_timestamp;
                    let p1 = transitions[validation_start + 4].local_timestamp
                        - transitions[validation_start + 2].local_timestamp;
                    let p2 = transitions[validation_start + 6].local_timestamp
                        - transitions[validation_start + 4].local_timestamp;

                    // Only accept if the pattern is stable and the period is reasonable.
                    if p0 > 0 && p0 == p1 && p1 == p2 && p0 >= 2_592_000 && p0 <= 34_560_000 {
                        // We have a stable period. Now compute the actual repeating
                        // unit length by looking at the offset sequence in the
                        // validation window. We prefer the smallest k (starting from 2)
                        // such that the offsets repeat every k steps.
                        let window_offsets: Vec<i32> = (0..VALIDATION_WINDOW)
                            .map(|i| transitions[validation_start + i].offset)
                            .collect();

                        let mut cycle_len = VALIDATION_WINDOW; // safe fallback

                        for candidate in 2..=VALIDATION_WINDOW {
                            let mut repeats = true;

                            for pos in 0..candidate {
                                let first = window_offsets[pos];
                                for j in (pos + candidate..VALIDATION_WINDOW).step_by(candidate) {
                                    if window_offsets[j] != first {
                                        repeats = false;
                                        break;
                                    }
                                }
                                if !repeats {
                                    break;
                                }
                            }

                            if repeats {
                                cycle_len = candidate;
                                break;
                            }
                        }

                        Repeating::Cycle {
                            start: validation_start,
                            len: cycle_len,
                            period: p0,
                        }
                    } else {
                        // Pattern is not cleanly repeating or period is unreasonable
                        // → conservative fallback.
                        Repeating::None
                    }
                }
            };

            // Optimization for binary size + cleanliness:
            // If we detected a stable repeating cycle, we keep historical transitions
            // + ONE copy of the actual repeating unit (whose length we compute from
            // the validated window). `len` now reflects the real cycle size rather
            // than the validation window. This keeps DATA_N small while preserving
            // full fidelity.
            let transitions = if let Repeating::Cycle { start, len, .. } = repeating {
                let keep_up_to = start + len;
                if keep_up_to < transitions.len() {
                    transitions.truncate(keep_up_to);
                }
                transitions
            } else {
                transitions
            };

            name_to_data.insert(name.to_string(), (transitions, repeating));
        }
    }

    // === Deduplication (only on transition data – Repeating info is zone-specific) ===
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

    // === Build entries with the new Repeating info ===
    let mut entries: Vec<(String, String, Repeating)> = Vec::new();

    for (name, (trans, repeating)) in &name_to_data {
        if let Some(&id) = unique.get(trans) {
            entries.push((name.clone(), data_names[id].clone(), *repeating));
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

    let minimal_entries: Vec<(String, String, Repeating)> = entries
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

    output.push_str(&format!(
        "//! This module is auto-generated from the IANA Time Zone Database\n\
     //! found at: https://www.iana.org/time-zones\n\
     //! Source directory: {}\n\
     //! It provides both a minimal mode (UTC + identical zones only) and a full\n\
     //! mode (`tz` feature) which has full historical transitions.\n
     //! Generator source: https://github.com/ragardner/tz-info-generator-rs\n\n",
        dir_name
    ));

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

    output.push_str(
        r#"#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transition {
    pub local_timestamp: i64,
    pub offset: i32,
    pub abbrev_idx: u16,
}

/// Repeating describes how (or if) the transition pattern continues
/// indefinitely after the last explicit entry in the `transitions` slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repeating {
    None,
    /// The pattern repeats every `period` seconds starting at index `start`.
    /// `len` is the number of entries in one repeating unit (usually 2).
    Cycle {
        start: usize,
        len: usize,
        period: i64,
    },
}

/// Returns the transition data for the given IANA timezone name.
///
/// Returns `Some((name, transitions, repeating))` or `None` if unknown.
/// `repeating` indicates whether (and how) the transition pattern repeats
/// indefinitely for zones with perpetual DST rules.
///
/// For zones with `Repeating::Cycle`, the `transitions` slice contains
/// only historical data + **one copy** of the repeating cycle (not many
/// repetitions). The `Cycle` variant tells the runtime how to use it.
#[inline]
pub fn get_tz_data(name: &str) -> Option<(&str, &'static [Transition], Repeating)> {
    let idx = TZ_ENTRIES.partition_point(|(n, _, _)| *n < name);
    if idx < TZ_ENTRIES.len() && TZ_ENTRIES[idx].0 == name {
        Some(TZ_ENTRIES[idx])
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy)]
pub struct OffsetInfo {
    /// The offset from UTC in seconds (positive = east of UTC).
    pub offset: i32,
    /// Time zone abbreviation in effect (e.g. "EDT", "GMT").
    pub abbrev: &'static str,
    /// Whether the requested local time falls in a gap (spring-forward).
    pub is_gap: bool,
    /// Size of the gap in seconds (only meaningful when `is_gap == true`).
    pub gap_size: i64,
}

/// Returns the abbreviation string for the given index into `ABBREVS`.
#[inline]
pub fn abbrev(idx: u16) -> &'static str {
    ABBREVS[idx as usize]
}

#[inline]
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
    repeating: Repeating,
    local_unix: i64,
) -> Option<OffsetInfo> {
    // We use the pre-validated `period` captured from Repeating::Cycle.
    // The transitions slice only contains historical data + one copy of
    // the repeating cycle (thanks to truncation at code generation time).
    // This keeps binary size small while preserving full fidelity.
    let (start, len, period) = match repeating {
        Repeating::Cycle { start, len, period } => (start, len, period),
        Repeating::None => return last_transition(transitions),
    };

    // Defensive bounds check (should never trigger for well-formed data)
    if start + len > transitions.len() || len < 2 {
        return last_transition(transitions);
    }

    let cycle = &transitions[start..start + len];
    let first = &cycle[0];
    if first.local_timestamp == i64::MIN {
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
                    offset: prev.offset,
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
            let window_size = off_diff.saturating_abs();
            let window_end = window_start + window_size;
            let query_local = first.local_timestamp + position_in_cycle;
            if query_local >= window_start && query_local < window_end {
                if off_diff > 0 {
                    return Some(OffsetInfo {
                        offset: nxt.offset,
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
}

/// Returns offset information for an IANA timezone at the given **local** Unix time.
///
/// If the local time falls in a gap (spring-forward), `is_gap` is `true` and
/// `gap_size` contains the number of skipped seconds. Add `gap_size` to the
/// original local time and re-query to obtain a valid instant.
pub fn offset_info_at_local(name: &str, local_unix: i64) -> Option<OffsetInfo> {
    let (_, transitions, repeating) = get_tz_data(name)?;
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
        return resolve_far_future_local(transitions, repeating, local_unix);
    }

    let prev = &transitions[idx - 1];

    if idx >= 2 {
        let pprev = &transitions[idx - 2];
        let off_diff = (prev.offset - pprev.offset) as i64;
        if off_diff > 0 {
            let window_start = prev.local_timestamp;
            let window_size = off_diff;
            let window_end = window_start + window_size;
            if local_unix >= window_start && local_unix < window_end {
                return Some(OffsetInfo {
                    offset: prev.offset,
                    abbrev: abbrev(prev.abbrev_idx),
                    is_gap: true,
                    gap_size: off_diff,
                });
            }
        }
    }

    if idx < transitions.len() {
        let nxt = &transitions[idx];
        let off_diff = (nxt.offset - prev.offset) as i64;
        if off_diff != 0 {
            let window_start = prev.local_timestamp;
            let window_size = off_diff.saturating_abs();
            let window_end = window_start + window_size;
            if local_unix >= window_start && local_unix < window_end {
                if off_diff > 0 {
                    return Some(OffsetInfo {
                        offset: nxt.offset,
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

/// Computes the UTC instant at which the transition at `idx` occurs.
///
/// For `idx == 0` this returns `i64::MIN`. For `idx >= 1` it is derived as
/// `local_timestamp[idx] - offset[idx-1]`.
#[inline]
fn transition_utc(transitions: &[Transition], idx: usize) -> i64 {
    if idx == 0 {
        i64::MIN
    } else {
        transitions[idx].local_timestamp - transitions[idx - 1].offset as i64
    }
}

/// Binary search for the last transition whose UTC time is ≤ `utc_unix`.
#[inline]
fn find_transition_for_utc(transitions: &[Transition], utc_unix: i64) -> usize {
    let mut lo = 0usize;
    let mut hi = transitions.len();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if transition_utc(transitions, mid) <= utc_unix {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    if lo == 0 { 0 } else { lo - 1 }
}

fn resolve_far_future_utc(
    transitions: &[Transition],
    repeating: Repeating,
    utc_unix: i64,
) -> Option<OffsetInfo> {
    // We use the pre-validated `period` captured from Repeating::Cycle.
    // This avoids recalculation and stays consistent with generator-time validation.
    let (start, len, period) = match repeating {
        Repeating::Cycle { start, len, period } => (start, len, period),
        Repeating::None => return last_transition(transitions),
    };

    // Defensive bounds check (should never trigger for well-formed data)
    if start + len > transitions.len() || len < 2 {
        return last_transition(transitions);
    }

    let cycle = &transitions[start..start + len];
    let first_t = transition_utc(transitions, start);
    if first_t == i64::MIN {
        return last_transition(transitions);
    }
    let elapsed = utc_unix - first_t;
    if elapsed < 0 {
        return last_transition(transitions);
    }
    let position_in_cycle = elapsed % period;

    let mut lo = 0usize;
    let mut hi = cycle.len();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let t_mid = transition_utc(transitions, start + mid);
        if (t_mid - first_t) <= position_in_cycle {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let best_j = if lo == 0 { 0 } else { lo - 1 };

    let t = &cycle[best_j];
    Some(OffsetInfo {
        offset: t.offset,
        abbrev: abbrev(t.abbrev_idx),
        is_gap: false,
        gap_size: 0,
    })
}

/// Returns offset information for an IANA timezone at the given **UTC** Unix time.
///
/// `is_gap` is always `false` because gaps are a local-time concept only.
/// Every UTC instant has exactly one well-defined offset.
pub fn offset_info_at_utc(name: &str, utc_unix: i64) -> Option<OffsetInfo> {
    let (_, transitions, repeating) = get_tz_data(name)?;
    if transitions.is_empty() {
        return None;
    }

    let idx = find_transition_for_utc(transitions, utc_unix);

    let last_idx = transitions.len() - 1;
    let last_t_utc = transition_utc(transitions, last_idx);
    if utc_unix > last_t_utc {
        if let Repeating::Cycle { .. } = repeating {
            return resolve_far_future_utc(transitions, repeating, utc_unix);
        }
    }

    let t = &transitions[idx];
    Some(OffsetInfo {
        offset: t.offset,
        abbrev: abbrev(t.abbrev_idx),
        is_gap: false,
        gap_size: 0,
    })
}

/// Returns the offset (in seconds) for an IANA timezone at the given UTC Unix time.
#[inline]
pub fn offset_at_utc(name: &str, utc_unix: i64) -> Option<i32> {
    offset_info_at_utc(name, utc_unix).map(|info| info.offset)
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
        "pub(crate) static TZ_ENTRIES: &[(&str, &'static [Transition], Repeating)] = &[\n",
    );
    for (name, data_name, repeating) in &entries {
        let repeating_str = match repeating {
            Repeating::None => "Repeating::None".to_string(),
            Repeating::Cycle { start, len, period } => format!(
                "Repeating::Cycle {{ start: {}, len: {}, period: {} }}",
                start, len, period
            ),
        };
        output.push_str(&format!(
            "    (\"{}\", {}, {}),\n",
            name, data_name, repeating_str
        ));
    }
    output.push_str("];\n\n");

    // Minimal version (reuses DATA_0 so everything stays DATA_N / TZ_ENTRIES)
    output.push_str("#[cfg(not(feature = \"tz\"))]\n");
    output.push_str(
        "pub(crate) static TZ_ENTRIES: &[(&str, &'static [Transition], Repeating)] = &[\n",
    );
    for (name, _data_name, repeating) in &minimal_entries {
        let repeating_str = match repeating {
            Repeating::None => "Repeating::None".to_string(),
            Repeating::Cycle { start, len, period } => format!(
                "Repeating::Cycle {{ start: {}, len: {}, period: {} }}",
                start, len, period
            ),
        };
        output.push_str(&format!("    (\"{}\", DATA_0, {}),\n", name, repeating_str));
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
