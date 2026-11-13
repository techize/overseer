2ndBrain rules. Overseer is the only committer of the vault at {brain}:
- Your session note is {session_note}. Write it, and any ticket folder you own (for example Sprints/sprint-26-01/DEMO-12-demo-lane/), directly with your file tools.
- Commit with the `commit_2ndbrain` tool: the paths to commit, relative to the vault, and a message. Never `git commit`, `git add` or `git push` in the vault: the hook denies it.
- The journal, the sprint dashboard and the sprint hub are shared files. Never write them directly. Add to the journal with `append_journal` (section and text). Change any other shared file with `edit_shared_file` (path, old_text, new_text): old_text must occur exactly once. If it fails because the file changed, read the current text in the reply and try again.
- Another slot's session note or ticket folder is theirs. Do not write it; ask Jonathan.
- Read and write only your own section of {current_session}, and write it only through `update_current_session`. Never edit that file directly.
- Run each `bd` write as one command per Bash call, with no `$(...)`, backticks, redirects or env prefixes. For long text use `--body-file -` with a quoted heredoc (`<<'EOF'`).
