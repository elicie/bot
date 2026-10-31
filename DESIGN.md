# Design

## Visual authority

The user selected Grok Bot as the fixed visual reference. The baseline is `../grok-bot-0.18-reconstructed/docs/assets/router-settings.png`, with conversation and settings layout evidence from that repository. The implementation uses original React and CSS, system fonts, open-source interface icons, and original geometric bot avatars.

## Layout

A full-height desktop workspace. A 280 px left sidebar contains a compact title/action row, search, bot rows, and bottom settings/account controls. The main conversation has a 51 px header, a scrollable transcript, and a rounded bottom composer. A collapsible computer panel contains the selected local workspace, file browser, and text editor. Settings use a centered two-column overlay with a 182 px navigation rail and a dark content surface. Mobile uses a dismissible sidebar and a full-width computer panel.

## Tokens

- Canvas `#070707`.
- Sidebar `#111111`.
- Elevated surface `#181818`.
- Settings navigation and assistant bubble `#262626`.
- User bubble `#5a5a5a`.
- Main text `#fcfcfc`.
- Secondary text `#b7b7b7`.
- Hairline `#ffffff1a`.
- Accent `#1084fe`.
- Success `#38d591`.
- Error `#ff5667`.
- System UI sans-serif, 14 px base text, 12 px metadata, 20 px content line height.
- Compact controls use 6–8 px radii. Messages use 18 px radii. Settings overlay uses 14 px corners.

## Behavior

All visible controls perform their named action. Empty, loading, disconnected, running, awaiting-approval, cancelled, and error states use real application state. The composer supports Enter to send and Shift+Enter for a new line. A running task exposes a stop control. File writes show a pending or explicit-save action and report success only after the backend confirms the write. Dialogs manage focus and Escape. Focus indicators, labels, reduced-motion preferences, and keyboard navigation apply throughout.

## Verification

Compare desktop screenshots with the available baseline and exercise all workflows in Chromium. Check a representative mobile viewport. A single reference screenshot supports layout and token comparison; it does not establish full-application pixel parity.
