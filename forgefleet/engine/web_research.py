"""Web Research — agents that can search docs and read web pages mid-task.

Item #7: Agent stuck on how to use a crate? Search docs.rs.
Uses DuckDuckGo (no API key) + simple HTML extraction.
"""
import json
import os
import re
import urllib.request
import urllib.error
from dataclasses import dataclass, field
from html.parser import HTMLParser


class TextExtractor(HTMLParser):
    """Simple HTML → text extractor."""
    def __init__(self):
        super().__init__()
        self.text = []
        self._skip = False
        self._skip_tags = {"script", "style", "nav", "footer", "header"}
    
    def handle_starttag(self, tag, attrs):
        if tag in self._skip_tags:
            self._skip = True
    
    def handle_endtag(self, tag):
        if tag in self._skip_tags:
            self._skip = False
    
    def handle_data(self, data):
        if not self._skip:
            text = data.strip()
            if text:
                self.text.append(text)
    
    def get_text(self) -> str:
        return "\n".join(self.text)


@dataclass
class SearchResult:
    """A web search result."""
    title: str
    url: str
    snippet: str


class WebResearcher:
    """Web search + page reading for agents.
    
    Provides two tools for the agent loop:
    - web_search(query) → list of results
    - web_read(url) → extracted text content
    """
    
    def search(self, query: str, num_results: int = 5) -> list[SearchResult]:
        """Search the web using DuckDuckGo HTML (no API key needed)."""
        try:
            encoded = urllib.request.quote(query)
            url = f"https://html.duckduckgo.com/html/?q={encoded}"
            
            req = urllib.request.Request(url, headers={
                "User-Agent": "Mozilla/5.0 (compatible; ForgeFleet/1.0)"
            })
            
            with urllib.request.urlopen(req, timeout=10) as resp:
                html = resp.read().decode("utf-8", errors="ignore")
            
            results = []
            # Parse DuckDuckGo HTML results
            for match in re.finditer(
                r'<a rel="nofollow" class="result__a" href="([^"]+)"[^>]*>(.*?)</a>.*?'
                r'<a class="result__snippet"[^>]*>(.*?)</a>',
                html, re.DOTALL,
            ):
                url_raw = match.group(1)
                # DDG wraps URLs
                actual_url = re.search(r'uddg=([^&]+)', url_raw)
                if actual_url:
                    url_clean = urllib.request.unquote(actual_url.group(1))
                else:
                    url_clean = url_raw
                
                title = re.sub(r'<[^>]+>', '', match.group(2)).strip()
                snippet = re.sub(r'<[^>]+>', '', match.group(3)).strip()
                
                if title and url_clean:
                    results.append(SearchResult(title=title, url=url_clean, snippet=snippet))
                
                if len(results) >= num_results:
                    break
            
            return results
        except Exception as e:
            return [SearchResult(title="Search error", url="", snippet=str(e))]
    
    def read_page(self, url: str, max_chars: int = 8000) -> str:
        """Read and extract text from a web page."""
        try:
            req = urllib.request.Request(url, headers={
                "User-Agent": "Mozilla/5.0 (compatible; ForgeFleet/1.0)"
            })
            
            with urllib.request.urlopen(req, timeout=15) as resp:
                content_type = resp.headers.get("Content-Type", "")
                if "text/html" not in content_type and "text/plain" not in content_type:
                    return f"Non-text content: {content_type}"
                
                html = resp.read().decode("utf-8", errors="ignore")
            
            # Extract text
            extractor = TextExtractor()
            extractor.feed(html)
            text = extractor.get_text()
            
            if len(text) > max_chars:
                text = text[:max_chars] + f"\n\n... [truncated at {max_chars} chars]"
            
            return text
        except Exception as e:
            return f"Error reading {url}: {e}"
    
    def search_docs(self, crate_or_package: str, query: str = "") -> str:
        """Search documentation for a specific crate/package.
        
        Tries:
        - docs.rs for Rust crates
        - npmjs.com for Node packages
        - PyPI for Python packages
        """
        search_query = f"{crate_or_package} {query}".strip()
        
        # Try docs.rs first for Rust
        docs_url = f"https://docs.rs/{crate_or_package}/latest"
        try:
            content = self.read_page(docs_url, max_chars=4000)
            if "not found" not in content.lower()[:100]:
                return f"## {crate_or_package} (docs.rs)\n\n{content}"
        except Exception:
            pass
        
        # Fallback to web search
        results = self.search(f"{search_query} documentation", 3)
        if results:
            return f"## Search results for {search_query}:\n\n" + "\n".join(
                f"- [{r.title}]({r.url})\n  {r.snippet}" for r in results
            )
        
        return f"No documentation found for {crate_or_package}"
    
    def as_tools(self):
        """Return Tool objects for use in agent loop."""
        from .tool import Tool
        
        return [
            Tool(
                name="web_search",
                description="Search the web. Use for finding documentation, examples, or solutions.",
                parameters={
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Search query"}
                    },
                    "required": ["query"],
                },
                func=lambda query="": "\n".join(
                    f"- {r.title}: {r.snippet}" for r in self.search(query)
                ),
            ),
            Tool(
                name="read_webpage",
                description="Read and extract text from a URL. Use for reading documentation pages.",
                parameters={
                    "type": "object",
                    "properties": {
                        "url": {"type": "string", "description": "URL to read"}
                    },
                    "required": ["url"],
                },
                func=lambda url="": self.read_page(url),
            ),
            Tool(
                name="search_docs",
                description="Search documentation for a Rust crate, npm package, or Python library.",
                parameters={
                    "type": "object",
                    "properties": {
                        "package": {"type": "string", "description": "Package/crate name"},
                        "query": {"type": "string", "description": "What to search for"},
                    },
                    "required": ["package"],
                },
                func=lambda package="", query="": self.search_docs(package, query),
            ),
        ]
