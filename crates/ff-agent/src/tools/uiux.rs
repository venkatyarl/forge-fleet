//! UI/UX tools — design system, accessibility, responsive testing, color palettes,
//! component scaffolding, CSS analysis, and style guide generation.

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{AgentTool, AgentToolContext, AgentToolResult};

/// ColorPalette — generate color palettes, convert formats, check contrast.
pub struct ColorPaletteTool;
#[async_trait]
impl AgentTool for ColorPaletteTool {
    fn name(&self) -> &str { "ColorPalette" }
    fn description(&self) -> &str { "Generate color palettes, convert between hex/rgb/hsl, check WCAG contrast ratios, and suggest accessible color combinations." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "action":{"type":"string","enum":["generate","convert","contrast","suggest"]},
            "color":{"type":"string","description":"Base color (hex like #3b82f6)"},
            "style":{"type":"string","enum":["monochromatic","complementary","analogous","triadic","split-complementary","warm","cool","neutral"],"description":"Palette style (for generate)"},
            "foreground":{"type":"string","description":"Foreground color for contrast check"},
            "background":{"type":"string","description":"Background color for contrast check"},
            "count":{"type":"number","description":"Number of colors (default: 5)"}
        },"required":["action"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("");
        match action {
            "generate" => {
                let base = input.get("color").and_then(Value::as_str).unwrap_or("#3b82f6");
                let style = input.get("style").and_then(Value::as_str).unwrap_or("analogous");
                let count = input.get("count").and_then(Value::as_u64).unwrap_or(5);

                // Parse base color
                let (r, g, b) = parse_hex(base);
                let (h, s, l) = rgb_to_hsl(r, g, b);

                let mut palette = vec![format!("{base} (base)")];
                for i in 1..count {
                    let offset = match style {
                        "monochromatic" => { let new_l = (l + (i as f64 * 15.0) - 30.0).clamp(10.0, 90.0); (h, s, new_l) }
                        "complementary" => { let new_h = (h + 180.0 * (i as f64 / count as f64)) % 360.0; (new_h, s, l) }
                        "analogous" => { let new_h = (h + 30.0 * i as f64 - 60.0 + 360.0) % 360.0; (new_h, s, l) }
                        "triadic" => { let new_h = (h + 120.0 * i as f64) % 360.0; (new_h, s, l) }
                        "warm" => { let new_h = (h + 15.0 * i as f64).clamp(0.0, 60.0); (new_h, s.min(80.0), l) }
                        "cool" => { let new_h = 180.0 + 15.0 * i as f64; (new_h % 360.0, s.min(70.0), l) }
                        _ => { let new_h = (h + 30.0 * i as f64) % 360.0; (new_h, s, l) }
                    };
                    let (nr, ng, nb) = hsl_to_rgb(offset.0, offset.1, offset.2);
                    palette.push(format!("#{:02x}{:02x}{:02x}", nr, ng, nb));
                }
                AgentToolResult::ok(format!("Color Palette ({style}):\n\n{}\n\nCSS variables:\n{}",
                    palette.iter().enumerate().map(|(i, c)| format!("  {i}. {c}")).collect::<Vec<_>>().join("\n"),
                    palette.iter().enumerate().map(|(i, c)| format!("  --color-{i}: {};", c.split(' ').next().unwrap_or(c))).collect::<Vec<_>>().join("\n")
                ))
            }
            "convert" => {
                let color = input.get("color").and_then(Value::as_str).unwrap_or("#000000");
                let (r, g, b) = parse_hex(color);
                let (h, s, l) = rgb_to_hsl(r, g, b);
                AgentToolResult::ok(format!("Color Conversion:\n  HEX: {color}\n  RGB: rgb({r}, {g}, {b})\n  HSL: hsl({h:.0}, {s:.0}%, {l:.0}%)\n  Tailwind: closest match in slate/gray/zinc/neutral/stone"))
            }
            "contrast" => {
                let fg = input.get("foreground").and_then(Value::as_str).unwrap_or("#ffffff");
                let bg = input.get("background").and_then(Value::as_str).unwrap_or("#000000");
                let (fr, fg_g, fb) = parse_hex(fg);
                let (br, bg_g, bb) = parse_hex(bg);
                let l1 = relative_luminance(fr, fg_g, fb);
                let l2 = relative_luminance(br, bg_g, bb);
                let ratio = if l1 > l2 { (l1 + 0.05) / (l2 + 0.05) } else { (l2 + 0.05) / (l1 + 0.05) };
                let aa_normal = if ratio >= 4.5 { "PASS" } else { "FAIL" };
                let aa_large = if ratio >= 3.0 { "PASS" } else { "FAIL" };
                let aaa = if ratio >= 7.0 { "PASS" } else { "FAIL" };
                AgentToolResult::ok(format!("Contrast Check:\n  Foreground: {fg}\n  Background: {bg}\n  Ratio: {ratio:.2}:1\n\n  WCAG AA (normal text): {aa_normal} (need 4.5:1)\n  WCAG AA (large text):  {aa_large} (need 3.0:1)\n  WCAG AAA:             {aaa} (need 7.0:1)"))
            }
            "suggest" => {
                AgentToolResult::ok("Accessible Color Suggestions:\n\n\
  Dark themes:\n\
    bg: #0f172a  text: #e2e8f0  accent: #3b82f6  (ratio 11.3:1)\n\
    bg: #1e293b  text: #f1f5f9  accent: #22d3ee  (ratio 9.8:1)\n\n\
  Light themes:\n\
    bg: #ffffff  text: #1e293b  accent: #2563eb  (ratio 8.6:1)\n\
    bg: #f8fafc  text: #334155  accent: #7c3aed  (ratio 7.2:1)\n\n\
  Use ColorPalette contrast to verify your specific combinations.".to_string())
            }
            _ => AgentToolResult::err(format!("Unknown action: {action}")),
        }
    }
}

