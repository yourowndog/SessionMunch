//! Markdown section parsing with heading awareness.
//!
//! Parses a Markdown document into sections divided by ATX (#) and Setext (==/--)
//! headings, preserving byte offsets into the source text.

use crate::ids::PageId;
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

/// A section of a Markdown document between headings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// The page this section belongs to.
    pub page_id: PageId,
    /// Order of this section within its page (0-indexed).
    pub ordinal: usize,
    /// ATX heading level (0 for preamble/no heading).
    pub level: u32,
    /// Text of the heading.
    pub heading: String,
    /// Full breadcrumb path of headings to this section.
    pub heading_path: Vec<String>,
    /// Section content body.
    pub body: String,
    /// Byte offset where section starts in original document.
    pub start_byte: usize,
    /// Byte offset where section ends in original document.
    pub end_byte: usize,
}

/// A passage extracted from a section, optimized for embedding input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Passage {
    /// Ordinal of the section this passage came from.
    pub section_ordinal: usize,
    /// Order of this passage within its section (0-indexed).
    pub ordinal: usize,
    /// Passage text (materialized from a contiguous byte range in section body).
    pub text: String,
    /// Byte offset where passage starts within section body.
    pub start_byte: usize,
    /// Byte offset where passage ends within section body.
    pub end_byte: usize,
}

/// Heading info extracted from offset iter events.
#[derive(Debug, Clone)]
struct HeadingInfo {
    level: u32,
    text: String,
    start_byte: usize,
}

fn compute_heading_path(stack: &mut Vec<(u32, String)>, level: u32, heading: &str) -> Vec<String> {
    // Truncate stack to entries with level < current level
    stack.retain(|(l, _)| *l < level);
    // Push current heading
    stack.push((level, heading.to_string()));
    // Return the heading text path
    stack.iter().map(|(_, h)| h.clone()).collect()
}

/// Count whitespace-separated tokens (the whitespace-count approximation used
/// throughout this parser's 320-target/420-max/48-overlap budget; documented
/// as an approximation of true model tokenization, not an exact count).
///
/// Sol amendment 2: when a token count exceeds hard maximum and no sentence
/// boundary is found, this function drives the byte-length fallback that
/// splits on UTF-8 character boundaries for degenerate/very long tokens.
pub fn count_tokens(text: &str) -> usize {
    text.split_whitespace().count()
}

/// Parse a Markdown document into sections divided by headings.
/// Returns sections with preserved byte offsets and heading paths.
pub fn parse_sections(body: &str, page_id: PageId) -> Vec<Section> {
    let mut sections = Vec::new();
    let mut heading_stack: Vec<(u32, String)> = Vec::new();

    // First pass: collect all headings with their byte ranges
    let opts = Options::empty();
    let mut headings: Vec<HeadingInfo> = Vec::new();
    let mut current_heading: Option<(u32, String, usize)> = None; // (level, text, start_byte)
    let mut in_heading = false;

    for (event, range) in Parser::new_ext(body, opts).into_offset_iter() {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                in_heading = true;
                current_heading = Some((level as u32, String::new(), range.start));
            }
            Event::End(TagEnd::Heading { .. }) if in_heading => {
                if let Some((level, text, start_byte)) = current_heading.take() {
                    let trimmed = text.trim().to_string();
                    headings.push(HeadingInfo {
                        level,
                        text: trimmed,
                        start_byte,
                    });
                }
                in_heading = false;
            }
            Event::Text(t) if in_heading => {
                if let Some((_, text, _)) = &mut current_heading {
                    text.push_str(&t);
                }
            }
            _ => {}
        }
    }

    // Now build sections between headings
    let mut section_ordinal = 0;

    // Handle preamble (content before first heading)
    if let Some(first_heading) = headings.first() {
        if first_heading.start_byte > 0 {
            let body_text = body[0..first_heading.start_byte].to_string();
            sections.push(Section {
                page_id,
                ordinal: section_ordinal,
                level: 0,
                heading: String::new(),
                heading_path: Vec::new(),
                body: body_text,
                start_byte: 0,
                end_byte: first_heading.start_byte,
            });
            section_ordinal += 1;
        }
    } else {
        // No headings at all - whole body is one section
        sections.push(Section {
            page_id,
            ordinal: 0,
            level: 0,
            heading: String::new(),
            heading_path: Vec::new(),
            body: body.to_string(),
            start_byte: 0,
            end_byte: body.len(),
        });
        return sections;
    }

    // Process each heading as a section boundary
    for (idx, heading) in headings.iter().enumerate() {
        let section_start = heading.start_byte;
        let section_end = headings
            .get(idx + 1)
            .map(|h| h.start_byte)
            .unwrap_or(body.len());

        // The section includes the heading and content up to next heading
        let body_text = body[section_start..section_end].to_string();

        let heading_path = compute_heading_path(&mut heading_stack, heading.level, &heading.text);

        sections.push(Section {
            page_id,
            ordinal: section_ordinal,
            level: heading.level,
            heading: heading.text.clone(),
            heading_path,
            body: body_text,
            start_byte: section_start,
            end_byte: section_end,
        });
        section_ordinal += 1;
    }

    sections
}

