//! Offline source checking shared by the three fake harnesses. Never fetches
//! the network: non-file URLs produce unreachable.

pub fn wait_gate() {
    if let Some(gate) = std::env::var_os("FAKE_REVIEW_GATE") {
        let gate = std::path::PathBuf::from(gate);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        while !gate.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}

fn read_page(url: &str) -> Result<String, String> {
    let path = url.strip_prefix("file://").ok_or("not a file URL")?;
    // Decode bytes, including UTF-8 filenames; '+' stays literal in a URL path.
    let mut bytes = Vec::new();
    let mut input = path.as_bytes().iter().copied();
    while let Some(b) = input.next() {
        if b == b'%' {
            let hi = input.next().and_then(|b| (b as char).to_digit(16));
            let lo = input.next().and_then(|b| (b as char).to_digit(16));
            match (hi, lo) {
                (Some(hi), Some(lo)) => bytes.push((hi * 16 + lo) as u8),
                _ => return Err("invalid percent escape in file URL".into()),
            }
        } else {
            bytes.push(b);
        }
    }
    let path = String::from_utf8(bytes).map_err(|e| e.to_string())?;
    #[cfg(windows)]
    let path = if path.starts_with('/') && path.as_bytes().get(2) == Some(&b':') {
        path[1..].to_string()
    } else {
        path
    };
    std::fs::read_to_string(path).map_err(|e| e.to_string())
}

pub fn document(prompt: &str) -> serde_json::Value {
    let mut lines = prompt.lines();
    let sources: Vec<serde_json::Value> = lines
        .find(|l| *l == "## Sources to check")
        .and_then(|_| lines.next())
        .and_then(|s| serde_json::from_str(s).ok())
        .expect("sources in prompt");
    let sources: Vec<serde_json::Value> = sources.iter().map(|s| {
        let (verdict, reason) = match read_page(s["url"].as_str().unwrap()) {
            Ok(text) if text.contains(s["quote"].as_str().unwrap()) => ("supported", String::new()),
            Ok(_) => ("not_supported", "quote not found in page".to_string()),
            Err(e) => ("unreachable", e),
        };
        serde_json::json!({"id": s["id"], "url": s["url"], "verdict": verdict, "reason": reason})
    }).collect();
    let verdict = if sources.iter().any(|s| s["verdict"] == "not_supported") {
        "REJECT"
    } else if sources.iter().any(|s| s["verdict"] == "unreachable") {
        "UNVERIFIED"
    } else {
        "PASS"
    };
    let verdict =
        std::env::var("FAKE_SOURCE_CHECK_VERDICT").unwrap_or_else(|_| verdict.to_string());
    serde_json::json!({"verdict": verdict, "findings": {"sources": sources, "extra_findings": []}})
}
