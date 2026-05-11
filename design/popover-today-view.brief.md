# Design brief — "Today" view in the tray popover

*Hand this to claude.ai/design. It's a brief, not a mockup — context + constraints + the problem to solve. Propose the layout/IA; don't ask me to choose between options up front.*

---

## What the app is

A hotkey-driven desktop time tracker for one person (a working accountant). It lives in the Windows tray. Four global hotkeys are the whole interaction model:

| Hotkey | Action |
|---|---|
| `Ctrl+Shift+;` | start the active timer |
| `Ctrl+Shift+'` | stop the active timer |
| `Ctrl+Shift+H` | add a workstream (opens the popover in add-mode) |
| `Ctrl+Shift+/` | open / close the popover |

Time entries roll into a monthly CSV. There's a full-page web UI (Recorded Time, Export-for-billing) opened in the browser for review/billing — but **the popover is the daily driver**: glance at it, switch which workstream the timer is on, dismiss. It should never feel like a window you "manage."

## The ask

Add a **"Today" surface to the popover** that shows the time recorded *today* — the entries logged so far, with a running roll-up — so the user can sanity-check their day without opening the full Recorded Time page in the browser.

The user is thinking of this as a **tab in the popover** ("Workstreams" ⇄ "Today"). Whether it's literally a tab, a swipe-between, or a peek panel is yours to decide — but the key IA question is: *the popover today is built around "the active timer + which workstream it's on." Adding "here's my day so far" is a real second job for it.* Resolve that gracefully. Don't bury the timer/switcher; don't make "Today" feel bolted on.

## Form factor (hard constraints)