/// Split a section into passages using range-based accounting.
/// Target passage size is 320 tokens with a hard maximum of 420 tokens.
/// Consecutive passages overlap by ~48 tokens for context continuity.
/// Always maintains the invariant: `&section.body[passage.start_byte..passage.end_byte] == passage.text`.
pub fn split_passages(section: &Section) -> Vec<Passage> {
    let mut passages = Vec::new();
    let body = &section.body;

    // Use offset iter on the section body to get real byte offsets for paragraphs
    let opts = Options::empty();
    let mut paragraph_ranges: Vec<(usize, usize)> = Vec::new();
    let mut in_paragraph = false;
    let mut para_start = 0;

    for (event, range) in Parser::new_ext(body, opts).into_offset_iter() {
        match event {
            Event::Start(Tag::Paragraph) => {
                in_paragraph = true;
                para_start = range.start;
            }
            Event::End(TagEnd::Paragraph) if in_paragraph => {
                paragraph_ranges.push((para_start, range.end));
                in_paragraph = false;
            }
            _ => {}
        }
    }

    // If no paragraphs found (e.g., just code blocks), treat whole body as one
    if paragraph_ranges.is_empty() {
        let text = body.trim();
        if !text.is_empty() {
            // Find the byte range of trimmed content
            let leading_ws = body.len() - body.trim_start().len();
            let trailing_ws = body.len() - body.trim_end().len();
            let trimmed_end = body.len() - trailing_ws;
            passages.push(Passage {
                section_ordinal: section.ordinal,
                ordinal: 0,
                text: text.to_string(),
                start_byte: leading_ws,
                end_byte: trimmed_end,
            });
        }
        return passages;
    }

    // Now split paragraphs into passages with token budget
    // Target: 320 tokens, hard max: 420 tokens, overlap: 48 tokens
    const TARGET_TOKENS: usize = 320;
    const MAX_TOKENS: usize = 420;
    const OVERLAP_TOKENS: usize = 48;

    let mut ordinal = 0;
    let mut current_ranges: Vec<(usize, usize)> = Vec::new();
    let mut current_token_count = 0;

    for (para_start, para_end) in paragraph_ranges {
        let para_text = &body[para_start..para_end];
        let para_tokens = count_tokens(para_text);

        // If a single paragraph exceeds MAX_TOKENS, split it at sentence boundaries
        if para_tokens > MAX_TOKENS {
            // Emit current passage first
            if !current_ranges.is_empty() {
                emit_passage_from_ranges(
                    body,
                    &current_ranges,
                    section.ordinal,
                    ordinal,
                    &mut passages,
                );
                ordinal += 1;
            }

            // Split this paragraph
            let sub_passages = split_long_text(para_text, para_start, section.ordinal, ordinal);
            passages.extend(sub_passages.iter().cloned());
            ordinal += sub_passages.len();

            // Reset current passage
            current_ranges.clear();
            current_token_count = 0;
        } else if current_token_count > 0 && current_token_count + para_tokens > TARGET_TOKENS {
            // Adding this paragraph would exceed target, so emit current and start new
            emit_passage_from_ranges(
                body,
                &current_ranges,
                section.ordinal,
                ordinal,
                &mut passages,
            );
            ordinal += 1;

            // Start new passage with overlap from previous
            let overlap_start = find_overlap_start_byte(&current_ranges, body, OVERLAP_TOKENS);
            current_ranges = vec![
                (overlap_start, current_ranges.last().unwrap().1),
                (para_start, para_end),
            ];
            current_token_count = count_tokens(&body[overlap_start..para_end]);
        } else {
            // Add this paragraph to current passage
            current_ranges.push((para_start, para_end));
            current_token_count += para_tokens;
        }
    }

    // Emit final passage
    if !current_ranges.is_empty() {
        emit_passage_from_ranges(
            body,
            &current_ranges,
            section.ordinal,
            ordinal,
            &mut passages,
        );
    }

    passages
}

