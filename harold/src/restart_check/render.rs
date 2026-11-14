//! The text of the restart check. The printed output and the receipt file hold this text.

use super::*;

/// The longest name that goes into a line.
const NAME_LIMIT: usize = 80;
/// The gap between two columns.
const GAP: &str = "   ";

/// A name with each control character and newline changed to a space, cut to 80 characters.
pub(crate) fn clean_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(NAME_LIMIT)
        .collect()
}

fn word(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Ok => "ok",
        Outcome::Fixed => "fixed",
        Outcome::Fail => "FAIL",
        Outcome::NotChecked => "not checked",
    }
}

/// The report as text, with the groups of stage 1.
pub(crate) fn render(report: &Report) -> String {
    let mut out = format!(
        "restart check: {}  (session {}, pane {})  {}\n",
        report.agent.title(),
        report.session,
        report.pane,
        report.time
    );
    out.push_str("identity\n");
    let identity = &report.identity;
    out.push_str(&format!(
        "  {:<6}{}{GAP}{}\n",
        word(identity.outcome),
        identity.subject,
        identity.detail
    ));
    push_fails(&mut out, report, Group::Identity);
    out.push_str("\ncommunication\n");
    push_table(&mut out, &report.communication);
    push_fails(&mut out, report, Group::Communication);
    if !report.not_checked.is_empty() {
        out.push('\n');
        for line in &report.not_checked {
            out.push_str(&format!("  not checked {line}\n"));
        }
    }
    out.push('\n');
    out.push_str(&result_line(report));
    out
}

fn push_fails(out: &mut String, report: &Report, group: Group) {
    let lines: Vec<_> = report.fails.iter().filter(|f| f.group == group).collect();
    if lines.is_empty() {
        return;
    }
    out.push('\n');
    for line in lines {
        out.push_str(&format!("  FAIL {}\n", line.text));
    }
}

fn push_table(out: &mut String, table: &LinkTable) {
    let mut grid: Vec<Vec<String>> = vec![table.header.clone()];
    for row in &table.rows {
        let mut cells = row.label.clone();
        cells.push(word(row.outgoing).into());
        cells.push(word(row.incoming).into());
        grid.push(cells);
    }
    let columns = table.header.len();
    let widths: Vec<usize> = (0..columns)
        .map(|column| {
            grid.iter()
                .map(|cells| cells.get(column).map_or(0, |cell| cell.chars().count()))
                .max()
                .unwrap_or(0)
        })
        .collect();
    for cells in &grid {
        let mut line = String::from("  ");
        for (column, width) in widths.iter().enumerate() {
            let cell = cells.get(column).map_or("", String::as_str);
            line.push_str(cell);
            if column + 1 < columns {
                line.push_str(&" ".repeat(width - cell.chars().count()));
                line.push_str(GAP);
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
}

fn result_line(report: &Report) -> String {
    let count = report.fails.len();
    if count == 0 {
        return "result: ok\n".into();
    }
    let target = match report.agent.recipient() {
        Recipient::Chief if report.agent == Agent::Chief => {
            let who = match count {
                1 => "it",
                2 => "both",
                _ => "all",
            };
            format!("the chief fixes {who}; nothing for the user")
        }
        Recipient::Chief => "sent to the chief".into(),
        Recipient::PodCoordination => "sent to pod-coordination".into(),
    };
    format!("result: {count} FAIL -> {target}\n")
}

#[cfg(test)]
#[path = "render_tests.rs"]
mod tests;
