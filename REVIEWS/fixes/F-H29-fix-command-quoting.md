# F-H29: Suggested fix commands quote every value they contain

**Findings:** H-29 (DEEPSEEK F3; KIMI ABN-M14; OPUS S9; ASTRA).
**Status:** proposed. High; fix before v1.

## Problem

When a path is blocked by a permission error, `issues` prints a command that would fix it. The shop copies that command to the clipboard with one key (`src/shop.rs:446-459`). The commands are built with `format!` and no quoting (`src/main.rs:1899-1916`):

```rust
"ssh {destination} 'sudo chown -R {user} {root}/{where_}'"
"sudo chown -R \"$(whoami)\" {spec}/{where_}"
```

`where_` is the shared prefix of the blocked paths, taken from names on disk, and the other side chooses those names. A directory named with `;`, backticks, `$(…)` or a newline makes the suggested command run the attacker's command. In the local form it runs under `sudo`, as root, when pasted. In the remote form, the single quotes protect nothing: the remote shell receives the text unquoted, so a `'` in the name closes the quoting. The paste-and-run flow is the documented purpose of this feature.

Also, when the destination has no `user@`, `user` is set to the whole destination (`:1906`), so the command chowns to the hostname.

The codebase has no shell-quoting helper at all.

## Proposed resolution

- **Add a quoting helper.** `shell_quote(s: &str) -> String` wraps the value in single quotes and replaces each `'` with `'\''`. Put it in a small shared module, which F-M-OUT uses too.
- **Local command:** `sudo chown -R "$(whoami)" <quoted spec/where_>`.
- **Remote command:** quote twice. First build the remote command with every value quoted, then quote that whole command once more as the single argument to `ssh`:
  `ssh <quoted destination> <quoted("sudo chown -R \"$(id -un)\" " + quoted(root/where_))>`.
  `$(id -un)` runs on the remote host, which also fixes the hostname-as-user bug.
- **Refuse strange names in suggestions.** If the prefix contains a control character or a newline, print "fix permissions on <escaped path> by hand" instead of a command to paste.
- **Never display raw path bytes.** The escaped form uses F-M-OUT's display sanitizer.

## Tests

- Build fix commands for a prefix containing `'`, `;`, `$(touch pwned)`, a backtick and a space. Run each command through `sh -n` to check that it parses. Then run the remote form's inner command with `sudo` stubbed out, and check that no file named `pwned` appears.
- A destination without `user@` produces `$(id -un)`, not the hostname.
- A prefix with a newline produces the manual-fix message, not a command.
