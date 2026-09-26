use anyhow::{Context, Result};
use axum::response::Html;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use std::process::Stdio;

pub(super) fn random_token(byte_count: usize) -> Result<String> {
    let mut bytes = vec![0; byte_count];
    getrandom::fill(&mut bytes).context("generate OAuth security token")?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

pub(super) fn sign_in_page(success: bool, title: &str, message: &str) -> Html<String> {
    let color = if success { "#16875d" } else { "#b42318" };
    Html(format!(
        "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{}</title><body style=\"margin:0;background:#101114;color:#f5f5f5;font:16px system-ui;display:grid;min-height:100vh;place-items:center\"><main style=\"max-width:32rem;padding:2rem\"><h1 style=\"color:{color}\">{}</h1><p>{}</p></main></body></html>",
        escape_html(title),
        escape_html(title),
        escape_html(message)
    ))
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

pub(super) fn open_browser(url: &str) -> bool {
    #[cfg(target_os = "windows")]
    {
        crate::proc::command("rundll32.exe")
            .args(["url.dll,FileProtocolHandler", url])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .is_ok()
    }
    #[cfg(target_os = "macos")]
    {
        crate::proc::command("open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .is_ok()
    }
    #[cfg(target_os = "linux")]
    {
        crate::proc::command("xdg-open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .is_ok()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = url;
        false
    }
}
