use axum::{
    extract::Query,
    http::{HeaderMap, HeaderValue, header},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedConfig {
    pub gateway_ws_url: String,
    pub widget_title: String,
    pub accent_color: String,
}

impl Default for EmbedConfig {
    fn default() -> Self {
        Self {
            gateway_ws_url: "ws://localhost:8787/ws".to_string(),
            widget_title: "ForgeFleet Chat".to_string(),
            accent_color: "#6366f1".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct EmbedQuery {
    pub ws: Option<String>,
    pub title: Option<String>,
    pub color: Option<String>,
}

pub fn build_widget_script(config: &EmbedConfig) -> String {
    format!(
        r#"(function () {{
  const script = document.currentScript;
  const wsUrl = script?.dataset?.ws || "{ws_url}";
  const title = script?.dataset?.title || "{title}";
  const accent = script?.dataset?.color || "{accent}";

  const root = document.createElement('div');
  root.id = 'ff-widget-root';
  root.innerHTML = `
    <style>
      #ff-widget-root * {{ box-sizing: border-box; font-family: Inter, system-ui, sans-serif; }}
      #ff-widget-toggle {{ position: fixed; right: 20px; bottom: 20px; background: {accent}; color: #fff; border: 0; border-radius: 999px; width: 56px; height: 56px; cursor: pointer; box-shadow: 0 10px 24px rgba(0,0,0,.25); z-index: 999998; }}
      #ff-widget-panel {{ position: fixed; right: 20px; bottom: 84px; width: 340px; max-width: calc(100vw - 24px); height: 460px; background: #0b1020; color: #e2e8f0; border: 1px solid #1f2937; border-radius: 14px; display: none; flex-direction: column; overflow: hidden; z-index: 999999; box-shadow: 0 18px 48px rgba(0,0,0,.35); }}
      #ff-widget-header {{ padding: 12px 14px; font-weight: 600; background: #111827; border-bottom: 1px solid #1f2937; }}
      #ff-widget-messages {{ flex: 1; overflow-y: auto; padding: 12px; display: flex; flex-direction: column; gap: 8px; }}
      .ff-bubble {{ max-width: 86%; padding: 8px 10px; border-radius: 10px; font-size: 14px; line-height: 1.35; }}
      .ff-me {{ align-self: flex-end; background: {accent}; color: #fff; }}
      .ff-bot {{ align-self: flex-start; background: #1f2937; color: #e5e7eb; }}
      #ff-widget-form {{ border-top: 1px solid #1f2937; display: flex; padding: 10px; gap: 8px; }}
      #ff-widget-input {{ flex: 1; border: 1px solid #374151; background: #0f172a; color: #e2e8f0; border-radius: 8px; padding: 8px 10px; }}
      #ff-widget-send {{ background: {accent}; color: #fff; border: 0; border-radius: 8px; padding: 8px 12px; cursor: pointer; }}
    </style>
    <button id='ff-widget-toggle'>💬</button>
    <section id='ff-widget-panel'>
      <header id='ff-widget-header'></header>
      <div id='ff-widget-messages'></div>
      <form id='ff-widget-form'>
        <input id='ff-widget-input' placeholder='Ask ForgeFleet…' autocomplete='off' />
        <button id='ff-widget-send' type='submit'>Send</button>
      </form>
    </section>
  `;

  document.body.appendChild(root);

  const toggle = root.querySelector('#ff-widget-toggle');
  const panel = root.querySelector('#ff-widget-panel');
  const header = root.querySelector('#ff-widget-header');
  const form = root.querySelector('#ff-widget-form');
  const input = root.querySelector('#ff-widget-input');
  const messages = root.querySelector('#ff-widget-messages');

  header.textContent = title;

  let socket;
  let opened = false;

  function bubble(text, cssClass) {{
    const node = document.createElement('div');
    node.className = 'ff-bubble ' + cssClass;
    node.textContent = text;
    messages.appendChild(node);
    messages.scrollTop = messages.scrollHeight;
  }}

  function connect() {{
    if (socket && (socket.readyState === WebSocket.OPEN || socket.readyState === WebSocket.CONNECTING)) return;

    socket = new WebSocket(wsUrl);
    socket.addEventListener('open', () => bubble('Connected to ForgeFleet.', 'ff-bot'));
    socket.addEventListener('message', (event) => {{
      try {{
        const payload = JSON.parse(event.data);
        bubble(payload.text || event.data, 'ff-bot');
      }} catch (_) {{
        bubble(event.data, 'ff-bot');
      }}
    }});
    socket.addEventListener('close', () => bubble('Connection closed.', 'ff-bot'));
  }}

  toggle.addEventListener('click', () => {{
    opened = !opened;
    panel.style.display = opened ? 'flex' : 'none';
    if (opened) connect();
  }});

  form.addEventListener('submit', (event) => {{
    event.preventDefault();
    const text = input.value.trim();
    if (!text) return;

    bubble(text, 'ff-me');
    input.value = '';

    if (socket?.readyState === WebSocket.OPEN) {{
      socket.send(JSON.stringify({{ text, source: 'embed_widget' }}));
    }} else {{
      bubble('Socket not connected yet.', 'ff-bot');
    }}
  }});
}})();
"#,
        ws_url = config.gateway_ws_url,
        title = config.widget_title,
        accent = config.accent_color,
    )
}

pub async fn widget_js_handler(Query(query): Query<EmbedQuery>) -> (HeaderMap, String) {
    let mut config = EmbedConfig::default();
    if let Some(ws) = query.ws {
        config.gateway_ws_url = ws;
    }
    if let Some(title) = query.title {
        config.widget_title = title;
    }
    if let Some(color) = query.color {
        config.accent_color = color;
    }

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/javascript; charset=utf-8"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=300"),
    );

    (headers, build_widget_script(&config))
}
