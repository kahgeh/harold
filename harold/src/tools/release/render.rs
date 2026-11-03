//! Prints a `NextResponse` as text and as JSON.

use harold_api::tools::release::v1::{Line, NextResponse, ProductNext};
use serde_json::{Value, json};

/// The text of the response: one block for each product, with a blank line between.
pub(super) fn text(response: &NextResponse) -> String {
    let blocks: Vec<String> = response.products.iter().map(product_text).collect();
    if blocks.is_empty() {
        return String::new();
    }
    blocks.join("\n")
}

fn product_text(product: &ProductNext) -> String {
    let mut out = format!("{}\n", product.name);
    if !product.unavailable.is_empty() {
        out += &format!("  unavailable: {}\n", product.unavailable);
        return out;
    }
    if product.paused {
        if product.release.is_empty() {
            out += "  paused: no release yet\n";
        } else {
            out += &format!("  paused: last release {}\n", product.release);
        }
        return out;
    }
    let released = !product.release.is_empty();
    let merged_heading = if released {
        format!("merged since the release ({})", product.release)
    } else {
        "no release yet: merged in the last 14 days".to_string()
    };
    section(&mut out, &merged_heading, &product.merged, true, false);
    section(&mut out, "in progress", &product.in_progress, true, true);
    section(
        &mut out,
        "approved, not started",
        &product.approved,
        true,
        false,
    );
    section(&mut out, "parked", &product.parked, false, false);
    section(
        &mut out,
        "awaiting approval",
        &product.awaiting,
        false,
        false,
    );
    section(
        &mut out,
        "not clear (in progress, no branch)",
        &product.not_clear,
        false,
        false,
    );
    if released {
        out += &format!(
            "  other commits since {}: {}\n",
            product.release, product.other_commits
        );
    }
    out
}

/// Adds one list. An empty list prints `<heading>: none` when `always` is set and
/// nothing otherwise.
fn section(out: &mut String, heading: &str, lines: &[Line], always: bool, counts: bool) {
    if lines.is_empty() {
        if always {
            *out += &format!("  {heading}: none\n");
        }
        return;
    }
    *out += &format!("  {heading}\n");
    for line in lines {
        *out += "    ";
        *out += &line.id;
        if !line.title.is_empty() {
            *out += "  ";
            *out += &line.title;
        }
        if counts {
            let unit = if line.commits == 1 {
                "commit"
            } else {
                "commits"
            };
            *out += &format!("  ({} {unit})", line.commits);
        }
        *out += "\n";
    }
}

/// The JSON of the response, with the proto field names as keys.
pub(super) fn json(response: &NextResponse) -> String {
    let products: Vec<Value> = response.products.iter().map(product_json).collect();
    let value = json!({ "products": products });
    serde_json::to_string_pretty(&value).expect("a JSON value always prints")
}

fn product_json(product: &ProductNext) -> Value {
    json!({
        "name": product.name,
        "paused": product.paused,
        "release": product.release,
        "merged": lines_json(&product.merged, false),
        "in_progress": lines_json(&product.in_progress, true),
        "approved": lines_json(&product.approved, false),
        "parked": lines_json(&product.parked, false),
        "awaiting": lines_json(&product.awaiting, false),
        "not_clear": lines_json(&product.not_clear, false),
        "other_commits": product.other_commits,
        "unavailable": product.unavailable,
    })
}

fn lines_json(lines: &[Line], counts: bool) -> Vec<Value> {
    lines
        .iter()
        .map(|line| {
            if counts {
                json!({ "id": line.id, "title": line.title, "commits": line.commits })
            } else {
                json!({ "id": line.id, "title": line.title })
            }
        })
        .collect()
}

#[cfg(test)]
#[path = "render_tests.rs"]
mod render_tests;
