use std::ops::RangeInclusive;

use agent_settings::TerminalCommentPattern;
use terminal::{Content as TerminalContent, GridRow};

/// The most rows a comment target spans, and how far from the pointed row
/// its start and end are looked for, so a long run of indented output can't
/// quote a whole log.
pub(crate) const MAX_TARGET_ROWS: usize = 200;

/// Footer hints Claude Code shows under an interactive picker, such as its
/// question and permission prompts. Comments go into the prompt, which the
/// picker replaces until it's answered.
const PICKER_HINTS: &[&str] = &["Enter to select", "Esc to cancel"];
/// How many of the last rows with text are searched for picker hints.
const PICKER_HINT_ROWS: usize = 12;

/// Box borders aren't part of the text they surround, so targets, quotes
/// and comment anchors leave them out.
pub(crate) fn is_box_drawing(character: char) -> bool {
    ('\u{2500}'..='\u{257F}').contains(&character)
}

/// Characters that comment anchors skip: whitespace and box borders change
/// when a TUI redraws or the terminal resizes, and an ellipsis marks a
/// truncated quote.
pub(crate) fn is_ignored_by_anchor(character: char) -> bool {
    character.is_whitespace() || is_box_drawing(character) || character == '…'
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TargetRow {
    pub line: i32,
    pub columns: RangeInclusive<usize>,
    /// The row's text continues on the next row, which the terminal wrapped.
    pub wraps: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommentTarget {
    pub rows: Vec<TargetRow>,
    /// The item continues past `rows`.
    pub truncated: bool,
}

struct LogicalLine {
    /// Indices into the rows, of the rows the terminal wrapped this line over.
    rows: RangeInclusive<usize>,
    indent: Option<usize>,
    has_marker: bool,
}

impl LogicalLine {
    fn is_blank(&self) -> bool {
        self.indent.is_none()
    }
}

/// The columns of a row's text, without surrounding whitespace and box
/// borders.
fn text_columns(characters: &[char]) -> Option<RangeInclusive<usize>> {
    let is_text = |character: &char| !character.is_whitespace() && !is_box_drawing(*character);
    let start = characters.iter().position(is_text)?;
    let end = characters.iter().rposition(is_text)?;
    Some(start..=end)
}

/// Whether text starts with a list marker followed by a space: a bullet,
/// an agent's message or answer marker, or a number or letter followed by
/// `.` or `)`.
fn starts_with_marker(text: &[char]) -> bool {
    const BULLETS: &[char] = &['-', '*', '•', '◦', '▪', '●', '⏺', '⎿', '>', '❯'];
    let followed_by_space =
        |index: usize| text.get(index).is_some_and(|character| *character == ' ');
    let Some(first) = text.first() else {
        return false;
    };
    if BULLETS.contains(first) {
        return followed_by_space(1);
    }
    let digits = text
        .iter()
        .take_while(|character| character.is_ascii_digit())
        .count();
    let label_length = if (1..=3).contains(&digits) {
        digits
    } else if first.is_ascii_alphabetic() {
        1
    } else {
        return false;
    };
    text.get(label_length)
        .is_some_and(|character| *character == '.' || *character == ')')
        && followed_by_space(label_length + 1)
}

fn logical_lines(rows: &[GridRow]) -> Vec<LogicalLine> {
    let mut lines = Vec::new();
    let mut start = 0;
    while start < rows.len() {
        let mut end = start;
        while end + 1 < rows.len() && rows[end].wraps {
            end += 1;
        }
        let first_text = (start..=end).find_map(|index| {
            let columns = text_columns(&rows[index].characters)?;
            Some((index, columns))
        });
        let (indent, has_marker) = match first_text {
            Some((index, columns)) => {
                let text = &rows[index].characters[*columns.start()..];
                (
                    Some(*columns.start()),
                    index == start && starts_with_marker(text),
                )
            }
            None => (None, false),
        };
        lines.push(LogicalLine {
            rows: start..=end,
            indent,
            has_marker,
        });
        start = end + 1;
    }
    lines
}

/// The index of the last line of the item whose marker is on line
/// `marker`: the lines after it that are indented deeper, up to a blank line.
fn item_end(lines: &[LogicalLine], marker: usize) -> usize {
    let Some(marker_indent) = lines[marker].indent else {
        return marker;
    };
    let mut end = marker;
    while let Some(next) = lines.get(end + 1)
        && next.indent.is_some_and(|indent| indent > marker_indent)
    {
        end += 1;
    }
    end
}

/// Markers of a new agent message or a user prompt, which end a pattern's
/// block even when it starts at the same indentation.
const MESSAGE_MARKERS: &[&str] = &["⏺ ", "❯ ", "> "];

/// The text that patterns match: the line's rows joined, without the cells
/// that pad wide characters and without surrounding whitespace, nor the
/// box borders around other text.
fn match_text(rows: &[GridRow], line: &LogicalLine) -> String {
    let mut text = String::new();
    for row in &rows[line.rows.clone()] {
        text.extend(
            row.characters
                .iter()
                .enumerate()
                .filter(|(column, _)| !row.wide_char_spacers.contains(column))
                .map(|(_, character)| *character),
        );
    }
    let text = text.trim();
    let without_borders =
        text.trim_matches(|character: char| character.is_whitespace() || is_box_drawing(character));
    // A line of only borders is a separator, which `end` patterns match.
    if without_borders.is_empty() {
        text.to_string()
    } else {
        without_borders.to_string()
    }
}

/// The match text of each line, and whether it starts a block, worked out
/// when first needed.
struct MatchTexts<'a> {
    rows: &'a [GridRow],
    lines: &'a [LogicalLine],
    patterns: &'a [TerminalCommentPattern],
    texts: Vec<Option<String>>,
    starts_block: Vec<Option<bool>>,
}

impl<'a> MatchTexts<'a> {
    fn new(
        rows: &'a [GridRow],
        lines: &'a [LogicalLine],
        patterns: &'a [TerminalCommentPattern],
    ) -> Self {
        Self {
            rows,
            lines,
            patterns,
            texts: vec![None; lines.len()],
            starts_block: vec![None; lines.len()],
        }
    }

    fn get(&mut self, index: usize) -> &str {
        let (rows, lines) = (self.rows, self.lines);
        self.texts[index].get_or_insert_with(|| match_text(rows, &lines[index]))
    }

    /// Whether the line matches the start of any pattern.
    fn starts_block(&mut self, index: usize) -> bool {
        if let Some(starts_block) = self.starts_block[index] {
            return starts_block;
        }
        let patterns = self.patterns;
        let text = self.get(index);
        let starts_block = patterns.iter().any(|pattern| pattern.start.is_match(text));
        self.starts_block[index] = Some(starts_block);
        starts_block
    }
}

/// The lines of the block of `pattern` that contains line `pointed`: from
/// the nearest line above that matches its start, to before the next line
/// that starts any pattern's block, matches its end, is indented less, or
/// starts a message at the same indentation.
fn pattern_block(
    lines: &[LogicalLine],
    texts: &mut MatchTexts,
    pointed: usize,
    pattern: &TerminalCommentPattern,
) -> Option<RangeInclusive<usize>> {
    let start = (0..=pointed)
        .rev()
        .find(|index| !lines[*index].is_blank() && pattern.start.is_match(texts.get(*index)))?;
    let start_indent = lines[start].indent?;
    let mut last = start;
    for index in start + 1..lines.len() {
        let text = texts.get(index);
        if !text.is_empty() && pattern.end.as_ref().is_some_and(|end| end.is_match(text)) {
            break;
        }
        let Some(indent) = lines[index].indent else {
            continue;
        };
        let starts_message = MESSAGE_MARKERS
            .iter()
            .any(|marker| text.starts_with(marker));
        if indent < start_indent
            || (indent == start_indent && starts_message)
            || texts.starts_block(index)
        {
            break;
        }
        last = index;
    }
    (pointed <= last).then_some(start..=last)
}

/// The innermost list item that contains line `pointed`, with its marker
/// on lines `first` to `pointed` and its end cut at line `last`.
fn innermost_item(
    lines: &[LogicalLine],
    pointed: usize,
    first: usize,
    last: usize,
) -> Option<RangeInclusive<usize>> {
    (first..=pointed)
        .rev()
        .take_while(|index| !lines[*index].is_blank())
        .filter(|index| lines[*index].has_marker)
        .map(|marker| marker..=item_end(lines, marker).min(last))
        .find(|item| *item.end() >= pointed)
}

/// The block at `line`: the innermost list item, the block of the first of
/// `patterns` that contains it, or else the line itself. Inside a pattern's
/// block, a list item other than its first line is targeted on its own.
/// `rows` are consecutive grid rows around `line`, and `more_below` says
/// whether the grid continues after them.
pub(crate) fn find_comment_target(
    rows: &[GridRow],
    line: i32,
    more_below: bool,
    patterns: &[TerminalCommentPattern],
) -> Option<CommentTarget> {
    let lines = logical_lines(rows);
    let row_index = rows.iter().position(|row| row.line == line)?;
    let pointed = lines
        .iter()
        .position(|logical_line| logical_line.rows.contains(&row_index))?;
    if lines[pointed].is_blank() {
        return None;
    }

    let mut texts = MatchTexts::new(rows, &lines, patterns);
    let block = patterns
        .iter()
        .find_map(|pattern| pattern_block(&lines, &mut texts, pointed, pattern));
    let item = match &block {
        Some(block) => innermost_item(&lines, pointed, block.start() + 1, *block.end()),
        None => innermost_item(&lines, pointed, 0, lines.len() - 1),
    };
    let line_range = item.or(block).unwrap_or(pointed..=pointed);
    let first_row = *lines[*line_range.start()].rows.start();
    let last_row = *lines[*line_range.end()].rows.end();
    let has_text = |index: &usize| text_columns(&rows[*index].characters).is_some();
    let first_text_row = (first_row..=last_row).find(has_text)?;
    let last_text_row = (first_row..=last_row).rev().find(has_text)?;
    let mut target_rows = (first_text_row..=last_text_row)
        .filter_map(|index| {
            let row = &rows[index];
            // A wrapped row's trailing spaces separate it from the next row's
            // text, even when the row has nothing else.
            let columns = match text_columns(&row.characters) {
                Some(columns) if row.wraps => {
                    *columns.start()..=row.characters.len().checked_sub(1)?
                }
                Some(columns) => columns,
                None if row.wraps => 0..=row.characters.len().checked_sub(1)?,
                None => return None,
            };
            Some(TargetRow {
                line: row.line,
                columns,
                wraps: row.wraps,
            })
        })
        .collect::<Vec<_>>();
    let mut truncated = more_below && last_row + 1 == rows.len();
    if target_rows.len() > MAX_TARGET_ROWS {
        target_rows.truncate(MAX_TARGET_ROWS);
        truncated = true;
    }
    Some(CommentTarget {
        rows: target_rows,
        truncated,
    })
}

/// Joins the text of a target's rows: rows the terminal wrapped continue
/// the line, and an ellipsis marks text cut off at the end.
pub(crate) fn target_text(target: &CommentTarget, rows_text: &[String]) -> String {
    let mut text = String::new();
    for (row, row_text) in target.rows.iter().zip(rows_text) {
        text.push_str(row_text);
        if !row.wraps {
            text.push('\n');
        }
    }
    let mut text = text.trim_end().to_string();
    if target.truncated {
        text.push('…');
    }
    text
}

/// Whether an interactive picker is open: its hints are in the last rows
/// with text on screen, which aren't the screen's last rows until the
/// output fills it.
pub(crate) fn picker_visible(content: &TerminalContent) -> bool {
    let mut rows = Vec::<String>::new();
    let mut current_line = None;
    for cell in &content.cells {
        if current_line != Some(cell.point.line) {
            current_line = Some(cell.point.line);
            rows.push(String::new());
        }
        if let Some(row) = rows.last_mut() {
            row.push(cell.character());
        }
    }
    rows.iter()
        .rev()
        .filter(|row| !row.trim().is_empty())
        .take(PICKER_HINT_ROWS)
        .any(|row| PICKER_HINTS.iter().any(|hint| row.contains(hint)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(text: &str) -> Vec<GridRow> {
        text.lines()
            .enumerate()
            .map(|(index, line)| {
                let (line_text, wraps) = match line.strip_suffix('\\') {
                    Some(line_text) => (line_text, true),
                    None => (line, false),
                };
                GridRow {
                    line: index as i32,
                    characters: line_text.chars().collect(),
                    wide_char_spacers: Vec::new(),
                    wraps,
                }
            })
            .collect()
    }

    fn target_lines(rows: &[GridRow], line: i32) -> Option<RangeInclusive<i32>> {
        let target = find_comment_target(rows, line, false, &[])?;
        let first = target.rows.first()?.line;
        let last = target.rows.last()?.line;
        Some(first..=last)
    }

    #[test]
    fn test_question_with_options() {
        let rows = grid(indoc::indoc! {"
            Some intro text.

            1. Which auth method should we use?
               a) OAuth
               b) API key
                  with a second line
            2. Should tokens expire?
        "});
        assert_eq!(target_lines(&rows, 2), Some(2..=5));
        assert_eq!(target_lines(&rows, 3), Some(3..=3));
        assert_eq!(target_lines(&rows, 5), Some(4..=5));
        assert_eq!(target_lines(&rows, 6), Some(6..=6));
    }

    #[test]
    fn test_line_fallback_and_blank_lines() {
        let rows = grid(indoc::indoc! {"
            Plain output line
            another plain line

            - bullet
        "});
        assert_eq!(target_lines(&rows, 1), Some(1..=1));
        assert_eq!(target_lines(&rows, 2), None);
        assert_eq!(target_lines(&rows, 3), Some(3..=3));
    }

    #[test]
    fn test_agent_message_with_nested_bullets() {
        let rows = grid(indoc::indoc! {"
            ⏺ Here is the plan:
              - first step
                continued
              - second step
            > my answer
        "});
        assert_eq!(target_lines(&rows, 0), Some(0..=3));
        assert_eq!(target_lines(&rows, 2), Some(1..=2));
        assert_eq!(target_lines(&rows, 4), Some(4..=4));
    }

    #[test]
    fn test_box_borders_are_left_out() {
        let rows = grid(indoc::indoc! {"
            ╭──────────────────╮
            │ - boxed bullet   │
            │   continued      │
            ╰──────────────────╯
        "});
        let target = find_comment_target(&rows, 2, false, &[]).unwrap();
        assert_eq!(
            target.rows,
            vec![
                TargetRow {
                    line: 1,
                    columns: 2..=15,
                    wraps: false,
                },
                TargetRow {
                    line: 2,
                    columns: 4..=12,
                    wraps: false,
                },
            ]
        );
        assert_eq!(find_comment_target(&rows, 0, false, &[]), None);
    }

    #[test]
    fn test_wrapped_rows_are_one_line() {
        let rows = grid("plain text that the \\\nterminal wrapped\n");
        let target = find_comment_target(&rows, 1, false, &[]).unwrap();
        assert_eq!(target.rows.len(), 2);
        assert_eq!(target.rows[0].columns, 0..=19);
        assert_eq!(
            target_text(
                &target,
                &[
                    "plain text that the ".to_string(),
                    "terminal wrapped".to_string()
                ]
            ),
            "plain text that the terminal wrapped"
        );
    }

    #[test]
    fn test_markers() {
        for text in ["- a", "* a", "• a", "❯ a", "1. a", "12) a", "a) a", "B. a"] {
            assert!(
                starts_with_marker(&text.chars().collect::<Vec<_>>()),
                "{text}"
            );
        }
        for text in ["-a", "e.g. a", "1.5 a", "1234. a", "ab) a", "text"] {
            assert!(
                !starts_with_marker(&text.chars().collect::<Vec<_>>()),
                "{text}"
            );
        }
    }

    #[test]
    fn test_long_items_are_truncated() {
        let mut text = String::from("- long item\n");
        for _ in 0..MAX_TARGET_ROWS + 10 {
            text.push_str("  more\n");
        }
        let rows = grid(&text);
        let target = find_comment_target(&rows, 0, false, &[]).unwrap();
        assert_eq!(target.rows.len(), MAX_TARGET_ROWS);
        assert!(target.truncated);
        assert!(target_text(&target, &["- long item".to_string()]).ends_with('…'));
    }

    fn default_patterns() -> Vec<TerminalCommentPattern> {
        let compile = |pattern: &str| {
            agent_settings::CompiledRegex::try_new(pattern, true).expect("valid test regex")
        };
        [
            r"^(⏺\s+)?(❓\s*)?(\*\*)?Q\d+(\*\*)?\s*[-–—:.)]",
            r"^(⏺\s+)?(\*\*)?Question\s+\d+\s*[-–—:.)]",
        ]
        .into_iter()
        .map(|start| TerminalCommentPattern {
            start: compile(start),
            end: Some(compile("^[─━-]{3,}$")),
        })
        .collect()
    }

    fn pattern_target_lines(
        rows: &[GridRow],
        line: i32,
        patterns: &[TerminalCommentPattern],
    ) -> Option<RangeInclusive<i32>> {
        let target = find_comment_target(rows, line, false, patterns)?;
        Some(target.rows.first()?.line..=target.rows.last()?.line)
    }

    #[test]
    fn test_grilling_questions() {
        let rows = grid(indoc::indoc! {"
            ⏺ Some questions:

              ❓ Q1 - Auth method:
              Which auth method should we use?

              ➡ Recommended: OAuth
                - because it's standard
              ────────────
              ❓ Q2 - Expiry:
              ➡ Recommended: one hour
            > my answer
        "});
        let patterns = default_patterns();
        assert_eq!(pattern_target_lines(&rows, 2, &patterns), Some(2..=6));
        assert_eq!(pattern_target_lines(&rows, 5, &patterns), Some(2..=6));
        assert_eq!(pattern_target_lines(&rows, 6, &patterns), Some(6..=6));
        assert_eq!(pattern_target_lines(&rows, 7, &patterns), None);
        assert_eq!(pattern_target_lines(&rows, 9, &patterns), Some(8..=9));
        assert_eq!(pattern_target_lines(&rows, 10, &patterns), Some(10..=10));
        assert_eq!(pattern_target_lines(&rows, 0, &patterns), Some(0..=0));
    }

    #[test]
    fn test_pattern_ends_at_dedent_and_next_start() {
        let rows = grid(indoc::indoc! {"
            ⏺ Question 1: first?
              detail
              Question 2: second?
            ⏺ Done.
        "});
        let patterns = default_patterns();
        assert_eq!(pattern_target_lines(&rows, 1, &patterns), Some(0..=1));
        assert_eq!(pattern_target_lines(&rows, 2, &patterns), Some(2..=2));
        assert_eq!(pattern_target_lines(&rows, 3, &patterns), Some(3..=3));
    }

    #[test]
    fn test_first_pattern_wins() {
        let compile = |pattern: &str| {
            agent_settings::CompiledRegex::try_new(pattern, true).expect("valid test regex")
        };
        let rows = grid("Q1\nfirst\n---\nsecond\n");
        let patterns = vec![
            TerminalCommentPattern {
                start: compile(r"^Q\d"),
                end: Some(compile("^---$")),
            },
            TerminalCommentPattern {
                start: compile(r"^Q\d"),
                end: None,
            },
        ];
        assert_eq!(pattern_target_lines(&rows, 1, &patterns), Some(0..=1));
        assert_eq!(pattern_target_lines(&rows, 3, &patterns), Some(0..=3));
        let reversed = patterns.into_iter().rev().collect::<Vec<_>>();
        assert_eq!(pattern_target_lines(&rows, 1, &reversed), Some(0..=3));
    }

    #[test]
    fn test_wide_char_spacers_are_left_out_of_match_text() {
        let mut rows = grid("❓  Q1 - Title\n");
        rows[0].wide_char_spacers = vec![1];
        let lines = logical_lines(&rows);
        assert_eq!(match_text(&rows, &lines[0]), "❓ Q1 - Title");
        assert_eq!(
            pattern_target_lines(&rows, 0, &default_patterns()),
            Some(0..=0)
        );
    }

    #[test]
    fn test_pattern_at_column_zero_ends_at_next_message() {
        let rows = grid(indoc::indoc! {"
            ⏺ Q2 - Expiry
              ➡ one hour

            ⏺ Next message
              more text
            > my answer
            Q3 revenue grew
        "});
        let patterns = default_patterns();
        assert_eq!(pattern_target_lines(&rows, 0, &patterns), Some(0..=1));
        assert_eq!(pattern_target_lines(&rows, 4, &patterns), Some(3..=4));
        assert_eq!(pattern_target_lines(&rows, 6, &patterns), Some(6..=6));
    }

    #[test]
    fn test_box_borders_are_left_out_of_match_text() {
        let rows = grid("│ Q1 - Title │\n│ ──────── │\n");
        let lines = logical_lines(&rows);
        assert_eq!(match_text(&rows, &lines[0]), "Q1 - Title");
        assert_eq!(match_text(&rows, &lines[1]), "│ ──────── │");
    }
}
