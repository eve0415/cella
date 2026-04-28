//! Terminal-aware table formatting for CLI output.
//!
//! Renders columnar data with dynamic widths. When stderr is a TTY,
//! shrinkable columns are truncated with '…' to fit the terminal.
//! When piped, natural widths are used without truncation.

use std::fmt::Write as _;
use std::io::IsTerminal;

const SEPARATOR: &str = "  ";

/// Defines a single column in a table.
pub struct Column {
    /// Header text displayed at the top.
    header: &'static str,
    /// Whether this column can be truncated to fit terminal width.
    shrinkable: bool,
    /// Minimum width when shrinking (header length is the absolute floor).
    min_width: usize,
}

impl Column {
    /// A column that never shrinks.
    pub const fn fixed(header: &'static str) -> Self {
        Self {
            header,
            shrinkable: false,
            min_width: 0,
        }
    }

    /// A column that can be truncated with '…' when the table is too wide.
    pub const fn shrinkable(header: &'static str) -> Self {
        Self {
            header,
            shrinkable: true,
            min_width: 0,
        }
    }

    /// The smallest this column can be (at least the header length).
    fn effective_min(&self) -> usize {
        self.header.len().max(self.min_width)
    }
}

/// A table that dynamically sizes columns to fit content and terminal width.
pub struct Table {
    columns: Vec<Column>,
    rows: Vec<Vec<String>>,
}

impl Table {
    pub const fn new(columns: Vec<Column>) -> Self {
        Self {
            columns,
            rows: Vec::new(),
        }
    }

    /// Add a row. The number of values must match the number of columns.
    pub fn add_row(&mut self, values: Vec<String>) {
        debug_assert_eq!(
            values.len(),
            self.columns.len(),
            "row length {} does not match column count {}",
            values.len(),
            self.columns.len(),
        );
        self.rows.push(values);
    }

    /// Render the table to a `String`.
    pub fn render(&self) -> String {
        self.render_with_width(Self::detect_term_width())
    }

    /// Render and print to stderr.
    pub fn eprint(&self) {
        eprint!("{}", self.render());
    }

    /// Render with an explicit terminal width (or `None` for no truncation).
    fn render_with_width(&self, term_width: Option<usize>) -> String {
        let ncols = self.columns.len();
        if ncols == 0 {
            return String::new();
        }

        // 1. Compute natural widths
        let mut widths: Vec<usize> = (0..ncols)
            .map(|i| {
                let max_val = self.rows.iter().map(|row| row[i].len()).max().unwrap_or(0);
                self.columns[i]
                    .header
                    .len()
                    .max(max_val)
                    .max(self.columns[i].min_width)
            })
            .collect();

        // 2. Shrink if needed
        if let Some(tw) = term_width {
            let total: usize =
                widths.iter().sum::<usize>() + SEPARATOR.len() * ncols.saturating_sub(1);
            if total > tw {
                self.shrink_columns(&mut widths, total - tw);
            }
        }

        // 3. Format output
        let mut buf = String::new();

        // Header
        let headers: Vec<String> = self.columns.iter().map(|c| c.header.to_string()).collect();
        format_line(&mut buf, &headers, &widths);
        buf.push('\n');

        // Rows
        for row in &self.rows {
            format_line(&mut buf, row, &widths);
            buf.push('\n');
        }

        buf
    }

    fn detect_term_width() -> Option<usize> {
        if !std::io::stderr().is_terminal() {
            return None;
        }
        terminal_size::terminal_size().map(|(terminal_size::Width(w), _)| w as usize)
    }

    fn shrink_columns(&self, widths: &mut [usize], excess: usize) {
        // Collect shrinkable columns and their available shrink room
        let shrinkables: Vec<(usize, usize)> = self
            .columns
            .iter()
            .enumerate()
            .filter(|(_, col)| col.shrinkable)
            .map(|(i, col)| {
                let available = widths[i].saturating_sub(col.effective_min());
                (i, available)
            })
            .collect();

        let total_available: usize = shrinkables.iter().map(|(_, a)| *a).sum();
        if total_available == 0 {
            return;
        }

        let actual_excess = excess.min(total_available);
        let mut remaining = actual_excess;

        // Proportional distribution
        for &(i, available) in &shrinkables {
            if total_available == 0 {
                break;
            }
            let reduction = (actual_excess * available / total_available).min(remaining);
            widths[i] -= reduction;
            remaining -= reduction;
        }

        // Distribute remainder one-at-a-time to columns with most room
        if remaining > 0 {
            let mut sorted: Vec<(usize, usize)> = shrinkables
                .into_iter()
                .filter(|&(i, _)| widths[i] > self.columns[i].effective_min())
                .collect();
            sorted.sort_by_key(|b| std::cmp::Reverse(b.1));

            if !sorted.is_empty() {
                for &(i, _) in sorted.iter().cycle() {
                    if remaining == 0 {
                        break;
                    }
                    if widths[i] > self.columns[i].effective_min() {
                        widths[i] -= 1;
                        remaining -= 1;
                    }
                }
            }
        }
    }
}

