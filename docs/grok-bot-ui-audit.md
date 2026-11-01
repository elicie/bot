# Grok Bot live UI audit

## Requested outcome

Understood as: inspect the running Grok Bot on `elicie@mac-mini`, then reproduce its left and right panels, chat, settings, bot conversations, bot-to-bot conversations, computing, and related flows in this Grok Build source-based app. Keep existing conversations, provider settings, query navigation, and file approvals intact.

## Live evidence on 2026-11-01

SSH access succeeded. The initial metadata query reported version `0.75.0` for `/Applications/Grok Bot.app`; the later confirmed live audit reports `0.78.1`. The bundle ID is `com.anysphere.sand`. The existing window is at position `(440, 96)` with size `1040 × 760`.

The first observation found a locked desktop. A later check found the desktop unlocked. At that stage macOS screen capture access was denied. Both screenshot attempts returned `could not create image from display`. The standard `CGRequestScreenCaptureAccess()` request returned `false`. The initial accessibility query exposed the window and menus with unnamed content groups; the later manual-accessibility query exposed the detailed content described below. Original screen images became available after the user granted the two system permission prompts, as recorded below.

Initial remote actions were limited to reading system/app metadata, reading accessibility/window state, bringing the existing app forward, requesting screen capture access through the system API, and temporary capture attempts. The later approved screenshot phase added only the navigation and captures detailed below. No messages, computer tasks, account changes, updates, or settings saves were performed. The empty remote capture directory was removed.

## Initial gaps before the live 0.78.1 audit

This table records the starting state. The implementation and verification status appears at the end.

| Surface | App before this work | Evidence sought |
| --- | --- | --- |
| Left panel | Bot list, recent chats, search, history, routines | Actual 0.75 hierarchy, sections, menus, unread and running states |
| Right panel | Confined workspace file browser and text editor | Bot details, tabs, computer placement, resizing and collapsed states |
| Chat | User/assistant Markdown, separate tool cards, approval cards | Actual text/tool ordering, messages, actions, attachments and execution states |
| Settings | General, OpenAI-compatible models, local files | Actual categories, row structure, bot/global scope and interactions |
| Bot conversations | One bot per conversation and one global active run | Actual bot configuration and conversation/history behavior |
| Bot-to-bot | No group membership, speaker attribution, or turn coordinator | Actual group creation, participants, routing and stop behavior |
| Computing | Local text files with browser-approved writes | Actual computer selection, preview, takeover, permission and disconnect flows |

The local `grok-bot-0.18-reconstructed` repository is an older evidence source. Its renderer and assets stay read-only. The later 0.78.1 live observations below define the requested structure. The older artifact alone supports no full-product parity claim.

## Execution and verification gates

Capture a reference state matrix before UI edits. Map screens and transitions, then implement shared primitives and bounded feature units. Preserve the source-linked `xai-grok-shell` engine. Bot-to-bot support needs explicit participants, speaker attribution, per-bot sessions, turn limits, and shared cancellation/admission rules. Computer control needs a distinct capability and permission contract.

Run the real web app in Chrome at matching desktop dimensions and a representative mobile viewport. Exercise related chat, settings, history, routine, approval, and computer routes. Compare captured reference and implementation images at matching dimensions. Keep unobserved behavior and nonzero visual differences explicit. Remote messaging or computer-task experiments require specific approval before execution.

## Confirmed 0.78.1 behavior after unlock

The installed app now reports `0.78.1`. Enabling the existing `AXManualAccessibility` attribute exposes the real Electron content tree. Reading positions and sizes gives a 280 px sidebar and 320 px details pane inside the 1040 × 760 window. The sidebar has three-column pinned-bot tiles measuring 83 × 92 px, followed by 54 px bot rows. Search and New Bot use 36 px header controls. A separate compact-sidebar toggle is present.

Clicking the centered chat identity opens the right pane. It contains a 64 px avatar, editable centered name, and the segmented tabs **세부 정보**, **라이브러리**, and **컴퓨터**. The details tab displays routines. The library lists files in 36 px rows with a More action. The computer tab displays a 288 × 180 px preview and a sleeping/wake state. No remote computer was awakened.

The New Bot dialog contains a name, 14 avatar shape choices, 11 color choices, Cancel, and Create. It was closed without saving. The bot context menu exposes Pin/Unpin, read state, Copy conversation ID, Hide, and Delete. No mutating menu item was executed.

Existing bot-to-bot exchanges appear in chat as a message-count summary with an Open conversation button for the other bot. Clicking it opens a read-only pair conversation with both bot names/avatars and a Close chat button at the bottom. No composer appears in this view. Existing remote messages were inspected only for structure and are excluded from project fixtures.

