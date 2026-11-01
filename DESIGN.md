# Design

## Visual authority

The user selected Grok Bot as the fixed reference. Live 0.78.1 authorized window screenshots, accessibility observations, and measured geometry in `docs/grok-bot-ui-audit.md` define the current structure and interactions. The older 0.18 screenshot supplies secondary dark-theme evidence. The implementation uses original React and CSS, system fonts, open-source interface icons, and original geometric bot avatars. The workspace is an Operate surface: completing chats, approvals, and file work drives its hierarchy.

## Layout

A full-height desktop workspace. The default 280 px sidebar has a 60 px header, circular search/add buttons, an on-demand search field, three-column pinned tiles with 64 px avatars, 54 px bot rows, and footer controls for app connections, routines, hidden bots, and settings. It supports resizing and an 88 px compact mode. A centered 41 px name pill inside the 60 px chat header opens the default 320 px resizable details panel with a 64 px avatar and 세부 정보 / 라이브러리 / 컴퓨터 tabs. The computer tab contains a 16:10 managed-browser preview and the confined workspace editor; its content scrolls to keep files reachable. Settings use the screenshot-measured 900 × 666 px overlay and a 198 px navigation rail. Mobile uses a dismissible sidebar, full-width details, and horizontally scrolling settings sections.

## Tokens

- Canvas `#111111`.
- Sidebar `#0d0d0d`.
- Elevated surface and assistant bubble `#202020`.
- Settings navigation `#111111`.
- User bubble `#3b3b3b`.
- Main text `#fcfcfc`.
- Secondary text `#b7b7b7`.
- Hairline `#ffffff1a`.
- Accent `#1084fe`.
- Success `#38d591`.
- Error `#ff5667`.
- System UI sans-serif, 14 px chat text, 12 px metadata, 22 px message line height.
- Compact controls use 6–8 px radii. Messages use 16 px radii. Chat identity and selected details tabs use pill shapes. Settings overlay uses 14 px corners.

## Behavior

The sidebar follows the original bot-only list and exposes no separate recent-conversation section. The existing chat-header history action retains access to older conversations. Bot context actions persist pinning and hiding. Bot creation offers 14 independent shapes and 11 colors. The selected conversation stays marked and remains reachable after starting another chat. Query routes restore bot, conversation, routine, and read-only bot-pair selection on reload and browser navigation. Pair transcripts show both bot identities, delivery state, execution links, and a close action; chat drafts survive round trips.

All visible controls perform their named action. Empty, loading, disconnected, running, awaiting-approval, cancelled, and error states use real application state. The composer supports Enter to send and Shift+Enter for a new line. A running task exposes a stop control. File writes show a pending or explicit-save action and report success only after the backend confirms the write. Dialogs manage focus and Escape. Focus indicators, labels, reduced-motion preferences, and keyboard navigation apply throughout.

## Private skills

The sidebar's 앱 연결 entry opens an observed-style 900 × 666 dialog with a navigation rail, selected-bot identity, private skill list, enabled switches, and a name/description/Markdown editor. Direct addition and SKILL.md import are identified as local extensions. Model creation prepares a composer request, preserves existing text, and saves only after complete one-use review. Drafts survive live snapshots and rejected saves; close, tabs and navigation guard unsaved work. The editor shows its serialized byte budget and keeps errors visible above its action footer. Mobile uses horizontal section tabs and a single-column editor. External connector accounts, install, share and publishing remain explicitly incomplete.

## Routines

The sidebar's 루틴 action opens a list and detail surface in the existing main stage. Creation and editing use an inline form with explicit schedule fields and timezone. Details show the selected bot, prompt, next server-calculated execution time, pause/resume, manual run, and retained history. History opens the linked conversation; queued conversations expose cancellation. Drafts survive live snapshots, and navigation confirms before discarding a changed routine form. Mobile stacks the form and details within the existing drawer navigation.

## Verification

Exercise all workflows in Chrome at 1040 × 760 and representative mobile dimensions, and inspect captured implementation images. Compare live window screenshots, accessibility dimensions, and transitions against the audit. The user granted both macOS capture prompts. Private original captures stay temporary; exact full-product pixel equality remains unverified.