fn format_line(buf: &mut String, values: &[String], widths: &[usize]) {
    for (i, value) in values.iter().enumerate() {
        if i > 0 {
            buf.push_str(SEPARATOR);
        }
        let w = widths[i];
        let is_last = i == values.len() - 1;

        if value.len() <= w {
            if is_last {
                // No right-padding on the last column
                buf.push_str(value);
            } else {
                let _ = write!(buf, "{value:<w$}");
            }
        } else {
            // Truncate with ellipsis
            truncate_with_ellipsis(buf, value, w);
        }
    }
}

fn truncate_with_ellipsis(buf: &mut String, value: &str, width: usize) {
    if width > 1 {
        buf.push_str(&value[..width - 1]);
    }
    buf.push('\u{2026}');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_alignment() {
        let mut table = Table::new(vec![
            Column::fixed("NAME"),
            Column::fixed("ID"),
            Column::fixed("STATE"),
        ]);
        table.add_row(vec![
            "short".to_string(),
            "abc123".to_string(),
            "running".to_string(),
        ]);
        table.add_row(vec![
            "a-longer-name".to_string(),
            "def456".to_string(),
            "stopped".to_string(),
        ]);

        let output = table.render_with_width(None);
        insta::assert_snapshot!(output, @"
        NAME           ID      STATE
        short          abc123  running
        a-longer-name  def456  stopped
        ");
    }

    #[test]
    fn shrinkable_column_truncates() {
        let mut table = Table::new(vec![Column::shrinkable("NAME"), Column::fixed("ID")]);
        table.add_row(vec![
            "a-very-long-container-name".to_string(),
            "abc123".to_string(),
        ]);

        // Terminal width forces shrinking: header "NAME" (4) + "ID" (6) + sep (2) = 12 min
        // Natural: 26 + 6 + 2 = 34. With term_width=20, excess=14.
        let output = table.render_with_width(Some(20));
        insta::assert_snapshot!(output, @"
        NAME          ID
        a-very-long…  abc123
        ");
    }

    #[test]
    fn no_truncation_when_fits() {
        let mut table = Table::new(vec![Column::shrinkable("NAME"), Column::fixed("ID")]);
        table.add_row(vec!["short".to_string(), "abc".to_string()]);

        let output = table.render_with_width(Some(200));
        insta::assert_snapshot!(output, @"
        NAME   ID
        short  abc
        ");
    }

    #[test]
    fn empty_table_shows_header_only() {
        let table = Table::new(vec![Column::fixed("NAME"), Column::fixed("STATE")]);

        let output = table.render_with_width(None);
        insta::assert_snapshot!(output, @"NAME  STATE");
    }

    #[test]
    fn last_column_no_trailing_padding() {
        let mut table = Table::new(vec![Column::fixed("A"), Column::fixed("B")]);
        table.add_row(vec!["xx".to_string(), "y".to_string()]);

        let output = table.render_with_width(None);
        // Last column "y" should not have trailing spaces
        for line in output.lines() {
            assert!(!line.ends_with(' '), "line has trailing space: {line:?}");
        }
    }

    #[test]
    fn multiple_shrinkable_columns_proportional() {
        let mut table = Table::new(vec![
            Column::shrinkable("NAME"),
            Column::fixed("ID"),
            Column::shrinkable("PATH"),
        ]);
        table.add_row(vec![
            "name-that-is-twenty!".to_string(), // 20 chars
            "abc".to_string(),
            "path-that-is-also-twenty".to_string(), // 24 chars
        ]);

        // Natural: 20 + 3 + 24 + 4 (separators) = 51
        // Term width 41 => excess 10
        // NAME available shrink: 20 - 4 = 16
        // PATH available shrink: 24 - 4 = 20
        // Total available: 36
        // NAME reduction: floor(10 * 16 / 36) = 4 -> width 16
        // PATH reduction: floor(10 * 20 / 36) = 5 -> width 19
        // remaining: 10 - 4 - 5 = 1, goes to PATH (more available)
        let output = table.render_with_width(Some(41));
        insta::assert_snapshot!(output, @"
        NAME              ID   PATH
        name-that-is-tw…  abc  path-that-is-also…
        ");
    }

    #[test]
    fn fixed_columns_never_shrink() {
        let mut table = Table::new(vec![Column::fixed("LONGHEADER"), Column::shrinkable("X")]);
        table.add_row(vec![
            "long-value-here".to_string(),
            "shrink-me-please-now".to_string(),
        ]);

        // Natural: 15 + 20 + 2 = 37. Term 25 => excess 12.
        // Only X is shrinkable. Available: 20 - 1 = 19.
        // Reduction: min(12, 19) = 12. X width: 20 - 12 = 8.
        let output = table.render_with_width(Some(25));
        insta::assert_snapshot!(output, @"
        LONGHEADER       X
        long-value-here  shrink-…
        ");
    }

    #[test]
    fn empty_columns_returns_empty_string() {
        let table = Table::new(vec![]);
        let output = table.render_with_width(None);
        assert!(output.is_empty());
    }

    #[test]
    fn single_column_no_separator() {
        let mut table = Table::new(vec![Column::fixed("VALUE")]);
        table.add_row(vec!["hello".to_string()]);
        let output = table.render_with_width(None);
        insta::assert_snapshot!(output, @"
        VALUE
        hello
        ");
    }

    #[test]
    fn truncate_with_ellipsis_width_1() {
        let mut buf = String::new();
        truncate_with_ellipsis(&mut buf, "hello world", 1);
        assert_eq!(buf, "\u{2026}");
    }

    #[test]
    fn truncate_with_ellipsis_width_2() {
        let mut buf = String::new();
        truncate_with_ellipsis(&mut buf, "hello world", 2);
        assert_eq!(buf, "h\u{2026}");
    }

    #[test]
    fn column_effective_min_uses_header_length() {
        let col = Column::fixed("NAME");
        assert_eq!(col.effective_min(), 4);
    }

    #[test]
    fn column_effective_min_uses_min_width_if_larger() {
        let col = Column {
            header: "X",
            shrinkable: true,
            min_width: 10,
        };
        assert_eq!(col.effective_min(), 10);
    }

    #[test]
    fn column_effective_min_uses_header_if_longer() {
        let col = Column {
            header: "LONGHEADER",
            shrinkable: true,
            min_width: 3,
        };
        assert_eq!(col.effective_min(), 10);
    }

    #[test]
    fn table_with_empty_values() {
        let mut table = Table::new(vec![Column::fixed("A"), Column::fixed("B")]);
        table.add_row(vec![String::new(), String::new()]);
        let output = table.render_with_width(None);
        // Should still render with proper spacing
        assert!(output.contains('A'));
        assert!(output.contains('B'));
    }

    #[test]
    fn extreme_shrink_all_columns_to_min() {
        let mut table = Table::new(vec![
            Column::shrinkable("ABCDE"),
            Column::shrinkable("FGHIJ"),
        ]);
        table.add_row(vec![
            "very-long-value-here".to_string(),
            "another-long-value".to_string(),
        ]);
        // Terminal width so small both columns shrink to header length
        let output = table.render_with_width(Some(12));
        // Both columns should be truncated
        assert!(output.contains('\u{2026}'));
    }

    #[test]
    fn no_shrinkable_columns_excess_ignored() {
        let mut table = Table::new(vec![Column::fixed("AAAA"), Column::fixed("BBBB")]);
        table.add_row(vec![
            "long-value-aaaa".to_string(),
            "long-value-bbbb".to_string(),
        ]);
        // Even though term is narrow, fixed columns never shrink
        let output = table.render_with_width(Some(10));
        // Should still render full values without truncation
        assert!(output.contains("long-value-aaaa"));
        assert!(output.contains("long-value-bbbb"));
    }

    #[test]
    fn multiple_rows_alignment() {
        let mut table = Table::new(vec![Column::fixed("ID"), Column::fixed("NAME")]);
        table.add_row(vec!["1".to_string(), "a".to_string()]);
        table.add_row(vec!["22".to_string(), "bb".to_string()]);
        table.add_row(vec!["333".to_string(), "ccc".to_string()]);
        let output = table.render_with_width(None);
        insta::assert_snapshot!(output, @"
        ID   NAME
        1    a
        22   bb
        333  ccc
        ");
    }
}
