//! Run artifact export — the local equivalent of `test artifact get`.
//!
//! The executor already records the meaningful evidence in `runs.code`: backend
//! QA artifacts (`testsprite-qa-artifact`) or the executed script/source. Browser
//! runs also leave screenshots under `testsprite_tests/shots`. This module turns
//! one `run_id` into a portable directory bundle for an agent/user to inspect.

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use serde_json::{Value, json};

/// Export a run bundle into `out_dir`. Returns the directory written.
pub async fn get(root: &Path, run_id: i64, out_dir: &Path) -> anyhow::Result<PathBuf> {
    let Some(run) = load_run(root, run_id).await? else {
        return Err(anyhow!("no run_id {run_id}"));
    };
    std::fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;

    let bundle = json!({
        "run": run,
        "notes": "Secrets are redacted by the executor before artifacts are stored.",
    });
    std::fs::write(
        out_dir.join("run.json"),
        serde_json::to_string_pretty(&bundle)?,
    )?;

    if let Some(code) = run.get("code").and_then(Value::as_str)
        && !code.trim().is_empty()
    {
        let name = if code.contains("testsprite-qa-artifact") {
            "qa-artifact.json"
        } else {
            "executed-artifact.txt"
        };
        std::fs::write(out_dir.join(name), code)?;
    }

    if let Some(test_id) = run.get("test_id").and_then(Value::as_str) {
        copy_screenshots(root, test_id, out_dir)?;
        copy_videos(root, test_id, out_dir)?;
    }

    Ok(out_dir.to_path_buf())
}

/// Latest runs report, markdown or JSON-ready value.
pub async fn report(root: &Path) -> anyhow::Result<Value> {
    let results = crate::local::store::latest_results(root).await?;
    let total = results.len();
    let passed = results
        .iter()
        .filter(|r| r.get("passed").and_then(Value::as_bool).unwrap_or(false))
        .count();
    let failed = total.saturating_sub(passed);
    Ok(json!({
        "total": total,
        "passed": passed,
        "failed": failed,
        "results": results,
        "clusters": crate::local::triage::triage(root).await?,
    }))
}

pub async fn write_report(root: &Path, out: &Path, json_out: bool) -> anyhow::Result<PathBuf> {
    let value = report(root).await?;
    if let Some(parent) = out.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let body = if json_out {
        serde_json::to_string_pretty(&value)?.into_bytes()
    } else if out.extension().and_then(|e| e.to_str()) == Some("pdf") {
        render_pdf(&value)
    } else {
        render_markdown(&value).into_bytes()
    };
    std::fs::write(out, body)?;
    Ok(out.to_path_buf())
}

/// Write a static dashboard HTML: pass counts, test list, failure clusters.
pub async fn write_dashboard(root: &Path, out: &Path) -> anyhow::Result<PathBuf> {
    let tests = crate::local::store::list(root).await?;
    let report = report(root).await?;
    let latest = report
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut by_id = std::collections::BTreeMap::new();
    for r in latest {
        if let Some(id) = r.get("id").and_then(Value::as_str) {
            by_id.insert(id.to_string(), r);
        }
    }
    if let Some(parent) = out.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let body = render_dashboard(&tests, &by_id, &report);
    std::fs::write(out, body)?;
    Ok(out.to_path_buf())
}

/// Write a PRD + generated plan review page for the human approval step.
pub async fn write_prd_review(root: &Path, id: &str, out: &Path) -> anyhow::Result<PathBuf> {
    let prd = crate::local::store::load_prd(root, id)
        .await?
        .ok_or_else(|| anyhow!("no PRD {id}"))?;
    if let Some(parent) = out.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(out, render_prd_review(&prd))?;
    Ok(out.to_path_buf())
}

