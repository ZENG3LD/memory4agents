use crate::error::CoreError;
use crate::types::{SearchHit, Symbol, SymbolKind, Visibility};
use rusqlite::Connection;
use std::collections::HashSet;

/// Compute 3-character trigrams for a string (lowercased, padded with spaces).
pub fn trigrams(s: &str) -> HashSet<String> {
    let lower = s.to_lowercase();
    // Pad short strings to always produce at least one trigram.
    let padded = if lower.len() < 3 {
        format!("  {lower} ")
    } else {
        format!(" {lower} ")
    };
    let chars: Vec<char> = padded.chars().collect();
    let mut result = HashSet::new();
    for window in chars.windows(3) {
        result.insert(window.iter().collect::<String>());
    }
    result
}

/// Jaccard similarity coefficient between two strings' trigram sets.
pub fn trigram_score(query: &str, candidate: &str) -> f32 {
    let q = trigrams(query);
    let c = trigrams(candidate);
    let intersection = q.intersection(&c).count();
    let union = q.union(&c).count();
    if union == 0 {
        return 0.0;
    }
    intersection as f32 / union as f32
}

/// Fuzzy search for symbols by name using trigram index.
///
/// Returns up to `limit` hits with score >= `threshold`, sorted by score descending.
pub fn fuzzy_search(
    conn: &Connection,
    workspace_id: Option<&str>,
    query: &str,
    limit: u32,
    threshold: f32,
) -> Result<Vec<SearchHit>, CoreError> {
    let q_trigrams = trigrams(query);
    if q_trigrams.is_empty() {
        return Ok(Vec::new());
    }

    // Build the IN clause placeholders.
    let placeholders: Vec<String> = (1..=q_trigrams.len()).map(|i| format!("?{i}")).collect();
    let in_clause = placeholders.join(", ");

    let sql = format!(
        "SELECT DISTINCT symbol_id FROM symbol_trigrams WHERE trigram IN ({in_clause})"
    );

    let tgram_list: Vec<String> = q_trigrams.into_iter().collect();

    let mut stmt = conn.prepare(&sql)?;
    let candidate_ids: Vec<i64> = stmt
        .query_map(rusqlite::params_from_iter(tgram_list.iter()), |row| {
            row.get(0)
        })?
        .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("[dig2memory] row read error: {e}");
                None
            }
        })
        .collect();

    // Load symbols for candidates and score them.
    let mut hits: Vec<SearchHit> = Vec::new();

    for sym_id in candidate_ids {
        let sym = match load_symbol_by_id(conn, sym_id, workspace_id)? {
            Some(s) => s,
            None => continue,
        };

        let score = trigram_score(query, &sym.name);
        if score >= threshold {
            hits.push(SearchHit { symbol: sym, score });
        }
    }

    // Sort by score descending.
    hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    hits.truncate(limit as usize);

    Ok(hits)
}

fn load_symbol_by_id(
    conn: &Connection,
    id: i64,
    workspace_id: Option<&str>,
) -> Result<Option<Symbol>, CoreError> {
    let result = match workspace_id {
        Some(ws) => {
            let mut stmt = conn.prepare(
                "SELECT id, workspace_id, file_path, name, kind, visibility, line, col, parent_name, signature, body_hash, shape_fingerprint, shape_fingerprint_loose, field_count, shape_summary, callsite_count, type_use_count
                 FROM symbols WHERE id = ?1 AND workspace_id = ?2",
            )?;
            let x = stmt
                .query_map(rusqlite::params![id, ws], row_to_symbol)?
                .next()
                .transpose()?;
            x
        }
        None => {
            let mut stmt = conn.prepare(
                "SELECT id, workspace_id, file_path, name, kind, visibility, line, col, parent_name, signature, body_hash, shape_fingerprint, shape_fingerprint_loose, field_count, shape_summary, callsite_count, type_use_count
                 FROM symbols WHERE id = ?1",
            )?;
            let x = stmt
                .query_map(rusqlite::params![id], row_to_symbol)?
                .next()
                .transpose()?;
            x
        }
    };
    Ok(result)
}

fn row_to_symbol(row: &rusqlite::Row<'_>) -> rusqlite::Result<Symbol> {
    let kind_str: String = row.get(4)?;
    let vis_str: String = row.get(5)?;
    Ok(Symbol {
        id: row.get(0)?,
        workspace_id: row.get(1)?,
        file_path: row.get(2)?,
        name: row.get(3)?,
        kind: SymbolKind::from_str(&kind_str).unwrap_or_else(|| {
            tracing::warn!("[dig2memory] unknown SymbolKind: {kind_str}");
            SymbolKind::Function
        }),
        visibility: Visibility::from_str(&vis_str).unwrap_or_else(|| {
            tracing::warn!("[dig2memory] unknown Visibility: {vis_str}");
            Visibility::Private
        }),
        line: row.get(6)?,
        col: row.get(7)?,
        parent_name: row.get(8)?,
        signature: row.get(9)?,
        body_hash: row.get(10)?,
        shape_fingerprint: row.get(11).unwrap_or(None),
        shape_fingerprint_loose: row.get(12).unwrap_or(None),
        field_count: row.get::<_, Option<i64>>(13).unwrap_or(None).map(|v| v as u32),
        shape_summary: row.get(14).unwrap_or(None),
        callsite_count: row.get::<_, Option<i64>>(15).unwrap_or(None).map(|v| v as u32).unwrap_or(0),
        type_use_count: row.get::<_, Option<i64>>(16).unwrap_or(None).map(|v| v as u32).unwrap_or(0),
    })
}

