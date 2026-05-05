use buffr_view_source::render;

/// When a URL has no file extension (or no recognisable grammar), the output
/// must contain the source text HTML-escaped but must not contain any
/// `<span class="hl-` elements produced by the highlighter.
#[test]
fn render_plain_when_no_extension() {
    let html = render("data:foo", b"hello world");
    assert!(
        html.contains("hello world"),
        "source text must appear in output"
    );
    assert!(
        !html.contains(r#"<span class="hl-"#),
        "no highlight spans expected when no grammar matches"
    );
}

/// The renderer must HTML-escape `<`, `>`, `&`, `"`, and `'` so that
/// arbitrary source cannot inject markup into the rendered page.
#[test]
fn render_html_escapes_dangerous_chars() {
    let html = render("foo.txt", b"<script>alert(1)</script>");
    assert!(
        !html.contains("<script>"),
        "raw <script> tag must not appear in output"
    );
    assert!(
        html.contains("&lt;script&gt;"),
        "escaped script tag must appear in output"
    );
}

/// Sources above 10 MiB must not crash and must surface the size-cap notice.
#[test]
fn render_oversized_source() {
    let big = vec![b'x'; 11 * 1024 * 1024];
    let html = render("foo.rs", &big);
    assert!(
        html.contains("source too large to highlight"),
        "size-cap notice must appear for oversized sources"
    );
}

/// Quick sanity check that every render produces well-formed HTML5 structure.
#[test]
fn render_returns_well_formed_html() {
    let html = render("foo.txt", b"hello");
    assert!(
        html.starts_with("<!DOCTYPE html>"),
        "output must start with DOCTYPE"
    );
    assert!(html.contains("<pre>"), "output must contain <pre>");
    assert!(
        html.contains("</html>"),
        "output must contain closing </html>"
    );
}