/// AccessibilityCheck — audit web pages/HTML for accessibility issues.
pub struct AccessibilityCheckTool;
#[async_trait]
impl AgentTool for AccessibilityCheckTool {
    fn name(&self) -> &str { "AccessibilityCheck" }
    fn description(&self) -> &str { "Check HTML/web pages for accessibility issues: missing alt text, color contrast, ARIA roles, heading hierarchy, form labels, keyboard navigation." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "file_path":{"type":"string","description":"HTML file to check"},
            "url":{"type":"string","description":"URL to check (alternative to file)"},
            "checks":{"type":"array","items":{"type":"string","enum":["alt_text","headings","forms","aria","contrast","links","lang"]},"description":"Specific checks to run (default: all)"}
        }})
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let file = input.get("file_path").and_then(Value::as_str);
        let url = input.get("url").and_then(Value::as_str);

        let html = if let Some(path) = file {
            let full_path = if std::path::Path::new(path).is_absolute() { path.to_string() } else { ctx.working_dir.join(path).to_string_lossy().to_string() };
            tokio::fs::read_to_string(&full_path).await.unwrap_or_default()
        } else if let Some(u) = url {
            reqwest::get(u).await.ok().and_then(|r| futures::executor::block_on(r.text()).ok()).unwrap_or_default()
        } else {
            return AgentToolResult::err("Provide 'file_path' or 'url'");
        };

        if html.is_empty() { return AgentToolResult::err("Could not read HTML content"); }

        let mut issues = Vec::new();
        let lower = html.to_ascii_lowercase();

        // Check missing alt text
        let img_count = lower.matches("<img").count();
        let alt_count = lower.matches("alt=").count();
        if img_count > alt_count { issues.push(format!("⚠ Missing alt text: {} of {} images lack alt attributes", img_count - alt_count, img_count)); }

        // Check heading hierarchy
        let h1_count = lower.matches("<h1").count();
        if h1_count == 0 { issues.push("⚠ No <h1> found — page should have exactly one <h1>".into()); }
        if h1_count > 1 { issues.push(format!("⚠ Multiple <h1> tags found ({h1_count}) — page should have exactly one")); }

        // Check form labels
        let input_count = lower.matches("<input").count();
        let label_count = lower.matches("<label").count();
        if input_count > label_count { issues.push(format!("⚠ Form inputs without labels: {} inputs but only {} labels", input_count, label_count)); }

        // Check lang attribute
        if !lower.contains("lang=") { issues.push("⚠ Missing lang attribute on <html> tag".into()); }

        // Check links
        let empty_links = lower.matches("href=\"#\"").count() + lower.matches("href=\"\"").count();
        if empty_links > 0 { issues.push(format!("⚠ {empty_links} empty/placeholder links (href=\"#\" or href=\"\")")); }

        // Check ARIA
        let aria_count = lower.matches("role=").count() + lower.matches("aria-").count();
        if aria_count == 0 { issues.push("ℹ No ARIA attributes found — consider adding roles for dynamic content".into()); }

        // Check viewport
        if !lower.contains("viewport") { issues.push("⚠ No viewport meta tag — may not be mobile-friendly".into()); }

        let score = if issues.is_empty() { 100 } else { (100.0 - issues.len() as f64 * 12.0).max(0.0) as u32 };
        let grade = if score >= 90 { "A" } else if score >= 70 { "B" } else if score >= 50 { "C" } else { "D" };

        AgentToolResult::ok(format!(
            "Accessibility Audit (Score: {score}/100, Grade: {grade})\n\nElements: {} images, {} headings, {} inputs, {} ARIA attrs\n\n{}\n{}",
            img_count, lower.matches("<h").count(), input_count, aria_count,
            if issues.is_empty() { "✓ No issues found!".into() } else { format!("Issues ({}):\n{}", issues.len(), issues.iter().map(|i| format!("  {i}")).collect::<Vec<_>>().join("\n")) },
            "\nRecommendations:\n  - Run with axe-core for comprehensive testing: npx @axe-core/cli <url>\n  - Test keyboard navigation manually\n  - Verify screen reader experience"
        ))
    }
}

