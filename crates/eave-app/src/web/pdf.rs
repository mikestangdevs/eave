//! Internal PDF viewer: pdf.js on `eave://pdf/…`.
//!
//! Alloy does not ship Chrome's PDF Viewer extension
//! (`chrome-extension://mhjfb…`). Navigating to a `.pdf` therefore used
//! to paint a blank tab. This module hosts Mozilla's pdf.js on an eave
//! scheme, proxies the bytes so CORS is not the viewer's problem, and is
//! the rewrite `nav_policy` asks for.

use cef::*;
use cef::wrapper::stream_resource_handler::StreamResourceHandler;

const PDF_JS: &[u8] = include_bytes!("../../assets/pdfjs/pdf.min.mjs");
const PDF_WORKER: &[u8] = include_bytes!("../../assets/pdfjs/pdf.worker.min.mjs");

const VIEWER: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="color-scheme" content="dark">
<title>PDF</title>
<style>
  html, body { margin: 0; background: #1c1c1c; color: #eee; font: 13px/1.4 -apple-system, sans-serif; }
  #pages { padding: 16px 0 48px; }
  canvas { display: block; margin: 12px auto; box-shadow: 0 8px 24px #0008; background: #fff; }
  #err { max-width: 40rem; margin: 4rem auto; padding: 0 1.5rem; }
</style>
</head>
<body>
<div id="pages"></div>
<div id="err" hidden></div>
<script type="module">
import * as pdfjs from "/pdf.min.mjs";
pdfjs.GlobalWorkerOptions.workerSrc = "/pdf.worker.min.mjs";
const src = new URL(location.href).searchParams.get("u") || "";
const file = "/file?u=" + encodeURIComponent(src);
try {
  const pdf = await pdfjs.getDocument({ url: file }).promise;
  document.title = (src.split("/").pop() || "PDF") + " — Eave";
  const root = document.getElementById("pages");
  for (let n = 1; n <= pdf.numPages; n++) {
    const page = await pdf.getPage(n);
    const vp = page.getViewport({ scale: 1.25 });
    const canvas = document.createElement("canvas");
    const ctx = canvas.getContext("2d");
    canvas.width = vp.width;
    canvas.height = vp.height;
    root.appendChild(canvas);
    await page.render({ canvasContext: ctx, viewport: vp }).promise;
  }
} catch (e) {
  const err = document.getElementById("err");
  err.hidden = false;
  err.textContent = "This PDF could not be opened. " + (e && e.message ? e.message : e);
}
</script>
</body>
</html>
"#;

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = from_hex(bytes[i + 1]);
            let lo = from_hex(bytes[i + 2]);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn from_hex(b: u8) -> Option<u8> {
    Some(match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => return None,
    })
}

fn query_u(url: &str) -> Option<String> {
    let q = url.split_once('?')?.1.split('#').next()?;
    for part in q.split('&') {
        if let Some(v) = part.strip_prefix("u=") {
            let decoded = percent_decode(v);
            if decoded.is_empty() {
                return None;
            }
            return Some(decoded);
        }
    }
    None
}

fn path_of(url: &str) -> &str {
    let rest = url
        .strip_prefix("eave://pdf")
        .or_else(|| url.strip_prefix("EAVE://pdf"))
        .unwrap_or(url);
    rest.split('?').next().unwrap_or(rest)
}

fn fetch_source(src: &str) -> Option<Vec<u8>> {
    if src.starts_with("file://") {
        let path = src.trim_start_matches("file://");
        return std::fs::read(path).ok();
    }
    if !(src.starts_with("http://") || src.starts_with("https://")) {
        return None;
    }
    let out = std::process::Command::new("curl")
        .args(["-fsSL", "--max-time", "60", "-L", "--proto", "=https,http", src])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(out.stdout)
}

fn serve(url: &str) -> (String, Vec<u8>) {
    match path_of(url) {
        "/pdf.min.mjs" => ("text/javascript".into(), PDF_JS.to_vec()),
        "/pdf.worker.min.mjs" => ("text/javascript".into(), PDF_WORKER.to_vec()),
        "/file" => {
            let Some(src) = query_u(url) else {
                return ("text/plain".into(), b"missing u".to_vec());
            };
            match fetch_source(&src) {
                Some(bytes) => ("application/pdf".into(), bytes),
                None => ("text/plain".into(), b"could not fetch pdf".to_vec()),
            }
        }
        // MIME must be the bare type. `text/html; charset=utf-8` in
        // CefResponse::SetMimeType is not HTML to Chromium — the viewer
        // was served as a source listing (live walk 2026-08-29).
        _ => ("text/html".into(), VIEWER.as_bytes().to_vec()),
    }
}

wrap_scheme_handler_factory! {
    struct PdfSchemeFactory {}

    impl SchemeHandlerFactory {
        fn create(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _scheme_name: Option<&CefString>,
            request: Option<&mut Request>,
        ) -> Option<ResourceHandler> {
            let url = request
                .map(|r| CefString::from(&r.url()).to_string())
                .unwrap_or_default();
            let (mime, mut body) = serve(&url);
            let stream = stream_reader_create_for_data(body.as_mut_ptr(), body.len())?;
            // CEF copies the buffer into the stream; `body` can drop.
            Some(StreamResourceHandler::new_with_stream(mime, stream))
        }
    }
}

/// Register `eave:` and the factory. Call AFTER `CefInitialize`.
pub fn register() {
    let mut factory = PdfSchemeFactory::new();
    register_scheme_handler_factory(
        Some(&CefString::from("eave")),
        Some(&CefString::from("pdf")),
        Some(&mut factory),
    );
}

/// Options for `on_register_custom_schemes` — must match the factory.
pub fn scheme_options() -> i32 {
    (SchemeOptions::STANDARD.get_raw()
        | SchemeOptions::SECURE.get_raw()
        | SchemeOptions::CORS_ENABLED.get_raw()
        | SchemeOptions::FETCH_ENABLED.get_raw()
        | SchemeOptions::CSP_BYPASSING.get_raw()) as i32
}

// CefApp for subprocess CefExecuteProcess.
//
// Chromium's network service is a helper. Custom schemes are unknown to
// it unless OnRegisterCustomSchemes runs in that process. Passing None
// for the helper's CefApp produced eave://pdf navigations that died in
// network.mojom.NetworkContext (VALIDATION_ERROR_DESERIALIZATION_FAILED)
// after a correct rewrite — a blank tab. The browser process already
// registers via EaveCefApp; the helper must too.
wrap_app! {
    struct SchemeApp {}

    impl App {
        fn on_register_custom_schemes(&self, registrar: Option<&mut SchemeRegistrar>) {
            let Some(registrar) = registrar else { return };
            registrar.add_custom_scheme(Some(&CefString::from("eave")), scheme_options());
        }
    }
}

pub fn scheme_app() -> App {
    SchemeApp::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_viewer_html_is_a_page() {
        let (mime, body) = serve("eave://pdf/view?u=https%3A%2F%2Fx.test%2Fa.pdf");
        assert_eq!(mime, "text/html");
        let s = String::from_utf8_lossy(&body);
        assert!(s.contains("pdf.min.mjs"));
        assert!(s.contains("getDocument"));
    }

    #[test]
    fn the_script_assets_are_javascript() {
        let (mime, body) = serve("eave://pdf/pdf.min.mjs");
        assert_eq!(mime, "text/javascript");
        assert!(body.len() > 1000);
        let (w_mime, w_body) = serve("eave://pdf/pdf.worker.min.mjs");
        assert_eq!(w_mime, "text/javascript");
        assert!(w_body.len() > 1000);
    }

    #[test]
    fn query_u_round_trips() {
        assert_eq!(
            query_u("eave://pdf/file?u=https%3A%2F%2Fx.test%2Fa.pdf"),
            Some("https://x.test/a.pdf".into())
        );
    }
}
