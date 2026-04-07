//! Media tools — screenshots, images, videos, links, QR codes.

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{AgentTool, AgentToolContext, AgentToolResult, MAX_TOOL_RESULT_CHARS, truncate_output};

/// ScreenshotCapture — take screenshots of web pages.
pub struct ScreenshotCaptureTool;

#[async_trait]
impl AgentTool for ScreenshotCaptureTool {
    fn name(&self) -> &str { "Screenshot" }
    fn description(&self) -> &str { "Take a screenshot of a web page or local file. Saves as PNG. Uses headless Chrome/Chromium." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "url":{"type":"string","description":"URL or local file path to screenshot"},
            "output":{"type":"string","description":"Output file path (default: screenshot.png)"},
            "width":{"type":"number","description":"Viewport width (default: 1280)"},
            "height":{"type":"number","description":"Viewport height (default: 720)"},
            "full_page":{"type":"boolean","description":"Capture full page scroll (default: false)"}
        },"required":["url"]})
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let url = input.get("url").and_then(Value::as_str).unwrap_or("");
        let output = input.get("output").and_then(Value::as_str).unwrap_or("screenshot.png");
        let width = input.get("width").and_then(Value::as_u64).unwrap_or(1280);
        let height = input.get("height").and_then(Value::as_u64).unwrap_or(720);

        if url.is_empty() { return AgentToolResult::err("Missing 'url'"); }

        let output_path = if std::path::Path::new(output).is_absolute() {
            std::path::PathBuf::from(output)
        } else {
            ctx.working_dir.join(output)
        };

        // Try multiple screenshot tools
        // 1. Chrome/Chromium headless
        for browser in ["google-chrome", "chromium-browser", "chromium", "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"] {
            let result = Command::new(browser)
                .args(["--headless", "--disable-gpu", "--no-sandbox",
                    &format!("--window-size={width},{height}"),
                    &format!("--screenshot={}", output_path.display()),
                    url])
                .output().await;
            if let Ok(out) = result {
                if out.status.success() {
                    return AgentToolResult::ok(format!("Screenshot saved: {}\nSize: {}x{}\nURL: {url}", output_path.display(), width, height));
                }
            }
        }

        // 2. Try wkhtmltoimage
        let wk_result = Command::new("wkhtmltoimage")
            .args(["--width", &width.to_string(), "--height", &height.to_string(), url, &output_path.to_string_lossy()])
            .output().await;
        if let Ok(out) = wk_result {
            if out.status.success() {
                return AgentToolResult::ok(format!("Screenshot saved: {}", output_path.display()));
            }
        }

        // 3. Try playwright
        let pw_cmd = format!(
            "npx -y playwright screenshot --viewport-size='{width},{height}' '{url}' '{}'",
            output_path.display()
        );
        let pw_result = Command::new("bash").arg("-c").arg(&pw_cmd).output().await;
        if let Ok(out) = pw_result {
            if out.status.success() {
                return AgentToolResult::ok(format!("Screenshot saved: {}", output_path.display()));
            }
        }

        AgentToolResult::err("Screenshot failed. Install Chrome, wkhtmltoimage, or Playwright.".to_string())
    }
}

/// ImageAnalyze — get info about images (dimensions, format, EXIF, OCR).
pub struct ImageAnalyzeTool;

#[async_trait]
impl AgentTool for ImageAnalyzeTool {
    fn name(&self) -> &str { "ImageAnalyze" }
    fn description(&self) -> &str { "Analyze an image: dimensions, format, file size, color info, and OCR text extraction." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "file_path":{"type":"string","description":"Path to the image file"},
            "ocr":{"type":"boolean","description":"Extract text via OCR (default: false)"}
        },"required":["file_path"]})
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let file_path = input.get("file_path").and_then(Value::as_str).unwrap_or("");
        let do_ocr = input.get("ocr").and_then(Value::as_bool).unwrap_or(false);
        let path = if std::path::Path::new(file_path).is_absolute() { std::path::PathBuf::from(file_path) } else { ctx.working_dir.join(file_path) };

        if !path.exists() { return AgentToolResult::err(format!("File not found: {}", path.display())); }

        let mut info = Vec::new();

        // File info
        if let Ok(meta) = tokio::fs::metadata(&path).await {
            info.push(format!("File: {}", path.display()));
            info.push(format!("Size: {} bytes ({:.1} KB)", meta.len(), meta.len() as f64 / 1024.0));
        }

        // Try identify (ImageMagick) or sips (macOS)
        let identify = Command::new("identify").arg(&path).output().await;
        match identify {
            Ok(out) if out.status.success() => {
                info.push(format!("ImageMagick: {}", String::from_utf8_lossy(&out.stdout).trim()));
            }
            _ => {
                // macOS fallback
                let sips = Command::new("sips").args(["-g", "all"]).arg(&path).output().await;
                if let Ok(out) = sips {
                    if out.status.success() {
                        let output = String::from_utf8_lossy(&out.stdout);
                        for line in output.lines().take(10) {
                            info.push(format!("  {}", line.trim()));
                        }
                    }
                }
            }
        }

        // OCR
        if do_ocr {
            let ocr = Command::new("tesseract").arg(&path).arg("stdout").output().await;
            match ocr {
                Ok(out) if out.status.success() => {
                    let text = String::from_utf8_lossy(&out.stdout);
                    info.push(format!("\nOCR Text:\n{}", truncate_output(&text, 2000)));
                }
                _ => info.push("\nOCR: tesseract not installed (brew install tesseract)".into()),
            }
        }

        AgentToolResult::ok(info.join("\n"))
    }
}