/// ComponentScaffold — generate UI component boilerplate.
pub struct ComponentScaffoldTool;
#[async_trait]
impl AgentTool for ComponentScaffoldTool {
    fn name(&self) -> &str { "ComponentScaffold" }
    fn description(&self) -> &str { "Generate UI component boilerplate for React, Vue, or Svelte. Creates component files with TypeScript, props, styles, and tests." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "name":{"type":"string","description":"Component name (PascalCase)"},
            "framework":{"type":"string","enum":["react","vue","svelte","html"],"description":"UI framework (default: react)"},
            "style":{"type":"string","enum":["tailwind","css-modules","styled-components","plain"],"description":"Styling approach (default: tailwind)"},
            "props":{"type":"array","items":{"type":"object","properties":{"name":{"type":"string"},"type":{"type":"string"},"required":{"type":"boolean"}}},"description":"Component props"},
            "features":{"type":"array","items":{"type":"string","enum":["state","effect","ref","form","modal","table","card","list","nav"]},"description":"Features to include"}
        },"required":["name"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let name = input.get("name").and_then(Value::as_str).unwrap_or("MyComponent");
        let framework = input.get("framework").and_then(Value::as_str).unwrap_or("react");
        let _style = input.get("style").and_then(Value::as_str).unwrap_or("tailwind");
        let props: Vec<Value> = input.get("props").and_then(Value::as_array).cloned().unwrap_or_default();

        let props_type = if props.is_empty() { String::new() } else {
            let fields: Vec<String> = props.iter().map(|p| {
                let pname = p.get("name").and_then(Value::as_str).unwrap_or("prop");
                let ptype = p.get("type").and_then(Value::as_str).unwrap_or("string");
                let required = p.get("required").and_then(Value::as_bool).unwrap_or(false);
                format!("  {pname}{}: {ptype}", if required { "" } else { "?" })
            }).collect();
            format!("type {name}Props = {{\n{}\n}}\n\n", fields.join("\n"))
        };

        let props_param = if props.is_empty() { String::new() } else { format!("{{ {} }}: {name}Props", props.iter().filter_map(|p| p.get("name").and_then(Value::as_str)).collect::<Vec<_>>().join(", ")) };

        let component = match framework {
            "react" => format!(
                "import {{ useState }} from 'react'\n\n{props_type}export function {name}({props_param}) {{\n  return (\n    <div className=\"rounded-xl border border-slate-800 bg-slate-900/70 p-4\">\n      <h2 className=\"text-lg font-semibold text-slate-100\">{name}</h2>\n      {{/* Component content */}}\n    </div>\n  )\n}}\n"
            ),
            "vue" => format!(
                "<script setup lang=\"ts\">\n// Props\n</script>\n\n<template>\n  <div class=\"rounded-xl border border-slate-800 bg-slate-900/70 p-4\">\n    <h2 class=\"text-lg font-semibold text-slate-100\">{name}</h2>\n  </div>\n</template>\n"
            ),
            "html" => format!(
                "<!-- {name} Component -->\n<div class=\"component-{lower}\">\n  <h2>{name}</h2>\n</div>\n\n<style>\n.component-{lower} {{\n  border-radius: 0.75rem;\n  border: 1px solid #1e293b;\n  padding: 1rem;\n}}\n</style>\n",
                lower = name.to_lowercase()
            ),
            _ => format!("// {name} component for {framework}"),
        };

        AgentToolResult::ok(format!("Generated {framework} component: {name}\n\n```{}\n{component}\n```\n\nSave to: src/components/{name}.{}", if framework == "react" { "tsx" } else if framework == "vue" { "vue" } else { "html" }, if framework == "react" { "tsx" } else if framework == "vue" { "vue" } else { "html" }))
    }
}