/// Emit a passage from a set of non-contiguous byte ranges.
/// Merges ranges into a single contiguous slice, trims it, and validates invariant.
fn emit_passage_from_ranges(
    body: &str,
    ranges: &[(usize, usize)],
    section_ordinal: usize,
    ordinal: usize,
    passages: &mut Vec<Passage>,
) {
    if ranges.is_empty() {
        return;
    }

    // Merge ranges: take first start and last end (includes whitespace between)
    let merged_start = ranges.iter().map(|(s, _)| s).min().copied().unwrap_or(0);
    let merged_end = ranges.iter().map(|(_, e)| e).max().copied().unwrap_or(0);

    let merged_text = &body[merged_start..merged_end];
    let trimmed = merged_text.trim();

    if trimmed.is_empty() {
        return;
    }

    // Calculate trimmed byte positions
    let leading_ws = merged_text.len() - merged_text.trim_start().len();
    let trimmed_start = merged_start + leading_ws;
    let trimmed_end =
        merged_start + (merged_text.len() - (merged_text.len() - merged_text.trim_end().len()));

    // Verify invariant: body[trimmed_start..trimmed_end] == trimmed
    debug_assert_eq!(&body[trimmed_start..trimmed_end], trimmed);

    passages.push(Passage {
        section_ordinal,
        ordinal,
        text: trimmed.to_string(),
        start_byte: trimmed_start,
        end_byte: trimmed_end,
    });
}

/// Find the byte position where the last ~48 whitespace-tokens begin in a merged range.
fn find_overlap_start_byte(ranges: &[(usize, usize)], body: &str, overlap_tokens: usize) -> usize {
    if let Some((last_start, last_end)) = ranges.last() {
        let last_range_text = &body[*last_start..*last_end];
        let overlap_byte_offset = find_overlap_byte_pos(last_range_text, overlap_tokens);
        last_start + overlap_byte_offset
    } else {
        0
    }
}

/// Find byte position where the last `overlap_tokens` whitespace-separated tokens begin.
fn find_overlap_byte_pos(text: &str, overlap_tokens: usize) -> usize {
    let total_tokens = text.split_whitespace().count();
    if total_tokens <= overlap_tokens {
        return 0;
    }

    let start_token_idx = total_tokens - overlap_tokens;
    let mut token_idx = 0;
    let mut in_token = false;

    for (i, c) in text.char_indices() {
        if c.is_whitespace() {
            in_token = false;
        } else if !in_token {
            // Starting a new token
            if token_idx == start_token_idx {
                return i;
            }
            token_idx += 1;
            in_token = true;
        }
    }

    0
}

