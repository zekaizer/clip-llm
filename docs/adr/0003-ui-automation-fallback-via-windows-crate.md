# ADR-0003: Read the selection through UI Automation, via the `windows` crate, when the copy fails

- Status: accepted
- Date: 2026-10-08

## Context

Selection capture on Windows injects Ctrl+C and reads the clipboard. Measured
on Windows 11 24H2 against eleven targets, the copy silently never lands in
cases where text is plainly selected: an Edge page whose selection ends in a
`contenteditable` element (7/7 attempts, Ctrl+Insert included). UI Automation
(`IUIAutomationTextPattern::GetSelection`) returned the full text there.

The same measurement rules UI Automation out as the primary path:

| Target | `GetSelection` with text selected |
|---|---|
| Notepad, classic Edit, Edge page, Windows Terminal, conhost | text |
| VS Code, Firefox, Edge PDF viewer | empty |
| Notepad++ (Scintilla), Explorer file list | no TextPattern |

An empty answer therefore proves nothing, and a non-empty one is plain text
only: table cells come back one per line and the HTML flavor, which carries
images, is absent.

UI Automation is COM. Every other Win32 call in this crate goes through
`windows-sys`, which has no COM interface wrappers.

| Option | Cost |
|---|---|
| `windows` 0.58, features `Win32_System_Com` + `Win32_UI_Accessibility` | already resolved at this version through `wgpu-hal` → `gpu-allocator`; only the two feature modules are new |
| `uiautomation` 0.25 | pulls `windows` 0.62, a second copy of the bindings; exposes no timeout setters |
| hand-written vtables over `windows-sys` | no dependency; unsafe definitions of six interfaces to maintain |

## Decision

Add `windows` 0.58 as a direct Windows-only dependency and use it in
`src/platform/uia.rs` alone. COM goes through `windows`; everything else
stays on `windows-sys`.

`Platform::selected_text` is consulted only after the copy has failed
(`NoTextAfterCopy` or `EmptyCopy`), and only a non-blank result is used. The
call runs on the capture thread (multithreaded apartment, owns no window),
with the client's connection and transaction timeouts lowered from the 2 s /
20 s defaults and an overall time budget, walking from the focused element up
through its ancestors as far as the enclosing document. A password field on
that path is never read: its text pattern answers with mask characters
(measured on a Win32 password Edit and on Edge), and the copy it refused is
not to be worked around. A property that cannot be read ends the lookup.

## Consequences

- A capture that used to end in "could not capture selected text" succeeds
  where the control exposes its selection; the result is text without images
  or markup, with `\r` and `\r\n` folded to `\n`.
- The failure path gets longer by the lookup when nothing is selected. A
  warm lookup measured 10-45 ms; a suspended target blocks until the
  connection timeout.
- The first lookup against a browser that has not built its accessibility
  tree yet can find nothing (3 of 6 cold Edge starts); the next capture in
  that tab succeeds.
- A page that suppresses its `copy` event is still read, as assistive
  technology would read it.
- Requesting TextPattern from Chromium turns on accessibility for that tab.
  This happens only on the fallback path, not on every capture.
- The MSVC release executable grew by 8.7 KB across the capture changes
  that introduced this (23,143,936 to 23,152,640 bytes).
- Two Win32 binding crates are direct dependencies. `windows` must stay on
  the version `wgpu` resolves, or the bindings are compiled twice.
