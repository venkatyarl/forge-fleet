"""Content Generator — blog posts, social media, marketing copy.

Generates content for any project. Brand voices configurable.
Uses local LLMs — $0 cost per post.
"""
import time
from dataclasses import dataclass, field
from .llm import LLM


@dataclass
class ContentPiece:
    type: str  # "blog", "tweet", "linkedin", "marketing", "video_script"
    title: str
    content: str
    project: str
    tags: list = field(default_factory=list)
    timestamp: float = 0


class ContentGenerator:
    """Generate marketing content using local LLMs."""
    
    BRAND_VOICES = {
        "HireFlow360": "Professional, innovative, employee-first. Tone: confident but not arrogant. Focus on trust, verification, AI-powered hiring.",
        "FierceFlow": "Bold, financial, empowering. Tone: authoritative, modern fintech. Focus on payments, payroll, financial freedom.",
        "MasterStaff": "Reliable, experienced, people-focused. Tone: trustworthy staffing partner. Focus on workforce solutions, EOR, consulting.",
        "TrustedHire": "Secure, thorough, transparent. Tone: the gold standard in background checks. Focus on verification, compliance, trust.",
    }
    
    def __init__(self, llm: LLM = None):
        self.llm = llm or LLM(base_url="http://192.168.5.100:51802/v1")  # 32B for writing
    
    def blog_post(self, topic: str, project: str = "HireFlow360", word_count: int = 800) -> ContentPiece:
        """Generate a blog post."""
        voice = self.BRAND_VOICES.get(project, self.BRAND_VOICES["HireFlow360"])
        
        content = self._generate(f"""Write a {word_count}-word blog post about: {topic}

Brand voice: {voice}

Format:
- Engaging headline
- Hook opening paragraph
- 3-4 sections with subheadings
- Actionable takeaways
- CTA at the end

Write naturally. No AI-sounding phrases like "in today's fast-paced world" or "it's important to note".""")
        
        return ContentPiece(type="blog", title=topic, content=content, project=project, timestamp=time.time())
    
    def tweet(self, topic: str, project: str = "HireFlow360") -> ContentPiece:
        """Generate a tweet/X post."""
        voice = self.BRAND_VOICES.get(project, "")
        content = self._generate(f"""Write a tweet about: {topic}
Brand: {project}. Voice: {voice}
Max 280 chars. Include 1-2 relevant hashtags. Be punchy and engaging.""")
        
        return ContentPiece(type="tweet", title=topic, content=content, project=project, timestamp=time.time())
    
    def linkedin_post(self, topic: str, project: str = "HireFlow360") -> ContentPiece:
        """Generate a LinkedIn post."""
        voice = self.BRAND_VOICES.get(project, "")
        content = self._generate(f"""Write a LinkedIn post about: {topic}
Brand: {project}. Voice: {voice}
Format: Hook line → story/insight → lesson → CTA
Keep under 1300 chars. Professional but not boring.""")
        
        return ContentPiece(type="linkedin", title=topic, content=content, project=project, timestamp=time.time())
    
    def marketing_copy(self, page: str, project: str = "HireFlow360") -> ContentPiece:
        """Generate website marketing copy."""
        voice = self.BRAND_VOICES.get(project, "")
        content = self._generate(f"""Write marketing copy for the {page} page of {project}.
Brand voice: {voice}
Include: headline, subheadline, 3 benefit bullets, social proof placeholder, CTA button text.
Be specific to {project}'s actual features — not generic.""")
        
        return ContentPiece(type="marketing", title=f"{project} {page}", content=content, project=project, timestamp=time.time())
    
    def _generate(self, prompt: str) -> str:
        try:
            messages = [
                {"role": "system", "content": "You are an expert content writer. Write naturally, avoid AI clichés."},
                {"role": "user", "content": prompt},
            ]
            response = self.llm.call(messages)
            return response.get("content", "")
        except Exception as e:
            return f"Generation failed: {e}"