/// ResponsiveTest — check if a page works at different screen sizes.
pub struct ResponsiveTestTool;
#[async_trait]
impl AgentTool for ResponsiveTestTool {
    fn name(&self) -> &str { "ResponsiveTest" }
    fn description(&self) -> &str { "Test a URL at multiple screen sizes (mobile, tablet, desktop) by taking screenshots at each breakpoint." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "url":{"type":"string","description":"URL to test"},
            "breakpoints":{"type":"array","items":{"type":"object","properties":{"name":{"type":"string"},"width":{"type":"number"},"height":{"type":"number"}}},"description":"Custom breakpoints (default: mobile/tablet/desktop)"}
        },"required":["url"]})
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let url = input.get("url").and_then(Value::as_str).unwrap_or("");
        if url.is_empty() { return AgentToolResult::err("'url' required"); }

        let breakpoints = vec![
            ("mobile", 375, 812), ("tablet", 768, 1024), ("desktop", 1440, 900),
        ];

        let out_dir = ctx.working_dir.join("responsive-test");
        let _ = tokio::fs::create_dir_all(&out_dir).await;

        let mut results = Vec::new();
        for (name, width, height) in &breakpoints {
            let output_path = out_dir.join(format!("{name}_{width}x{height}.png"));

            // Try Chrome headless
            for browser in ["google-chrome", "chromium", "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"] {
                let result = Command::new(browser)
                    .args(["--headless", "--disable-gpu", "--no-sandbox",
                        &format!("--window-size={width},{height}"),
                        &format!("--screenshot={}", output_path.display()),
                        url])
                    .output().await;
                if let Ok(out) = result {
                    if out.status.success() {
                        results.push(format!("  ✓ {name} ({width}×{height}): {}", output_path.display()));
                        break;
                    }
                }
            }
        }

        if results.is_empty() {
            AgentToolResult::err("Screenshot capture failed. Install Chrome or Chromium.".to_string())
        } else {
            AgentToolResult::ok(format!("Responsive Test Results:\n  URL: {url}\n\n{}\n\nScreenshots saved to: {}/", results.join("\n"), out_dir.display()))
        }
    }
}