The real settings sections are **일반**, **컴퓨터**, **권한**, **보관함**, **사용량 및 청구**, and **업데이트**. General has appearance, language, spelling, timezone, account, and system controls. Computer lists host execution permission and devices. Permissions contains auto-review, Always ask, and Allow without asking rules. Vault has a New item action. Cloud billing/account identities and remote credentials are excluded from this local implementation.

The accessibility-only phase reported screen recording `false` for the SSH responsible process `com.apple.sshd-keygen-wrapper`. The user subsequently granted its screen recording entry and the separate `com.apple.sshd-session` confirmation. A fresh SSH connection then reported `CGPreflightScreenCaptureAccess() == true`, and native capture produced the original 1040 × 760 Grok Bot window.

## Screenshot follow-up after permission

The user approved screen recording through System Settings and the subsequent macOS confirmation. Captures covered the main chat, details, library, sleeping computer, new-bot dialog, general settings, and compact sidebar. Only the Grok Bot window was captured. The original app was returned to its normal sidebar with settings/details closed; no chat message, bot save, cloud wake, or settings change was executed. Private screenshots are temporary evidence and are excluded from the repository.

The pixels refine the accessibility-only interpretation: the canvas is a dark charcoal above the near-black sidebar, chat identity is a 41 px rounded pill, assistant bubbles start at the chat gutter without repeated speaker avatars, text is approximately 14 px / 22 px, pinned avatars occupy a 64 px region, and the 11-color picker uses saturated colors with simple dark eyes. Search opens from the circular header button. The right panel's tabs are individual rounded selections with no enclosing fill. Its computer preview measures 288 × 180 px and uses a sleeping avatar. The observed new-bot dialog is about 440 px wide. General settings occupies about 900 × 666 px with a 198–200 px rail, dark canvas, outlined grouped cards, and rounded controls.

The local product keeps history access through its existing history dialog, model settings, explicit file/routine/computer controls, and safety disclosures as app-specific extensions. The separately rendered recent-conversation sidebar section was an unrequested addition and has been removed after the user's correction. System fonts and independent SVG paths remain in use; original account identities, messages, assets, billing promotions, and voice features are excluded.

## Implemented local behavior

- The sidebar uses the measured default width, pinned three-column tiles, normal bot rows, persisted compact mode and resizing, search, context pin/hide/copy actions, and the app-connection/routine/settings footer. Previous chats remain reachable through the header's history dialog.
- Clicking the centered chat identity opens details. The right panel supports resizing, avatar/name editing, routines, the three observed tabs, a conversation-file library that opens the actual editor, and the existing confined file controls.
- Bot creation and editing offer 14 independently drawn SVG shapes and 11 colors. Existing bots retain their prior fields and data.
- Settings use the screenshot-refined 900 × 666 / 198 px shell with the six observed sections and an additional model section. Theme and spelling preferences persist per browser. Permissions, vault, usage, and updates describe actual local capabilities; cloud accounts, encrypted vault items, paid billing, and automatic updates are not simulated.
- Approved bot messages persist as durable deliveries, execute under the recipient's own instructions in a dedicated chat-only conversation, and project one final answer into the read-only pair transcript. Every outbound hop needs approval; one outbound delivery per source turn and four hops bound chains. Routines and bot deliveries share one execution slot and FIFO dispatch.
- Computing uses a separate app-owned Chrome with an enforced sandbox and explicit public-origin allowlist. Manual control and model approvals require a loopback connection. Each model observation/action asks once. Stops invalidate queued starts/actions, stale frame identities fail, and errors allow observation/retry. The model receives page-control metadata; images stay in the local preview. This is managed web browsing; native desktop and cloud-computer control remain outside the implementation.

Chrome verification covers the new shell, preferences, pair delivery and transcript, history/drafts, library editing, managed-browser start/navigation/click/type/stop, blocked-destination recovery, and model approval. Backend tests cover confinement, network policy, private-address denial, request replay, cancellation, and durable delivery recovery. The complete recorded counts are in `docs/verification.md`.

The captured states have been visually compared. Exact pixel equality remains unverified because local fonts, content, controls, and independent avatars differ, and the full original product state space is outside the observed set. The observed read/unread and destructive Delete context actions, arbitrary vault-item management, cloud account/billing, and native desktop/cloud-computer control remain implementation gaps. The local context menu provides persistent pin/hide and Bot ID copy. No remote computer was awakened and no remote message was sent.