- The popover is a **borderless, non-resizable window, 430 × 720 logical px**, anchored just above the system tray, bottom-right. It auto-clamps shorter on small screens (min ~320px tall) — so the layout must degrade gracefully when vertical space is tight (scroll the list, keep the header + roll-up pinned).
- The visible **card is 360px wide**, centered, with a small downward "tail" pointing at the tray. (The extra ~35px each side is shadow breathing room — don't use it.)
- Dismiss is `Esc`, the `Ctrl+Shift+/` toggle, or clicking away. There is no title bar, no min/max/close chrome.
- Opening the full app is one line in the action bar: `Open Recorded Time ↗` — keep an equivalent affordance so "Today" reads as a *summary*, with "go to the full editable log" one click away.

## The data

A recorded-time entry (mock it; the backend that reads the CSV through this surface doesn't exist yet):

```js
{ id, date:'May 11', start:'9:08 AM', end:'9:54 AM', dur:'0:46',
  client:'Halden & Roe', engagement:'Q1 REVIEW',
  description:'Reconciled cash, tied out AR aging.',
  billable:true, locked:false, runId:null }
```

- `dur` is `H:MM`. `billable:false` entries exist (lunch/admin/breaks — engagement might be `BREAK`, `ADMIN`, `INTERNAL`).
- `locked:true` means the entry is already in an export — read-only; unlocking is a deliberate, warn-toned thing that belongs on the *full* page, not here.
- A workstream the timer is currently running on should show as a **live, in-progress row** ("running · 0:23 and counting") that ticks — it's part of "today" even though it isn't a closed entry yet.

Roll-up the design should surface (compact — this is a glance, not a report): **today's total**, **billable hours**, **# of distinct clients / engagements touched**, maybe a tiny per-client breakdown. The full Recorded Time page already has a 4-counter strip (entries · billable hours · clients · engagements) and per-day grouping — borrow that vocabulary so the two surfaces feel like one product, just sized differently.

## Interactions

- **Read-mostly.** Quick edits (fix a typo in a description, nudge a start/end time, flip the billable pill) are *nice* but optional — the full page is where heavy editing happens. If you include inline edit, match the full page's pattern: click a cell → edit → Enter/blur commits, Esc cancels; editing start/end recomputes `dur`.
- **No "new entry" flow here** — adding a workstream is `Ctrl+Shift+H` (already in the popover); logging time is start/stop. Don't duplicate that.
- **Navigation:** moving between "Workstreams" and "Today" should be one obvious gesture and instant. The popover re-renders by replacing its content; entrance animation only plays on a *fresh* open of the whole popover, not on tab switches (so switching tabs must not flash — keep it in-place).
- **Empty state:** "nothing logged yet today" needs a calm, non-nagging treatment (the app's whole voice is pull-based, no surveillance language — see the existing first-run / no-active empty states for tone).
- Keyboard: `Tab`/`Shift+Tab` currently cycle workstreams in the popover; if "Today" is a tab, decide what `Tab` does there (probably nothing destructive — maybe scroll, or move between editable cells if you add editing).

## Visual system (match it exactly)

The popover and both web pages share one token set. Use these — don't invent colors:

```
--paper:#F2F3F1   --paper-2:#ECEEEC   --panel:#FFFFFF
--ink:#14171A     --ink-2:#3C434B     --ink-3:#6A727C   --ink-4:#9097A0   --ink-5:#BFC5CA
--rule:#E1E4E6    --rule-2:#D2D7DB    --rule-strong:#BFC5CA
--signal:#1361FF  --signal-soft:#DCE8FF  --signal-ink:#0A3FA8        (the "running"/primary accent)
--good:#1E8A52    --warn:#B45309        --bad:#C0392B
--brand-a:#6366F1 --brand-b:#3730A3                                  (the "db"-ish brand mark gradient)
--sans: -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, "Helvetica Neue", Arial, sans-serif
--mono: ui-monospace, "SF Mono", "JetBrains Mono", "Cascadia Mono", Menlo, Consolas, "Liberation Mono", monospace
```

Conventions already in use, worth carrying over:
- Monospace + `font-variant-numeric: tabular-nums` for all durations / clocks / timestamps.
- Tiny uppercase mono labels with `letter-spacing:.14em` for section headers ("CLIENT", "TODAY", etc.).
- A pulsing `--signal` dot = running timer; a hollow `--ink-4` ring = idle.
- Cards: `--panel` background, `border-radius:12px`, hairline `--rule` borders, very soft shadows (`0 1px 2px rgba(20,23,26,.05)`-ish).
- The chrome bar (top of the popover): brand mark · connection dot · `Esc` hint. Whatever "Today" adds shouldn't crowd that out.
- `prefers-reduced-motion` is respected (animations collapse to ~0ms) — keep that.

## What to deliver

A **self-contained HTML file** — vanilla JS, no build step, no CDN, no framework, no external assets — exactly like the existing `src/Tray popover.html`, `src/Recorded time.html`, `src/Export for billing.html`. Either:
- a standalone `Today.html` we can wire as a new state, **or**
- a drop-in new state inside the popover's existing render (`popoverInner()` already branches between idle / running / add-form / empty states — "today" would be the next branch).

Include mocked data covering: a normal mid-day (5–8 entries, mixed billable, one running), an early-morning sparse case (1–2 entries), and the empty "nothing yet" case. Make the running row tick (1s interval) like the existing popover timer does.

## Non-goals / out of scope

- Date-range switching (Today/Yesterday/This week/…) — that's the full Recorded Time page. This surface is **today only**, period.
- The unlock-a-locked-entry flow — full page only.
- Any new global hotkey or tray-menu item.
- Renaming/rebranding anything — the app is "Time Tracker"; the brand mark is the small "db"-style rounded square (keep it).
- Real backend wiring — leave a `// wire to /recorded?day=today in live_view.rs` comment where the fetch would go.

## Reference files in this repo (look at these for tone & patterns)

- `src/Tray popover.html` — the popover today: chrome bar, idle/running headers, the workstream switcher, the add-workstream form with autocomplete dropdowns, the empty states, the `?ref` keyboard-reference card. Your new surface lives alongside these.
- `src/Recorded time.html` — the full editable log: the 4-counter scope strip, per-day grouping, click-to-edit cells, the billable pill, locked rows, the date-range switcher. Borrow vocabulary; don't copy the table wholesale into a 360px card.
- `src/Export for billing.html` — same visual system, for reference.
- `src/popover_window.rs` — confirms the 430×720 window, the tray anchoring, the `Esc`→hide IPC, the `open-recorded` IPC.
