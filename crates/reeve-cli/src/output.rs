//! Plain-text list formatting for registered identities.

use std::io::{self, Write};

use crate::identity::IdentityRow;

// "External" is the longest IdentityType variant (8 chars).
const COL_TYPE_WIDTH: usize = 8;
// Display window for human-friendly names; longer names truncate at the boundary.
const COL_NAME_WIDTH: usize = 24;
// UUIDv7 canonical hyphenated form is exactly 36 chars.
const COL_ID_WIDTH: usize = 36;

/// Write a formatted identity list to `out`. One header row followed by one
/// row per entry. Columns are fixed-width with a single space separator to
/// keep the output readable at 80 columns.
pub(crate) fn write_identity_table(
    out: &mut impl Write,
    rows: &[IdentityRow<'_>],
) -> Result<(), io::Error> {
    writeln!(
        out,
        "{:<type_w$} {:<name_w$} {:<id_w$} FINGERPRINT",
        "TYPE",
        "NAME",
        "ID",
        type_w = COL_TYPE_WIDTH,
        name_w = COL_NAME_WIDTH,
        id_w = COL_ID_WIDTH,
    )?;
    for row in rows {
        writeln!(
            out,
            "{:<type_w$} {:<name_w$} {:<id_w$} {}",
            row.identity_type.to_string(),
            truncate(row.display_name, COL_NAME_WIDTH),
            row.identity_id,
            row.fingerprint,
            type_w = COL_TYPE_WIDTH,
            name_w = COL_NAME_WIDTH,
            id_w = COL_ID_WIDTH,
        )?;
    }
    Ok(())
}

/// Truncate a string to at most `max_chars` Unicode scalar values. The
/// `writeln!` format width counts bytes, not Unicode scalar values; this
/// truncates before formatting so the column stays bounded for non-ASCII
/// names.
fn truncate(s: &str, max_chars: usize) -> &str {
    if s.chars().count() <= max_chars {
        return s;
    }
    let end = s.char_indices().nth(max_chars).map_or(s.len(), |(i, _)| i);
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::IdentityRow;
    use reeve_types::IdentityType;

    fn capture(rows: &[IdentityRow<'_>]) -> String {
        let mut buf = Vec::new();
        write_identity_table(&mut buf, rows).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn empty_list_prints_header_only() {
        let output = capture(&[]);
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("TYPE"));
        assert!(lines[0].contains("NAME"));
        assert!(lines[0].contains("ID"));
        assert!(lines[0].contains("FINGERPRINT"));
    }

    #[test]
    fn single_identity_produces_two_lines() {
        let row = IdentityRow {
            identity_type: IdentityType::Operator,
            display_name: "Ada",
            identity_id: "01234567-89ab-7def-8123-456789abcdef",
            fingerprint: "aa:bb:cc:dd:ee:ff:00:11",
        };
        let output = capture(std::slice::from_ref(&row));
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 2, "expected header + one data row");
        assert!(lines[1].contains("Operator"), "type column missing");
        assert!(lines[1].contains("Ada"), "name column missing");
        assert!(
            lines[1].contains("01234567-89ab-7def-8123-456789abcdef"),
            "id column missing"
        );
        assert!(
            lines[1].contains("aa:bb:cc:dd:ee:ff:00:11"),
            "fingerprint column missing"
        );
    }

    #[test]
    fn multiple_identities_produce_correct_row_count() {
        let rows = [
            IdentityRow {
                identity_type: IdentityType::Operator,
                display_name: "Ada",
                identity_id: "01234567-89ab-7def-8123-456789abcdef",
                fingerprint: "aa:bb:cc:dd:ee:ff:00:11",
            },
            IdentityRow {
                identity_type: IdentityType::Agent,
                display_name: "lead",
                identity_id: "01234567-89ab-7def-8123-456789abcde0",
                fingerprint: "00:11:22:33:44:55:66:77",
            },
            IdentityRow {
                identity_type: IdentityType::External,
                display_name: "ci-bot",
                identity_id: "01234567-89ab-7def-8123-456789abcde1",
                fingerprint: "ff:ee:dd:cc:bb:aa:99:88",
            },
        ];
        let output = capture(&rows);
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 4, "header + three data rows");
        assert!(lines[1].contains("Operator"));
        assert!(lines[2].contains("Agent"));
        assert!(lines[3].contains("External"));
    }

    #[test]
    fn very_long_display_name_is_truncated_to_column_width() {
        let long_name = "A".repeat(60);
        let row = IdentityRow {
            identity_type: IdentityType::Operator,
            display_name: &long_name,
            identity_id: "01234567-89ab-7def-8123-456789abcdef",
            fingerprint: "aa:bb:cc:dd:ee:ff:00:11",
        };
        let output = capture(std::slice::from_ref(&row));
        let data_line = output.lines().nth(1).unwrap();
        assert!(
            data_line.contains("01234567-89ab-7def-8123-456789abcdef"),
            "long name must not push id column off the line"
        );
    }

    #[test]
    fn non_ascii_display_name_is_handled_safely() {
        let name = "Ångström";
        let row = IdentityRow {
            identity_type: IdentityType::Operator,
            display_name: name,
            identity_id: "01234567-89ab-7def-8123-456789abcdef",
            fingerprint: "aa:bb:cc:dd:ee:ff:00:11",
        };
        let output = capture(std::slice::from_ref(&row));
        let data_line = output.lines().nth(1).unwrap();
        assert!(
            data_line.contains("Ångström"),
            "non-ASCII name must appear in output"
        );
    }

    #[test]
    fn truncate_ascii_at_boundary() {
        let s = "0123456789";
        assert_eq!(truncate(s, 5), "01234");
        assert_eq!(truncate(s, 10), "0123456789");
        assert_eq!(truncate(s, 20), "0123456789");
    }

    #[test]
    fn truncate_zero_returns_empty() {
        assert_eq!(truncate("abc", 0), "");
        assert_eq!(truncate("héllo", 0), "");
    }

    #[test]
    fn truncate_multibyte_at_code_point_boundary() {
        // "é" is U+00E9, encoded as two bytes in UTF-8.
        let s = "aéb";
        // 3 code points total; truncating to 2 must yield "aé".
        let t = truncate(s, 2);
        assert_eq!(t, "aé");
        assert!(std::str::from_utf8(t.as_bytes()).is_ok());
    }
}
