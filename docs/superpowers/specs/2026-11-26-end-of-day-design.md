# End of day (#88)

## Why

At the end of the day the user runs the checks by hand. Did every slot that
ran write its session logs, with outcome and highlights? Does any stretch of
work have no log covering it? Then the notes folder's split script shares the
day's minutes across tickets and epics, and its output is pushed to the time
tracker. Those steps live in three places and get run in the wrong order: a
missing log found after the split means running the split again. One button
runs them in order and shows the result as a draft to fix, edit and approve.

## Goals

- An End of day button on the board header and a palette command (Cmd+K)
  open an overlay that shows the draft for today.
- The draft has three parts, in this order:
  1. **Problems.** The session log check for each stretch of every slot that
     ran today, the Today panel's red gaps, and the split script's own notes
     (a session with no start or end, a session that ends before it starts,
     the script failing). Each problem links to its slot (go to the slot) or
     its log file (open it).
  2. **Minutes per ticket**, read-only, as the script worked them out.
  3. **Hours per epic**, editable in quarter hours, with the day's target and
     what is left to place. Approve is on only when they total the target
     and the split succeeded.
- Run again rebuilds the draft from scratch, so a log fixed after the first
  run shows up.
- Approve writes the approved hours to the day's time log note in the notes
  folder, and to the allocation file that `jc push` reads.
- `overseer ctl eod` prints the same draft.

## Non-goals