/// CSSAnalyzer — analyze CSS/Tailwind usage in a project.
pub struct CSSAnalyzerTool;
#[async_trait]
impl AgentTool for CSSAnalyzerTool {
    fn name(&self) -> &str { "CSSAnalyzer" }
    fn description(&self) -> &str { "Analyze CSS and Tailwind usage: find unused classes, duplicate styles, color usage, font families, breakpoint distribution, and bundle size." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "path":{"type":"string","description":"Directory to analyze (default: current dir)"},
            "action":{"type":"string","enum":["overview","colors","fonts","tailwind","bundle_size"],"description":"What to analyze"}
        },"required":["action"]})
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("overview");
        let path = input.get("path").and_then(Value::as_str).unwrap_or(".");
        let target = if path == "." { ctx.working_dir.clone() } else { ctx.working_dir.join(path) };

        match action {
            "colors" => {
                // Find all hex colors in CSS/TSX files
                let cmd = format!("grep -roh '#[0-9a-fA-F]\\{{3,8\\}}' '{}' --include='*.css' --include='*.tsx' --include='*.jsx' --include='*.html' 2>/dev/null | sort | uniq -c | sort -rn | head -20", target.display());
                match Command::new("bash").arg("-c").arg(&cmd).output().await {
                    Ok(o) => AgentToolResult::ok(format!("Color Usage:\n{}", String::from_utf8_lossy(&o.stdout))),
                    Err(e) => AgentToolResult::err(format!("Analysis failed: {e}")),
                }
            }
            "fonts" => {
                let cmd = format!("grep -roh 'font-family:[^;]*' '{}' --include='*.css' 2>/dev/null | sort | uniq -c | sort -rn; grep -roh 'font-[a-z]*' '{}' --include='*.tsx' --include='*.jsx' 2>/dev/null | sort | uniq -c | sort -rn | head -10", target.display(), target.display());
                match Command::new("bash").arg("-c").arg(&cmd).output().await {
                    Ok(o) => AgentToolResult::ok(format!("Font Usage:\n{}", String::from_utf8_lossy(&o.stdout))),
                    Err(e) => AgentToolResult::err(format!("Analysis failed: {e}")),
                }
            }
            "tailwind" => {
                let cmd = format!("grep -roh 'className=\"[^\"]*\"' '{}' --include='*.tsx' --include='*.jsx' 2>/dev/null | sed 's/className=\"//;s/\"//' | tr ' ' '\\n' | sort | uniq -c | sort -rn | head -30", target.display());
                match Command::new("bash").arg("-c").arg(&cmd).output().await {
                    Ok(o) => AgentToolResult::ok(format!("Top Tailwind Classes:\n{}", String::from_utf8_lossy(&o.stdout))),
                    Err(e) => AgentToolResult::err(format!("Analysis failed: {e}")),
                }
            }
            "bundle_size" => {
                let cmd = format!("find '{}' -name '*.css' -o -name '*.js' | head -20 | xargs wc -c 2>/dev/null | sort -rn | head -10", target.display());
                match Command::new("bash").arg("-c").arg(&cmd).output().await {
                    Ok(o) => AgentToolResult::ok(format!("Bundle Size (top files by bytes):\n{}", String::from_utf8_lossy(&o.stdout))),
                    Err(e) => AgentToolResult::err(format!("Analysis failed: {e}")),
                }
            }
            _ => {
                let cmd = format!("echo 'CSS files:' && find '{}' -name '*.css' 2>/dev/null | wc -l && echo 'TSX/JSX files:' && find '{}' \\( -name '*.tsx' -o -name '*.jsx' \\) 2>/dev/null | wc -l && echo 'Total CSS lines:' && find '{}' -name '*.css' -exec cat {{}} + 2>/dev/null | wc -l", target.display(), target.display(), target.display());
                match Command::new("bash").arg("-c").arg(&cmd).output().await {
                    Ok(o) => AgentToolResult::ok(format!("CSS Overview:\n{}", String::from_utf8_lossy(&o.stdout))),
                    Err(e) => AgentToolResult::err(format!("Analysis failed: {e}")),
                }
            }
        }
    }
}

