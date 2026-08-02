@AGENTS.md

# Claude Code specific tips

## Preferred tools for file operations

Use Claude Code's built-in tools instead of shell commands for file operations:

- **Glob** over `find` or `ls` for finding files by pattern
- **Grep** over `grep` or `rg` for searching file contents
- **Read** over `cat`, `head`, or `tail` for reading files
- **Edit** over `sed` or `awk` for editing files
- **Write** over `echo` or heredocs for creating files

These built-in tools provide better sandboxing and don't require explicit
permission allowlisting.