- Sending anything to the time tracker or the issue tracker. The user runs
  `jc push` themself (the user's answer, below).
- Overseer's own minute split. The script is the only place the rule lives
  (the user's answer, below).
- Past days. Only today (local time).
- Fixing logs from the overlay. The links take the user to the slot or file.
- Approving from `ctl`. It only prints.

## Anti-goals

- A click that logs real time. Nothing leaves the machine.
- A second copy of the split rule that can disagree with the script.
- A draft that hides why the split failed: a FATAL from the script shows its
  message, and the hours part says no hours could be worked out.
- Silently replacing an allocation file that may already have been edited or
  pushed.

## Approaches considered

Where the split comes from:

1. **Run the notes folder's split script (chosen).** `day-minutes.py`
   already does the 1/N split per concurrent minute, the epic mapping
   through `jc`, the BAU fill to the day's target and the quarter-hour
   rounding. Running it keeps one rule.
2. **Overseer works it out** from its own stretches and each slot's ticket.
   It works without the notes folder, but it duplicates the rule, can
   disagree, and has no epic mapping.

The user picked 1.

What Approve does:

1. **Save only (chosen).** Write the note and the allocation file; the user
   runs `jc push`.
2. **Save and send.** Also run a configured push command. One less step, but
   the button then logs real time.

The user picked 1.

Who runs the scripts:

1. **The daemon (chosen).** The daemon already owns writes to the notes
   folder, already runs `sync_command` and `bd_command` there, and already
   has the session log check and the day's gaps. One request returns the
   whole draft, and `ctl` gets it for free.
2. **The app**, the way the palette's bug report runs a command. Less
   protocol, but the app would still ask the daemon for the problems, and
   would write to the notes folder, which only the daemon does today.

The user picked 1.

## Decisions

**Order of the steps.** The daemon:

1. runs `eod_index_command` in the notes folder, so the index has any log
   written or fixed since the last run;
2. runs `eod_split_command` in the notes folder with `{out}` filled with a
   fresh path in Overseer's own temporary folder and `{date}` with today's
   `YYYY-MM-DD`, and reads the JSON written there;
3. runs the session log check (`session_logs::problems`) for every stretch
   of today, ended or still running, and takes the gap spans from the
   current `DayView`;
4. returns the draft.

A command whose file is missing is skipped with a problem saying so, as
`sync_command` is skipped. A command that exits non-zero is a problem with
the last lines of its stderr (a FATAL line first when there is one). If the
index command fails the split still runs, since the index may be fine from an
earlier run. Each command is killed after 180 seconds. Both commands go
through the existing `run_program` helper.

**The split's output.** The JSON the script writes with `--out`:
`date`, `target_hours`, `epics` (`key`, `summary`, `hours`) and
`derivation` (`ticket_minutes`, `ticket_epic`, `fit`, `unknown_times`,
`bad_times`, `sessions`). Overseer reads these fields only and ignores the
rest. Each `unknown_times` slug and each `bad_times` entry becomes a problem
naming the log. The `fit` line is shown under the hours.

**Config** (daemon, `config.toml`, in the settings panel like the others):

| Key | Default | Meaning |
|-----|---------|---------|
| `eod_index_command` | `["python3", "Claude/tools/session-index.py"]` | Run first in the notes folder. Empty skips it. |
| `eod_split_command` | `["python3", "Claude/tools/day-minutes.py", "--date", "{date}", "--out", "{out}"]` | Writes the split as JSON to `{out}`. Empty turns the button off. |
| `eod_time_log` | `06-Daily/{yyyy}/{mm}-{Month}/{yyyy}-{mm}-{dd}-time-log.md` | The approved note, under the notes folder. |
| `eod_allocation_file` | `~/.jira-clockify/allocations/{yyyy}-{mm}-{dd}.json` | Where the approved hours go for `jc push`. Empty skips it. |
| `eod_target_arg` | `["--target", "{hours}"]` | Added to the split command when the user sets the day's target; `{hours}` is the target in hours (`9`, `8.75`). Empty means the target cannot be changed. |

The defaults point at the 2ndBrain layout. When Overseer owns the notes
folder (`~/OverseerNotes`), the two command defaults are cleared, as
`sync_command` is, so the button says the end of day is not set up and names
the key to set.

**The day's target.** Some days run past the usual day, and the user books
more than the default (9 hours, say). The overlay shows the day's target
with its own − and + buttons, in quarter hours, starting at the script's
default (`target_hours` from the last run). Changing it runs the split again
with the new target (`eod_target_arg`, filled with the hours, is added to
the split command), so the script does the BAU fill or the scaling for the
new day and Approve still needs the total to match. Run again keeps the
target the user set. `overseer ctl eod --target 9` does the same. With
`eod_target_arg` empty the target cannot be changed and its buttons are off.

**Editing hours.** Each epic row has − and + buttons that move it a quarter
hour, never below zero, and shows the script's derived hours beside it. The
header shows the target and what is left (`7.50 of 7.50 h`, or
`0.25 h left to place`). Approve is on only when the total equals the target
and the split succeeded. The daemon checks the total again on save and
refuses a draft that does not match.

**Run again.** It replaces the draft. If the user has changed any hours, it
asks first: `Discard your changes to the hours?`.

**Approve.** The app sends the draft's id and the epic hours back. The daemon writes:

- the allocation file, in the script's own shape (`date`, `target_hours`,
  `epics` with the edited hours, and the script's `derivation` unchanged),
  so `jc push` reads it as before;
- the time log note: a `# Time log YYYY-MM-DD` heading, the approved
  hours per epic as a table (epic, summary, hours, tickets), the minutes per
  ticket, the problems still open at approval, and `Approved at HH:MM`.

If either file already exists, the overlay asks
`The allocation file for today already exists (it may have been pushed).
Replace it?` and writes nothing until the user says yes. The overlay then
shows both paths, and the `jc push` command to run, ready to copy.

**Notes writer.** The note is written whether or not `brain_writer` is on:
the user asked for this one file by clicking Approve. It is not committed;
the user's own sync picks it up. When Overseer refused the notes folder
(`notes_refused`), the button is off and says why.

**Protocol.** `Request::EndOfDay { target_quarters }` (None for the
script's default) returns `Response::EndOfDay(DayDraft)`.
`Request::SaveDay { id, hours, replace }` returns `Response::DaySaved { paths }`
or `Response::Error`; a refusal because a file exists is its own response,
`Response::DayExists { paths }`, so the app can ask. New types in the proto
take `#[serde(default)]` on every new field and an `Other` variant where
they are enums, as the timeline's did, so an older client keeps working.

**Running time.** The split calls `jc` and can take several seconds. The
overlay opens at once with `Running the checks...` and fills in when the
reply comes. The request does not block the daemon's other requests.

## Details (my calls, after the builder check)

- **The daemon keeps the split.** Each draft gets an id. The daemon keeps
  the last draft's split JSON in memory (the whole object, `derivation` as
  an opaque `serde_json::Value`), and `SaveDay` sends only the id and the
  epic hours. The daemon checks the hours against its own stored target,
  refuses an unknown or stale id (`Run the checks again`), and writes
  `derivation` back exactly as the script wrote it. Nothing is re-run on save.
- **Totals are exact**, in whole quarter hours. An epic set to 0 is left out
  of the allocation file, as the script leaves it out. The user cannot add
  an epic that is not in the draft.
- **Derived hours beside an epic** are the sum of its tickets'
  `ticket_minutes` divided by 60, labelled `from sessions`. They are before
  the BAU fill and any scaling, which the `fit` line explains.
- **A FATAL split.** `run_program` returns stdout and stderr on a non-zero
  exit, so the problems also take the script's stdout `note:` lines (a
  session with no times, one that ends before it starts), as well as the
  FATAL line. Overseer's own log check shows beside them, so a log with no
  end time is caught either way.
- **Links from split problems.** A slug links to `Sessions/<slug>.md` under
  the notes folder when that file exists, and to nothing otherwise. A
  `bad_times` entry is `slug (HH:MM-HH:MM)`, and the slug is the text
  before ` (`.
- **A missing command file** is the first element after the program that
  does not start with `-`, when it is a relative path. It is resolved under
  the notes folder. Both commands run with the login-shell environment, as
  `bd_command` does, so the script finds `jc` under a LaunchAgent.
- **The limit is 180 seconds** for each command, more than the script's own
  120 s `jc` wait. The index command takes no git lock: it writes files but
  commits nothing, and the user's own sync commits them.

## Left to the builder

- The module layout in the daemon and the exact names of the draft types.
- Where in the board header the button goes, its icon, and the palette
  command's wording.
- The overlay's layout within the pattern the palette and start card use.
- How the stderr lines are trimmed for a problem.
- The temporary folder for `{out}` and when the file is removed.

## Testing

- The draft builder from fixture JSON: a clean day; a FATAL split (no hours,
  Approve off); unknown and bad times as problems; a missing command file.
- Problems from fixture logs and a fixture `DayView`, each with its slot or
  path.
- Save: the allocation file keeps the script's shape with the edited hours;
  a total off the target is refused; an existing file gives `DayExists`, and
  `replace` writes it.
- The hours editor: quarter-hour steps, no negatives, Approve on only at the
  target.
- A daemon test with demo scripts standing in for both commands, using
  invented ticket keys (DEMO-n) only.
- `ctl eod` output from a fixture draft.