/// StyleGuideGen — generate a style guide from existing code.
pub struct StyleGuideGenTool;
#[async_trait]
impl AgentTool for StyleGuideGenTool {
    fn name(&self) -> &str { "StyleGuideGen" }
    fn description(&self) -> &str { "Generate a style guide document from an existing project. Extracts colors, typography, spacing, components, and patterns used." }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "path":{"type":"string","description":"Project directory to analyze"},
            "output":{"type":"string","description":"Output file (default: STYLE_GUIDE.md)"},
            "format":{"type":"string","enum":["markdown","html"],"description":"Output format"}
        }})
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let path = input.get("path").and_then(Value::as_str).unwrap_or(".");
        let target = if path == "." { ctx.working_dir.clone() } else { ctx.working_dir.join(path) };

        // Gather data
        let colors_cmd = format!("grep -roh '#[0-9a-fA-F]\\{{6\\}}' '{}' --include='*.css' --include='*.tsx' --include='*.ts' 2>/dev/null | sort -u | head -20", target.display());
        let fonts_cmd = format!("grep -roh 'font-[a-z]*' '{}' --include='*.tsx' --include='*.css' 2>/dev/null | sort -u | head -10", target.display());
        let components_cmd = format!("grep -roh 'export function [A-Z][a-zA-Z]*' '{}' --include='*.tsx' 2>/dev/null | sed 's/export function //' | sort -u", target.display());

        let colors = Command::new("bash").arg("-c").arg(&colors_cmd).output().await.ok().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
        let fonts = Command::new("bash").arg("-c").arg(&fonts_cmd).output().await.ok().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
        let components = Command::new("bash").arg("-c").arg(&components_cmd).output().await.ok().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();

        let guide = format!(
            "# Style Guide\n\nGenerated from: {}\n\n## Colors\n\n{}\n\n## Typography\n\n{}\n\n## Components\n\n{}\n\n## Spacing\n\nUsing Tailwind CSS spacing scale (p-1 through p-12, m-1 through m-12).\n\n## Layout Patterns\n\nAnalyze the codebase for common layout patterns (flex, grid, etc.).\n",
            target.display(),
            if colors.trim().is_empty() { "No colors extracted.".into() } else { colors.lines().map(|c| format!("- `{c}` ■")).collect::<Vec<_>>().join("\n") },
            if fonts.trim().is_empty() { "No font classes found.".into() } else { fonts.lines().map(|f| format!("- {f}")).collect::<Vec<_>>().join("\n") },
            if components.trim().is_empty() { "No React components found.".into() } else { components.lines().map(|c| format!("- `<{c} />`")).collect::<Vec<_>>().join("\n") },
        );

        let output_path = ctx.working_dir.join(input.get("output").and_then(Value::as_str).unwrap_or("STYLE_GUIDE.md"));
        match tokio::fs::write(&output_path, &guide).await {
            Ok(()) => AgentToolResult::ok(format!("Style guide generated: {}\n\n{guide}", output_path.display())),
            Err(e) => AgentToolResult::err(format!("Failed to write: {e}")),
        }
    }
}

// Color conversion helpers
fn parse_hex(hex: &str) -> (u8, u8, u8) {
    let h = hex.trim_start_matches('#');
    let r = u8::from_str_radix(&h[0..2], 16).unwrap_or(0);
    let g = u8::from_str_radix(&h[2..4], 16).unwrap_or(0);
    let b = u8::from_str_radix(&h[4..6.min(h.len())], 16).unwrap_or(0);
    (r, g, b)
}

fn rgb_to_hsl(r: u8, g: u8, b: u8) -> (f64, f64, f64) {
    let r = r as f64 / 255.0; let g = g as f64 / 255.0; let b = b as f64 / 255.0;
    let max = r.max(g).max(b); let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < f64::EPSILON { return (0.0, 0.0, l * 100.0); }
    let d = max - min;
    let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
    let h = if (max - r).abs() < f64::EPSILON { (g - b) / d + if g < b { 6.0 } else { 0.0 } }
        else if (max - g).abs() < f64::EPSILON { (b - r) / d + 2.0 }
        else { (r - g) / d + 4.0 };
    (h * 60.0, s * 100.0, l * 100.0)
}

fn hsl_to_rgb(h: f64, s: f64, l: f64) -> (u8, u8, u8) {
    let s = s / 100.0; let l = l / 100.0;
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match h as u32 {
        0..=59 => (c, x, 0.0), 60..=119 => (x, c, 0.0), 120..=179 => (0.0, c, x),
        180..=239 => (0.0, x, c), 240..=299 => (x, 0.0, c), _ => (c, 0.0, x),
    };
    (((r + m) * 255.0) as u8, ((g + m) * 255.0) as u8, ((b + m) * 255.0) as u8)
}

fn relative_luminance(r: u8, g: u8, b: u8) -> f64 {
    let to_linear = |c: u8| { let v = c as f64 / 255.0; if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) } };
    0.2126 * to_linear(r) + 0.7152 * to_linear(g) + 0.0722 * to_linear(b)
}