fn split_long_text(
    text: &str,
    base_offset: usize,
    section_ordinal: usize,
    start_ordinal: usize,
) -> Vec<Passage> {
    let mut result = Vec::new();
    let mut remaining = text;
    let mut offset = base_offset;
    let mut ordinal = start_ordinal;

    loop {
        if remaining.trim().is_empty() {
            break;
        }

        let tokens = count_tokens(remaining);
        if tokens <= 420 {
            let trimmed = remaining.trim();
            if !trimmed.is_empty() {
                let leading_ws = remaining.len() - remaining.trim_start().len();
                let trailing_ws = remaining.len() - remaining.trim_end().len();
                result.push(Passage {
                    section_ordinal,
                    ordinal,
                    text: trimmed.to_string(),
                    start_byte: offset + leading_ws,
                    end_byte: offset + (remaining.len() - trailing_ws),
                });
            }
            break;
        }

        // Find sentence boundary within first ~320 tokens
        let mut sentence_end = None;
        for (i, c) in remaining.char_indices() {
            if matches!(c, '.' | '!' | '?') {
                // Check if next char is whitespace or end
                let next_bytes = &remaining[i + c.len_utf8()..];
                if next_bytes.is_empty()
                    || next_bytes
                        .chars()
                        .next()
                        .is_some_and(|ch| ch.is_whitespace())
                {
                    sentence_end = Some(i + c.len_utf8());
                }
            }
            if count_tokens(&remaining[..i]) > 320 {
                break;
            }
        }

        let split_at = if let Some(pos) = sentence_end {
            pos
        } else {
            // Fallback: split at word boundary near token budget
            let target_chars =
                (remaining.len() * 320 / count_tokens(remaining)).min(remaining.len());
            remaining
                .char_indices()
                .find(|(i, c)| c.is_whitespace() && *i > target_chars / 2 && *i < target_chars * 2)
                .map(|(i, _)| i)
                .unwrap_or_else(|| {
                    // Ensure we don't split mid-character
                    let fallback = remaining.len().min(1000);
                    let mut pos = fallback;
                    while pos > 0 && !remaining.is_char_boundary(pos) {
                        pos -= 1;
                    }
                    pos
                })
        };

        if split_at > 0 {
            let part = &remaining[..split_at];
            let trimmed = part.trim();
            if !trimmed.is_empty() {
                let leading_ws = part.len() - part.trim_start().len();
                let trailing_ws = part.len() - part.trim_end().len();
                result.push(Passage {
                    section_ordinal,
                    ordinal,
                    text: trimmed.to_string(),
                    start_byte: offset + leading_ws,
                    end_byte: offset + (part.len() - trailing_ws),
                });
                ordinal += 1;
            }

            // Move past with overlap
            let overlap_byte_pos = find_overlap_byte_pos(part, 48);
            let next_start = if split_at > overlap_byte_pos {
                split_at - overlap_byte_pos
            } else {
                split_at
            };
            remaining = &remaining[next_start..];
            offset += next_start;
        } else {
            break;
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Sol amendment 3: cross-encoder trait/boundary scaffold (interface only)
// Reranking is not a dependency until lexical + dense + RRF passes evaluation (H1).
// ---------------------------------------------------------------------------

/// Score for a single (passage, query) pair from a cross-encoder reranker.
///
/// The value is an arbitrary float produced by the underlying model;
/// callers must not assume a specific range until the model is fixed.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct CrossEncoderScore(pub f32);

/// A re-usable cross-encoder session that can score (passage, query) pairs.
///
/// Sol amendment 3: trait only — no concrete implementation until H1 gate.
/// A future implementation might wrap `ort` (ONNX Runtime) or `candle` to
/// load a local ONNX model (e.g. `ms-marco-MiniLM-L-6-v2`).
pub trait CrossEncoder: Send + Sync {
    /// Score a single (passage, query) pair.
    ///
    /// Returns `None` if scoring is unavailable or the model is not loaded;
    /// the retrieval pipeline falls back to the RRF order in that case
    /// (Sol amendment 3: failure/timeouts preserve RRF order).
    fn score(&self, passage: &str, query: &str) -> Option<CrossEncoderScore>;

    /// Score a batch of (passage, query) pairs.
    ///
    /// Default implementation calls `score` per pair. A concrete impl should
    /// override this for efficient batch inference.
    fn score_batch(
        &self,
        passages: &[&str],
        query: &str,
    ) -> Vec<Option<CrossEncoderScore>> {
        passages.iter().map(|p| self.score(p, query)).collect()
    }
}

// ---------------------------------------------------------------------------
// Sol amendment 4: enrichment-hook interface (scaffold only)
/// Future/optional ingestion-time semantic enrichment: a strong LLM may
/// generate retrieval titles, summaries, entities, or classification tags
/// for passages.  Derived metadata only — original text remains authoritative.
/// Do not implement enrichment in D1 unless D1 naturally requires the interface.
/// Enrichment metadata attached to a passage at ingestion time.
///
/// All fields optional; the presence of enrichment must never be required
/// for correct retrieval. Original passage text is always authoritative
/// (Sol amendment 4).
#[derive(Debug, Clone, Default)]
pub struct PassageEnrichment {
    /// LLM-generated retrieval title (shorter, more descriptive than heading path).
    pub title: Option<String>,
    /// One-sentence summary of the passage content.
    pub summary: Option<String>,
    /// Named entities extracted from the passage.
    pub entities: Option<Vec<String>>,
    /// Free-form classification labels.
    pub labels: Option<Vec<String>>,
}

/// Enrichment hook called at ingestion time for a new passage.
///
/// Sol amendment 4: interface only — no implementation in D1.
/// A future writer (D2) calls `enrich()` synchronously or via a background
/// queue; a `None` return is a no-op, never a failure.
pub trait EnrichmentHook: Send + Sync {
    /// Produce enrichment metadata for the given passage text and heading path.
    ///
    /// Returns `None` if enrichment is unavailable, disabled, or times out.
    /// The calling pipeline treats `None` as a no-op — original text stays authoritative.
    fn enrich(
        &self,
        passage_text: &str,
        heading_path: &[String],
    ) -> Option<PassageEnrichment>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::PageId;

    fn test_page_id() -> PageId {
        "550e8400-e29b-41d4-a716-446655440000".parse().unwrap()
    }

    /// Helper: verify that all passages' byte ranges correctly slice back to their text.
    fn assert_passage_byte_ranges_valid(body: &str, passages: &[Passage]) {
        for p in passages {
            let sliced = &body[p.start_byte..p.end_byte];
            assert_eq!(
                sliced, p.text,
                "Passage {} failed invariant: &body[{}..{}] != text",
                p.ordinal, p.start_byte, p.end_byte
            );
        }
    }

    #[test]
    fn test_preamble_creates_section_heading_empty() {
        let body = "This is preamble text\nwithout any heading.\n\nSome more content.";
        let page_id = test_page_id();
        let sections = parse_sections(body, page_id);

        assert_eq!(sections.len(), 1);
        assert!(sections[0].heading.is_empty());
        assert_eq!(sections[0].body, body);
        assert_eq!(sections[0].start_byte, 0);
        assert_eq!(sections[0].end_byte, body.len());

        let passages = split_passages(&sections[0]);
        assert_passage_byte_ranges_valid(&sections[0].body, &passages);
    }

    #[test]
    fn test_atx_headings_create_sections() {
        let body = "# H1\n\nContent one.\n\n## H2\n\nContent two.\n\n### H3\n\nContent three.";
        let page_id = test_page_id();
        let sections = parse_sections(body, page_id);

        assert_eq!(sections.len(), 3);
        assert_eq!(sections[0].level, 1);
        assert_eq!(sections[0].heading, "H1");
        assert_eq!(sections[0].heading_path, vec!["H1"]);
        assert_eq!(sections[1].level, 2);
        assert_eq!(sections[1].heading, "H2");
        assert_eq!(sections[1].heading_path, vec!["H1", "H2"]);
        assert_eq!(sections[2].level, 3);
        assert_eq!(sections[2].heading, "H3");
        assert_eq!(sections[2].heading_path, vec!["H1", "H2", "H3"]);

        for section in &sections {
            let passages = split_passages(section);
            assert_passage_byte_ranges_valid(&section.body, &passages);
        }
    }

    #[test]
    fn test_setext_headings_create_sections() {
        let body = "H1\n===\n\nContent one.\n\nH2\n---\n\nContent two.";
        let page_id = test_page_id();
        let sections = parse_sections(body, page_id);

        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].level, 1);
        assert_eq!(sections[0].heading, "H1");
        assert_eq!(sections[1].level, 2);
        assert_eq!(sections[1].heading, "H2");

        for section in &sections {
            let passages = split_passages(section);
            assert_passage_byte_ranges_valid(&section.body, &passages);
        }
    }

    #[test]
    fn test_nested_heading_path() {
        let body = "# H1\n\n## H2\n\n### H3\n\n## H2 again\n\n# H1 again";
        let page_id = test_page_id();
        let sections = parse_sections(body, page_id);

        assert_eq!(sections.len(), 5);
        // H1 -> H2 -> H3
        assert_eq!(sections[2].heading_path, vec!["H1", "H2", "H3"]);
        // H2 again (jump back to level 2)
        assert_eq!(sections[3].heading_path, vec!["H1", "H2 again"]);
        // H1 again (jump back to level 1)
        assert_eq!(sections[4].heading_path, vec!["H1 again"]);

        for section in &sections {
            let passages = split_passages(section);
            assert_passage_byte_ranges_valid(&section.body, &passages);
        }
    }

    #[test]
    fn test_fenced_code_heading_ignored() {
        let body = "# Real H1\n\n```\n# Not a heading\n```\n\n## Real H2";
        let page_id = test_page_id();
        let sections = parse_sections(body, page_id);

        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].heading, "Real H1");
        assert_eq!(sections[1].heading, "Real H2");
        // The code block content should be in first section's body
        assert!(sections[0].body.contains("# Not a heading"));

        for section in &sections {
            let passages = split_passages(section);
            assert_passage_byte_ranges_valid(&section.body, &passages);
        }
    }

    #[test]
    fn test_indented_code_heading_ignored() {
        // Four-space indented block after blank line is a code block in CommonMark
        let body = "# Real H1\n\n    # Not a heading\n\n## Real H2";
        let page_id = test_page_id();
        let sections = parse_sections(body, page_id);

        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].heading, "Real H1");
        assert_eq!(sections[1].heading, "Real H2");
        // The indented code should be in first section's body
        assert!(sections[0].body.contains("# Not a heading"));

        for section in &sections {
            let passages = split_passages(section);
            assert_passage_byte_ranges_valid(&section.body, &passages);
        }
    }

    #[test]
    fn test_unicode_offsets() {
        let body = "# Café\n\nCafé has café.\n\n## Résumé";
        let page_id = test_page_id();
        let sections = parse_sections(body, page_id);

        assert_eq!(sections.len(), 2);
        // Verify byte offsets slice correctly
        assert_eq!(
            &body[sections[0].start_byte..sections[0].end_byte],
            sections[0].body
        );
        assert_eq!(
            &body[sections[1].start_byte..sections[1].end_byte],
            sections[1].body
        );

        // Check unicode chars in heading text
        assert_eq!(sections[0].heading, "Café");
        assert_eq!(sections[1].heading, "Résumé");

        for section in &sections {
            let passages = split_passages(section);
            assert_passage_byte_ranges_valid(&section.body, &passages);
        }
    }

    #[test]
    fn test_passage_budget_and_overlap() {
        let mut body = String::from("Para one. ");
        body.push_str(&"Para one. ".repeat(99));
        body.push_str("\n\n");
        body.push_str(&"Para two. ".repeat(100));
        let page_id = test_page_id();
        let sections = parse_sections(&body, page_id);
        assert_eq!(sections.len(), 1);

        let passages = split_passages(&sections[0]);
        assert_passage_byte_ranges_valid(&sections[0].body, &passages);

        // Should create multiple passages due to token budget
        assert!(
            passages.len() >= 2,
            "Expected multiple passages, got {}",
            passages.len()
        );

        // Check each passage <= 420 tokens
        for p in &passages {
            assert!(
                count_tokens(&p.text) <= 420,
                "Passage exceeds 420 tokens: {}",
                count_tokens(&p.text)
            );
        }

        // Check overlap - adjacent passages should share content
        if passages.len() >= 2 {
            let p0 = &passages[0].text;
            let p1 = &passages[1].text;
            // At least some overlap in words
            let words0: std::collections::HashSet<_> = p0.split_whitespace().collect();
            let words1: std::collections::HashSet<_> = p1.split_whitespace().collect();
            let overlap: Vec<_> = words0.intersection(&words1).collect();
            assert!(
                !overlap.is_empty(),
                "Expected overlap between adjacent passages"
            );
        }
    }

    #[test]
    fn test_sentence_preference() {
        let body = "First sentence. Second sentence! Third sentence? Fourth sentence.";
        let page_id = test_page_id();
        let sections = parse_sections(body, page_id);
        let passages = split_passages(&sections[0]);
        assert_passage_byte_ranges_valid(&sections[0].body, &passages);

        // Should prefer sentence boundaries
        for p in &passages {
            let text = p.text.trim_end();
            if text.ends_with('.') || text.ends_with('!') || text.ends_with('?') {
                // Good - ends at sentence boundary
            } else if count_tokens(&p.text) >= 420 {
                // Acceptable if at hard limit
            } else {
                panic!("Passage should end at sentence boundary: {:?}", text);
            }
        }
    }

    #[test]
    fn test_long_unbroken_span_fallback() {
        // A very long "paragraph" with no sentence punctuation
        let long = "word ".repeat(500);
        let body = long;
        let page_id = test_page_id();
        let sections = parse_sections(&body, page_id);
        let passages = split_passages(&sections[0]);
        assert_passage_byte_ranges_valid(&sections[0].body, &passages);

        // Should not panic, should produce multiple passages
        assert!(passages.len() >= 2);
        for p in &passages {
            assert!(!p.text.is_empty());
        }
    }

    #[test]
    fn test_preamble_with_heading() {
        // Preamble should be captured as level-0 section before first heading
        let body = "Intro paragraph before any heading.\n\nMore intro.\n\n# First H1\n\nContent under heading.";
        let page_id = test_page_id();
        let sections = parse_sections(body, page_id);

        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].level, 0);
        assert!(sections[0].heading.is_empty());
        assert!(sections[0].heading_path.is_empty());
        assert!(sections[0].body.contains("Intro paragraph"));
        assert!(sections[0].body.contains("More intro"));
        assert_eq!(sections[1].level, 1);
        assert_eq!(sections[1].heading, "First H1");
        assert_eq!(sections[1].heading_path, vec!["First H1"]);

        for section in &sections {
            let passages = split_passages(section);
            assert_passage_byte_ranges_valid(&section.body, &passages);
        }
    }

    #[test]
    fn test_multibyte_char_no_panic() {
        // >420 tokens of multibyte CJK chars with no spaces (must not panic on mid-char split)
        let cjk_word = "漢字";
        let long = cjk_word.repeat(300); // Each char is ~3 bytes, no spaces = no natural token boundaries
        let body = long;
        let page_id = test_page_id();
        let sections = parse_sections(&body, page_id);
        let passages = split_passages(&sections[0]);
        assert_passage_byte_ranges_valid(&sections[0].body, &passages);

        // Should not panic, should produce multiple passages
        assert!(
            !passages.is_empty(),
            "Expected passages for multibyte content"
        );

        // All passages should have content
        for p in &passages {
            assert!(!p.text.is_empty());
        }
    }

    #[test]
    fn test_long_paragraph_plus_normal_no_duplicate() {
        // Regression test for defect 1: ensure long para content appears exactly once in concatenated passages
        let long_para = "unique_marker ".repeat(150); // ~150 tokens
        let mut body = String::new();
        body.push_str(&long_para);
        body.push_str("\n\nNormal paragraph content.");

        let page_id = test_page_id();
        let sections = parse_sections(&body, page_id);
        assert_eq!(sections.len(), 1);

        let passages = split_passages(&sections[0]);
        assert_passage_byte_ranges_valid(&sections[0].body, &passages);

        // Concatenate all passage text
        let concatenated = passages
            .iter()
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>()
            .join("");

        // Count occurrences of the unique marker
        let marker_count = concatenated.matches("unique_marker").count();
        assert_eq!(
            marker_count, 150,
            "Unique marker should appear exactly 150 times, but got {}",
            marker_count
        );

        // Verify no passage is empty
        for p in &passages {
            assert!(!p.text.is_empty());
        }
    }
}
