#!/usr/bin/env python3
import sys, json, re

try:
    d = json.loads(sys.argv[1])
except:
    print("ERROR: JSON parse failed")
    sys.exit(0)

if "error" in d:
    print("ERROR: " + str(d["error"].get("message", "unknown")))
    sys.exit(0)

msg = d["choices"][0]["message"]
content = msg.get("content", "")

# If content empty, try reasoning_content
if not content or not content.strip():
    content = msg.get("reasoning_content", "")

# If still empty, give up
if not content or not content.strip():
    print("ERROR: empty response")
    sys.exit(0)

# Strip DeepSeek <think> blocks (handle both <think>...</think> and reasoning...</think>)
if "</think>" in content:
    content = content.split("</think>", 1)[-1]
content = re.sub(r"<think>.*?</think>", "", content, flags=re.DOTALL)

# Try to extract code block from markdown
code_blocks = re.findall(r"```(?:bash|sh|shell)?\n(.*?)\n```", content, re.DOTALL)
if code_blocks:
    print(code_blocks[-1].strip())  # Use last code block (after reasoning)
    sys.exit(0)

# Try code block without language tag
code_blocks = re.findall(r"```\n(.*?)\n```", content, re.DOTALL)
if code_blocks:
    print(code_blocks[-1].strip())
    sys.exit(0)

# If it has a shebang, use as-is
if content.strip().startswith("#!/bin/bash") or content.strip().startswith("#!/bin/sh"):
    print(content.strip())
    sys.exit(0)

# If it has bash keywords, use as-is
bash_keywords = ["for ", "if ", "while ", "#!/bin/", "echo ", "mkdir ", "mv ", "cp ", "cat ", "grep ", "sed ", "awk ", "find ", "ls ", "cd ", "touch ", "rm ", "chmod ", "chown ", "tar ", "zip ", "unzip ", "curl ", "wget ", "ssh ", "scp ", "rsync ", "git ", "docker ", "kubectl ", "terraform ", "ansible ", "python ", "perl ", "ruby ", "node ", "npm ", "yarn ", "pip ", "conda ", "virtualenv ", "venv ", "source ", "export ", "unset ", "alias ", "function ", "declare ", "local ", "readonly ", "trap ", "exit ", "return ", "continue ", "break ", "shift ", "getopts ", "set ", "shopt ", "enable ", "builtin ", "command ", "type ", "hash ", "help ", "jobs ", "bg ", "fg ", "kill ", "wait ", "disown ", "suspend ", "ulimit ", "umask ", "eval ", "exec ", "caller ", "test ", "[ ", "[[ ", "true ", "false ", ": ", "printf ", "read ", "select ", "case ", "esac ", "done ", "fi ", "then ", "else ", "elif ", "in ", "do ", "done ", "{", "}", "(", ")", ";;", "|", "||", "&", "&&", ";", "<", ">", "<<", ">>", "$(", "${", "$((", "((", "))", "#", "!", "*", "?", "[", "]", "^", "$", "`", "\\", "\"", "~", "=", "+", "-", "_", ",", ".", ":", "@", "%", "/"]
for kw in bash_keywords:
    if kw in content:
        print(content.strip())
        sys.exit(0)

# Try to find embedded script starting with shebang
lines = content.split("\n")
script_lines = []
in_script = False
for line in lines:
    if line.strip().startswith("#!/bin/bash") or line.strip().startswith("#!/bin/sh"):
        in_script = True
    if in_script:
        script_lines.append(line)
if script_lines:
    print("\n".join(script_lines).strip())
    sys.exit(0)

# Fallback: return everything
print(content.strip())