/// VideoDownload — download videos from URLs.
pub struct VideoDownloadTool;

#[async_trait]
impl AgentTool for VideoDownloadTool {
    fn name(&self) -> &str { "VideoDownload" }
    fn description(&self) -> &str { "Download videos from URLs (YouTube, Vimeo, Twitter, etc.) using yt-dlp. Can extract audio only." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "url":{"type":"string","description":"Video URL"},
            "output":{"type":"string","description":"Output filename (default: auto)"},
            "audio_only":{"type":"boolean","description":"Extract audio only (default: false)"},
            "quality":{"type":"string","enum":["best","720p","480p","audio"],"description":"Quality (default: best)"},
            "info_only":{"type":"boolean","description":"Just show video info, don't download (default: false)"}
        },"required":["url"]})
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let url = input.get("url").and_then(Value::as_str).unwrap_or("");
        let audio_only = input.get("audio_only").and_then(Value::as_bool).unwrap_or(false);
        let info_only = input.get("info_only").and_then(Value::as_bool).unwrap_or(false);

        if url.is_empty() { return AgentToolResult::err("Missing 'url'"); }

        let mut args = vec!["--no-playlist"];
        if info_only { args.push("--dump-json"); }
        if audio_only { args.extend(["--extract-audio", "--audio-format", "mp3"]); }
        if let Some(output) = input.get("output").and_then(Value::as_str) { args.extend(["-o", output]); }
        args.push(url);

        // Try yt-dlp first, then youtube-dl
        for cmd in ["yt-dlp", "youtube-dl"] {
            let result = Command::new(cmd).args(&args).current_dir(&ctx.working_dir).output().await;
            if let Ok(out) = result {
                let output = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
                if out.status.success() {
                    return AgentToolResult::ok(truncate_output(&output, MAX_TOOL_RESULT_CHARS));
                }
            }
        }

        AgentToolResult::err("Video download failed. Install yt-dlp: brew install yt-dlp".to_string())
    }
}

/// LinkPreview — fetch metadata from URLs (OpenGraph, title, description).
pub struct LinkPreviewTool;

#[async_trait]
impl AgentTool for LinkPreviewTool {
    fn name(&self) -> &str { "LinkPreview" }
    fn description(&self) -> &str { "Fetch link metadata: title, description, image, OpenGraph tags, favicon. Works with any URL." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "url":{"type":"string","description":"URL to preview"},
            "urls":{"type":"array","items":{"type":"string"},"description":"Multiple URLs to preview"}
        }})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let mut urls: Vec<String> = Vec::new();
        if let Some(url) = input.get("url").and_then(Value::as_str) { urls.push(url.to_string()); }
        if let Some(arr) = input.get("urls").and_then(Value::as_array) {
            for u in arr { if let Some(s) = u.as_str() { urls.push(s.to_string()); } }
        }
        if urls.is_empty() { return AgentToolResult::err("Provide 'url' or 'urls'"); }

        let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(10))
            .user_agent("ForgeFleet-Agent/0.1").build().unwrap_or_default();

        let mut results = Vec::new();
        for url in &urls {
            match client.get(url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    let html = resp.text().await.unwrap_or_default();
                    let title = extract_meta(&html, "<title>", "</title>").unwrap_or_default();
                    let og_title = extract_meta_attr(&html, "og:title").unwrap_or_default();
                    let og_desc = extract_meta_attr(&html, "og:description").unwrap_or_default();
                    let og_image = extract_meta_attr(&html, "og:image").unwrap_or_default();
                    let description = extract_meta_attr(&html, "description").unwrap_or_default();

                    results.push(format!(
                        "URL: {url}\n  Title: {}\n  Description: {}\n  Image: {}\n",
                        if !og_title.is_empty() { &og_title } else { &title },
                        if !og_desc.is_empty() { &og_desc } else { &description },
                        og_image
                    ));
                }
                Ok(resp) => results.push(format!("URL: {url}\n  Error: HTTP {}\n", resp.status())),
                Err(e) => results.push(format!("URL: {url}\n  Error: {e}\n")),
            }
        }

        AgentToolResult::ok(results.join("\n"))
    }
}

