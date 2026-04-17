---
name: tilde-expansion-rule
description: Rule that ~/ paths must be expanded to $HOME or full paths before passing to tools
type: reference
---

## Tilde Expansion Rule

When using paths starting with `~/`, you must first expand them to the user's home directory (e.g., `/home/USER` or `$HOME`) instead of passing `~/` directly to tools like `read_file`, `write_file`, etc. Tilde expansion does not work with these tools.

**Example:**
- ❌ `~/brain/notes/foo.md` — will not work as expected
- ✅ `/home/USER/brain/notes/foo.md` — correct
- ✅ `$HOME/brain/notes/foo.md` — correct