/// Write a tiny visual replay HTML for a frontend test's `planSteps` and
/// screenshots. No server/dashboard needed: open the file in a browser.
pub async fn write_replay(root: &Path, id: &str, out: &Path) -> anyhow::Result<PathBuf> {
    let test = crate::local::store::load_one(root, id).await?;
    if let Some(parent) = out.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let steps = test
        .extra
        .get("planSteps")
        .or_else(|| test.extra.get("steps"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let shots = screenshots(root, id)?;
    let vids = videos(root, id)?;
    let mut html = String::from(
        "<!doctype html><meta charset=utf-8><title>TestSprite Replay</title>\
         <style>body{font-family:system-ui;margin:2rem;background:#111;color:#eee}\
         .step{border:1px solid #284b31;border-radius:10px;padding:1rem;margin:1rem 0;background:#18231b}\
         img{max-width:100%;border:1px solid #333;border-radius:8px;margin-top:.75rem}code{color:#8fca8f}</style>",
    );
    html.push_str(&format!(
        "<h1>{}</h1><p><code>{}</code></p>",
        html_escape(&test.title),
        html_escape(id)
    ));
    for (i, step) in steps.iter().enumerate() {
        html.push_str("<div class=step>");
        html.push_str(&format!("<h2>Step {}</h2>", i + 1));
        html.push_str(&format!(
            "<pre>{}</pre>",
            html_escape(&serde_json::to_string_pretty(step).unwrap_or_else(|_| step.to_string()))
        ));
        if let Some(shot) = shots.get(i) {
            html.push_str(&format!(
                "<img src=\"{}\" alt=\"step {} screenshot\">",
                html_escape(&rel_for_html(out, shot)),
                i + 1
            ));
        }
        html.push_str("</div>");
    }
    if steps.is_empty() {
        html.push_str("<p>No planSteps stored for this test.</p>");
    }
    if let Some(final_shot) = shots.last() {
        html.push_str(&format!(
            "<h2>Final screenshot</h2><img src=\"{}\" alt=\"final screenshot\">",
            html_escape(&rel_for_html(out, final_shot))
        ));
    }
    if let Some(video) = vids.last() {
        html.push_str(&format!(
            "<h2>Recording</h2><video controls src=\"{}\" style=\"max-width:100%\"></video>",
            html_escape(&rel_for_html(out, video))
        ));
    }
    std::fs::write(out, html)?;
    Ok(out.to_path_buf())
}

async fn load_run(root: &Path, run_id: i64) -> anyhow::Result<Option<Value>> {
    let pool = crate::local::db::open(root).await?;
    type RunRow = (
        i64,
        String,
        String,
        Option<String>,
        i64,
        Option<String>,
        Option<String>,
        String,
        String,
        Option<String>,
    );
    let row: Option<RunRow> = sqlx::query_as(
        "SELECT r.run_id,r.test_id,t.title,t.kind,r.passed,r.verdict,r.failure_kind,r.error,r.code,r.analysis \
         FROM runs r LEFT JOIN tests t ON t.id=r.test_id WHERE r.run_id=?",
    )
    .bind(run_id)
    .fetch_optional(&pool)
    .await?;

    Ok(row.map(
        |(run_id, test_id, title, kind, passed, verdict, failure_kind, error, code, analysis)| {
            let mut v = json!({
                "run_id": run_id,
                "test_id": test_id,
                "title": title,
                "kind": kind,
                "passed": passed != 0,
                "verdict": verdict,
                "failureKind": failure_kind,
                "error": error,
                "code": code,
            });
            if let Some(a) = analysis.and_then(|s| serde_json::from_str::<Value>(&s).ok()) {
                v["analysis"] = a;
            }
            v
        },
    ))
}

fn copy_screenshots(root: &Path, test_id: &str, out_dir: &Path) -> anyhow::Result<()> {
    let shots = screenshots(root, test_id)?;
    let dst = out_dir.join("screenshots");
    for shot in shots {
        let Some(name) = shot.file_name() else {
            continue;
        };
        std::fs::create_dir_all(&dst)?;
        let _ = std::fs::copy(&shot, dst.join(name));
    }
    Ok(())
}

fn copy_videos(root: &Path, test_id: &str, out_dir: &Path) -> anyhow::Result<()> {
    let vids = videos(root, test_id)?;
    let dst = out_dir.join("videos");
    for video in vids {
        let Some(name) = video.file_name() else {
            continue;
        };
        std::fs::create_dir_all(&dst)?;
        let _ = std::fs::copy(&video, dst.join(name));
    }
    Ok(())
}

fn screenshots(root: &Path, test_id: &str) -> anyhow::Result<Vec<PathBuf>> {
    let dir = crate::local::ts_dir(root).join("shots");
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(test_id) {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

fn videos(root: &Path, test_id: &str) -> anyhow::Result<Vec<PathBuf>> {
    let dir = crate::local::ts_dir(root).join("videos");
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(test_id) {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

fn rel_for_html(html: &Path, target: &Path) -> String {
    let base = html.parent().unwrap_or_else(|| Path::new("."));
    target
        .strip_prefix(base)
        .unwrap_or(target)
        .to_string_lossy()
        .replace('\\', "/")
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn render_dashboard(
    tests: &[crate::local::LocalTest],
    latest: &std::collections::BTreeMap<String, Value>,
    report: &Value,
) -> String {
    let total = tests.len();
    let passed = report.get("passed").and_then(Value::as_u64).unwrap_or(0);
    let failed = report.get("failed").and_then(Value::as_u64).unwrap_or(0);
    let never = total.saturating_sub(latest.len());
    let mut html = format!(
        "<!doctype html><meta charset=utf-8><title>TestSprite Dashboard</title>\
         <style>body{{font-family:system-ui;margin:0;background:#101512;color:#edf7ef}}\
         aside{{position:fixed;inset:0 auto 0 0;width:230px;background:#162018;padding:24px}}\
         main{{margin-left:278px;padding:32px}}.card{{background:#17231b;border:1px solid #2e5c3a;border-radius:14px;padding:18px;margin:12px 0}}\
         table{{border-collapse:collapse;width:100%;background:#111a14}}td,th{{border-bottom:1px solid #29382d;padding:10px;text-align:left}}\
         .pass{{color:#51c878}}.fail{{color:#ff6b6b}}.never{{color:#d5c65e}}code{{color:#9ee6ad}}</style>\
         <aside><h2>TestSprite</h2><p>Local dashboard</p><p><a href='testsprite-report.md'>Markdown report</a></p><p><a href='testsprite-report.pdf'>PDF report</a></p></aside>\
         <main><h1>Dashboard</h1><div class=card><b>{passed}/{total}</b> pass · <b>{failed}</b> failed · <b>{never}</b> never run</div>",
    );
    html.push_str("<div class=card><h2>Recent tests</h2><table><tr><th>Test</th><th>Kind</th><th>Group</th><th>Status</th><th>Failure</th></tr>");
    for t in tests {
        let r = latest.get(&t.id);
        let passed = r.and_then(|r| r.get("passed")).and_then(Value::as_bool);
        let status = match passed {
            Some(true) => "<span class=pass>Pass</span>".to_string(),
            Some(false) => "<span class=fail>Failed</span>".to_string(),
            None => "<span class=never>Never run</span>".to_string(),
        };
        let fk = r
            .and_then(|r| r.get("failureKind"))
            .and_then(Value::as_str)
            .unwrap_or("-");
        let kind = t
            .kind
            .as_ref()
            .and_then(|k| serde_json::to_value(k).ok())
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        html.push_str(&format!(
            "<tr><td><code>{}</code><br>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            html_escape(&t.id),
            html_escape(&t.title),
            html_escape(&kind),
            html_escape(t.group().unwrap_or("-")),
            status,
            html_escape(fk),
        ));
    }
    html.push_str("</table></div>");
    if let Some(clusters) = report.get("clusters").and_then(Value::as_array)
        && !clusters.is_empty()
    {
        html.push_str("<div class=card><h2>Failure clusters</h2><ul>");
        for c in clusters {
            html.push_str(&format!(
                "<li><code>{}</code> ×{}</li>",
                html_escape(
                    c.get("failure_kind")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                ),
                c.get("count").and_then(Value::as_u64).unwrap_or(0)
            ));
        }
        html.push_str("</ul></div>");
    }
    html.push_str("</main>");
    html
}

fn render_prd_review(prd: &Value) -> String {
    let id = prd.get("id").and_then(Value::as_str).unwrap_or("");
    let source = prd.get("source").and_then(Value::as_str).unwrap_or("");
    let approved = prd
        .get("approvedAt")
        .and_then(Value::as_str)
        .unwrap_or("not approved");
    let mut html = format!(
        "<!doctype html><meta charset=utf-8><title>TestSprite PRD Review</title>\
         <style>body{{font-family:system-ui;margin:2rem;background:#101512;color:#edf7ef}}\
         .card{{background:#17231b;border:1px solid #2e5c3a;border-radius:14px;padding:18px;margin:12px 0}}\
         pre{{white-space:pre-wrap;background:#0b100d;padding:1rem;border-radius:10px}}\
         li{{margin:.4rem 0}}code{{color:#9ee6ad}}</style>\
         <h1>PRD Review</h1><p><code>{}</code></p><p>source: {} · approval: {}</p>",
        html_escape(id),
        html_escape(source),
        html_escape(approved)
    );
    html.push_str("<div class=card><h2>Requirements</h2>");
    html.push_str(&format!(
        "<pre>{}</pre>",
        html_escape(&serde_json::to_string_pretty(&prd["prd"]).unwrap_or_default())
    ));
    html.push_str("</div><div class=card><h2>Generated test plan</h2><ol>");
    if let Some(plan) = prd.get("plan").and_then(Value::as_array) {
        for case in plan {
            let title = case
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("untitled");
            let desc = case
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("");
            html.push_str(&format!(
                "<li><b>{}</b><br>{}</li>",
                html_escape(title),
                html_escape(desc)
            ));
        }
    }
    html.push_str("</ol></div><p>Approve after review: <code>testsprite-rs prd approve ");
    html.push_str(&html_escape(id));
    html.push_str("</code></p>");
    html
}

fn render_markdown(v: &Value) -> String {
    let total = v.get("total").and_then(Value::as_u64).unwrap_or(0);
    let passed = v.get("passed").and_then(Value::as_u64).unwrap_or(0);
    let failed = v.get("failed").and_then(Value::as_u64).unwrap_or(0);
    let mut out = format!(
        "# TestSprite Report\n\n**Summary:** {passed}/{total} passed ({failed} failed)\n\n"
    );
    out.push_str("## Results\n\n| Test | Verdict | Failure kind | Error |\n|---|---|---|---|\n");
    if let Some(rows) = v.get("results").and_then(Value::as_array) {
        for r in rows {
            let title = r.get("title").and_then(Value::as_str).unwrap_or("");
            let verdict = r
                .get("verdict")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let fk = r.get("failureKind").and_then(Value::as_str).unwrap_or("-");
            let err = r.get("error").and_then(Value::as_str).unwrap_or("");
            out.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                md_cell(title),
                md_cell(verdict),
                md_cell(fk),
                md_cell(&err.chars().take(160).collect::<String>())
            ));
        }
    }
    if let Some(clusters) = v.get("clusters").and_then(Value::as_array)
        && !clusters.is_empty()
    {
        out.push_str("\n## Failure clusters\n\n");
        for c in clusters {
            out.push_str(&format!(
                "- `{}` ×{}\n",
                c.get("failure_kind")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
                c.get("count").and_then(Value::as_u64).unwrap_or(0)
            ));
        }
    }
    out
}

fn md_cell(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

fn render_pdf(v: &Value) -> Vec<u8> {
    let lines = report_lines(v);
    let pages: Vec<Vec<String>> = lines.chunks(46).map(|c| c.to_vec()).collect();
    let page_count = pages.len().max(1);
    let mut objects: Vec<String> = Vec::new();
    objects.push("<< /Type /Catalog /Pages 2 0 R >>".to_string());
    let kids = (0..page_count)
        .map(|i| format!("{} 0 R", 4 + i * 2))
        .collect::<Vec<_>>()
        .join(" ");
    objects.push(format!(
        "<< /Type /Pages /Kids [{kids}] /Count {page_count} >>"
    ));
    objects.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string());
    for (i, page_lines) in pages.iter().enumerate() {
        let page_id = 4 + i * 2;
        let content_id = page_id + 1;
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents {content_id} 0 R >>"
        ));
        let mut stream = String::from("BT /F1 10 Tf 50 760 Td 14 TL\n");
        for line in page_lines {
            let clipped: String = line.chars().take(110).collect();
            stream.push_str(&format!("({}) Tj T*\n", pdf_escape(&clipped)));
        }
        stream.push_str("ET\n");
        objects.push(format!(
            "<< /Length {} >>\nstream\n{}endstream",
            stream.len(),
            stream
        ));
    }

    let mut out = Vec::new();
    out.extend_from_slice(b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n");
    let mut offsets = Vec::with_capacity(objects.len() + 1);
    offsets.push(0usize);
    for (i, obj) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{}\nendobj\n", i + 1, obj).as_bytes());
    }
    let xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for off in offsets.iter().skip(1) {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

fn report_lines(v: &Value) -> Vec<String> {
    let total = v.get("total").and_then(Value::as_u64).unwrap_or(0);
    let passed = v.get("passed").and_then(Value::as_u64).unwrap_or(0);
    let failed = v.get("failed").and_then(Value::as_u64).unwrap_or(0);
    let mut lines = vec![
        "TestSprite Report".to_string(),
        format!("Summary: {passed}/{total} passed ({failed} failed)"),
        String::new(),
        "Results".to_string(),
    ];
    if let Some(rows) = v.get("results").and_then(Value::as_array) {
        for r in rows {
            let title = r.get("title").and_then(Value::as_str).unwrap_or("");
            let verdict = r
                .get("verdict")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let fk = r.get("failureKind").and_then(Value::as_str).unwrap_or("-");
            let err = r.get("error").and_then(Value::as_str).unwrap_or("");
            lines.push(format!(
                "- {title} [{verdict}/{fk}] {}",
                err.chars().take(140).collect::<String>()
            ));
        }
    }
    if let Some(clusters) = v.get("clusters").and_then(Value::as_array)
        && !clusters.is_empty()
    {
        lines.push(String::new());
        lines.push("Failure clusters".to_string());
        for c in clusters {
            lines.push(format!(
                "- {} x{}",
                c.get("failure_kind")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
                c.get("count").and_then(Value::as_u64).unwrap_or(0)
            ));
        }
    }
    lines
}

fn pdf_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('(', "\\(")
        .replace(')', "\\)")
        .chars()
        .map(|c| if c.is_ascii() { c } else { '?' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::executors::{Outcome, TestKind};

    #[tokio::test]
    async fn artifact_get_writes_run_json_and_qa_artifact() {
        let root = crate::local::tmp_root();
        crate::local::store::add_value(&root, json!({"id":"t1","title":"T"}))
            .await
            .unwrap();
        crate::local::store::write_result(
            &root,
            "t1",
            &Outcome::pass("{\"kind\":\"testsprite-qa-artifact\"}".to_string()),
            None,
            TestKind::Backend,
        )
        .await
        .unwrap();
        let run_id: i64 = sqlx::query_scalar("SELECT max(run_id) FROM runs")
            .fetch_one(&crate::local::db::open(&root).await.unwrap())
            .await
            .unwrap();
        let out = root.join("bundle");
        let videos = crate::local::ts_dir(&root).join("videos");
        std::fs::create_dir_all(&videos).unwrap();
        std::fs::write(videos.join("t1-chromium.webm"), b"webm").unwrap();
        get(&root, run_id, &out).await.unwrap();
        assert!(out.join("run.json").exists());
        assert!(out.join("qa-artifact.json").exists());
        assert!(out.join("videos/t1-chromium.webm").exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn report_markdown_lists_results() {
        let root = crate::local::tmp_root();
        crate::local::store::add_value(&root, json!({"id":"t1","title":"T"}))
            .await
            .unwrap();
        crate::local::store::write_result(
            &root,
            "t1",
            &Outcome::fail("boom", String::new()),
            None,
            TestKind::Backend,
        )
        .await
        .unwrap();
        let out = root.join("report.md");
        write_report(&root, &out, false).await.unwrap();
        let body = std::fs::read_to_string(out).unwrap();
        assert!(body.contains("# TestSprite Report"));
        assert!(body.contains("T"));
        assert!(body.contains("boom"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn report_pdf_writes_a_pdf_file() {
        let root = crate::local::tmp_root();
        crate::local::store::add_value(&root, json!({"id":"t1","title":"PDF T"}))
            .await
            .unwrap();
        crate::local::store::write_result(
            &root,
            "t1",
            &Outcome::fail("pdf boom", String::new()),
            None,
            TestKind::Backend,
        )
        .await
        .unwrap();
        let out = root.join("report.pdf");
        write_report(&root, &out, false).await.unwrap();
        let body = std::fs::read(out).unwrap();
        assert!(body.starts_with(b"%PDF-1.4"));
        assert!(String::from_utf8_lossy(&body).contains("PDF T"));
        assert!(String::from_utf8_lossy(&body).contains("startxref"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn dashboard_html_lists_tests_and_statuses() {
        let root = crate::local::tmp_root();
        crate::local::store::add_value(
            &root,
            json!({"id":"ok","title":"OK test","kind":"backend","group":"smoke"}),
        )
        .await
        .unwrap();
        crate::local::store::add_value(
            &root,
            json!({"id":"new","title":"Never","kind":"frontend"}),
        )
        .await
        .unwrap();
        crate::local::store::write_result(
            &root,
            "ok",
            &Outcome::pass(String::new()),
            None,
            TestKind::Backend,
        )
        .await
        .unwrap();
        let out = root.join("dashboard.html");
        write_dashboard(&root, &out).await.unwrap();
        let html = std::fs::read_to_string(out).unwrap();
        assert!(html.contains("TestSprite"));
        assert!(html.contains("OK test"));
        assert!(html.contains("smoke"));
        assert!(html.contains("Pass"));
        assert!(html.contains("Never run"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn prd_review_html_lists_requirements_and_plan() {
        let root = crate::local::tmp_root();
        let prd = json!({"product_overview":"todo app","features":[{"name":"Todos"}]});
        let plan = vec![json!({"id":"TC001","title":"create todo","description":"POST creates"})];
        let id = crate::local::store::save_prd(&root, "instruction:todo", &prd, &plan)
            .await
            .unwrap();
        let out = root.join("review.html");
        write_prd_review(&root, &id, &out).await.unwrap();
        let html = std::fs::read_to_string(out).unwrap();
        assert!(html.contains("PRD Review"));
        assert!(html.contains("todo app"));
        assert!(html.contains("create todo"));
        assert!(html.contains("prd approve"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn replay_html_lists_steps_and_screenshots() {
        let root = crate::local::tmp_root();
        crate::local::store::add_value(
            &root,
            json!({"id":"front","title":"Login","kind":"frontend","planSteps":["Input Email","Click Sign In"]}),
        )
        .await
        .unwrap();
        let shots = crate::local::ts_dir(&root).join("shots");
        std::fs::create_dir_all(&shots).unwrap();
        std::fs::write(shots.join("front-chromium-step01.png"), b"png").unwrap();
        let videos = crate::local::ts_dir(&root).join("videos");
        std::fs::create_dir_all(&videos).unwrap();
        std::fs::write(videos.join("front-chromium.webm"), b"webm").unwrap();
        let out = root.join("replay.html");
        write_replay(&root, "front", &out).await.unwrap();
        let html = std::fs::read_to_string(out).unwrap();
        assert!(html.contains("Login"));
        assert!(html.contains("Input Email"));
        assert!(html.contains("front-chromium-step01.png"));
        assert!(html.contains("front-chromium.webm"));
        std::fs::remove_dir_all(&root).ok();
    }
}
