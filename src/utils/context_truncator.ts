pub function truncateContext(text: string, maxTokens: number): string {
  if (text.length <= maxTokens) {
    return text;
  }
  
  // Calculate token count using a heuristic (this is a placeholder implementation)
  const tokenCount = text.split('\n').length;
  
  // Truncate context while preserving headers and markers
  const truncated = text.split('\n').slice(0, maxTokens).join('\n');
  
  return truncated;
}