/// ImageConvert — resize, convert, compress images.
pub struct ImageConvertTool;

#[async_trait]
impl AgentTool for ImageConvertTool {
    fn name(&self) -> &str { "ImageConvert" }
    fn description(&self) -> &str { "Convert, resize, or compress images. Supports PNG, JPG, WebP, GIF conversions." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "input":{"type":"string","description":"Input image path"},
            "output":{"type":"string","description":"Output path (format determined by extension)"},
            "resize":{"type":"string","description":"Resize dimensions (e.g. '800x600', '50%')"},
            "quality":{"type":"number","description":"JPEG/WebP quality 1-100 (default: 85)"}
        },"required":["input","output"]})
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let input_path = input.get("input").and_then(Value::as_str).unwrap_or("");
        let output_path = input.get("output").and_then(Value::as_str).unwrap_or("");
        let resize = input.get("resize").and_then(Value::as_str);
        let quality = input.get("quality").and_then(Value::as_u64).unwrap_or(85);

        let in_path = if std::path::Path::new(input_path).is_absolute() { input_path.to_string() } else { ctx.working_dir.join(input_path).to_string_lossy().to_string() };
        let out_path = if std::path::Path::new(output_path).is_absolute() { output_path.to_string() } else { ctx.working_dir.join(output_path).to_string_lossy().to_string() };

        // Try ImageMagick convert
        let mut args = vec![in_path.clone()];
        if let Some(r) = resize { args.extend(["-resize".to_string(), r.to_string()]); }
        args.extend(["-quality".to_string(), quality.to_string(), out_path.clone()]);

        let result = Command::new("convert").args(&args).output().await;
        match result {
            Ok(out) if out.status.success() => {
                AgentToolResult::ok(format!("Converted: {input_path} → {output_path}"))
            }
            _ => {
                // macOS fallback: sips
                let mut sips_args = vec!["-s".to_string(), "format".to_string()];
                let ext = std::path::Path::new(&out_path).extension().and_then(|e| e.to_str()).unwrap_or("png");
                sips_args.push(ext.to_string());
                if let Some(r) = resize {
                    if let Some((w, h)) = r.split_once('x') {
                        sips_args.extend(["-z".to_string(), h.to_string(), w.to_string()]);
                    }
                }
                sips_args.extend(["--out".to_string(), out_path.clone(), in_path]);
                match Command::new("sips").args(&sips_args).output().await {
                    Ok(out) if out.status.success() => AgentToolResult::ok(format!("Converted: {input_path} → {output_path}")),
                    _ => AgentToolResult::err("Image conversion failed. Install ImageMagick: brew install imagemagick".to_string()),
                }
            }
        }
    }
}

// HTML parsing helpers
fn extract_meta(html: &str, start_tag: &str, end_tag: &str) -> Option<String> {
    let start = html.find(start_tag)? + start_tag.len();
    let end = html[start..].find(end_tag)? + start;
    Some(html[start..end].trim().to_string())
}

fn extract_meta_attr(html: &str, property: &str) -> Option<String> {
    // Look for <meta property="og:title" content="..." /> or <meta name="description" content="..." />
    let patterns = [
        format!("property=\"{}\" content=\"", property),
        format!("property=\"{}\" content='", property),
        format!("name=\"{}\" content=\"", property),
        format!("name=\"{}\" content='", property),
    ];
    for pattern in &patterns {
        if let Some(start) = html.find(pattern.as_str()) {
            let content_start = start + pattern.len();
            let quote = if pattern.ends_with('"') { '"' } else { '\'' };
            if let Some(end) = html[content_start..].find(quote) {
                return Some(html[content_start..content_start + end].to_string());
            }
        }
    }
    None
}
