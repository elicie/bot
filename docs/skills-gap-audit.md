# Plugin and skill gap audit

Understood as: the user requested a working Grok Bot-style application with the original feature structure, including skills and technology integrations. User corrections identify an unrequested recent-conversation sidebar and a real bot-creation failure. Inspect the original skill flow before implementing it and retain prior conversations and settings.

## Live observation on 2026-11-01

Read-only navigation on the authorized Mac's Grok Bot 0.78.1 confirmed:

| Surface | Observed behavior | Current local implementation |
| --- | --- | --- |
| Sidebar | Bot tiles and ordinary bot rows; no recent-conversation section | The unrequested recent section has been removed from source; chat history remains accessible from its dialog |
| App connection | The sidebar's app-connection button opens a catalog; it includes My plugins and Bot templates | Sidebar entry opens bot-scoped private skill management; external catalog/templates remain incomplete |
| My plugins | Installed plugins, connectors, and a separate private-skills section for the selected bot | Local private-skills list with persisted enable/disable; connector support disclosed as incomplete |
| Plugin detail | Accounts, connector information, supplied skill count/list, install/remove/share controls | External connector account/authentication/install/share remains incomplete |
| Private skill detail | Name, description, Markdown instructions; Save, Delete skill, Publish, and Back | Name/description/instructions edit, Save/Delete/Back; publishing remains incomplete |
| Private skill applicability | The recovered prior-version contract is scoped to an agent and supports enable/disable; the live selected-skill editor has no toggle in the editor itself | Per-bot persisted list toggle; enforcement belongs to the authenticated broker |
| Skill creation | The prior-version empty state tells the user to ask a bot to create a skill; no live create/save operation was executed | Prepares a composer request while preserving existing text; model proposals require complete one-use save review. Direct addition is labeled a local extension |
| Inference | Original execution semantics were not exercised remotely | Local source engine discovers app skill tools and reads enabled bot-owned instructions through its broker; real-model relevance selection remains unverified |

The catalog displays real connectors with optional bundled skills. Opening a connector detail and a private skill required no account connection or save. The original app was returned to its main chat. No plugin was connected/installed/removed, no skill was saved/deleted/published, and no remote message or task was run. Private skill names, bodies, and account values are excluded from this document and project fixtures.

## Boundaries requiring explicit implementation

Skill CRUD must persist, scope to the correct bot, preserve drafts/errors, and connect to actual model execution. Instruction loading must retain confined file access, one-use file/computer approval, recipient chat-only restrictions, source-session continuity, and key redaction. A model-generated skill needs a reviewable save proposal and explicit user approval. Local skills and a cloud connector marketplace have separate storage, authentication, and execution contracts. A descriptive placeholder does not implement either capability.

## Pinned source audit and implementation

The audit used Grok Build revision `2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8`. Native skill discovery enumerates filesystem roots and supplies metadata; its ordinary Skill alias loads bodies using the native file reader. This app's curated profile exposes `search_tool` and `use_tool`, so native discovery alone cannot complete that loading flow. Native skill extensions and slash expansion can read host paths outside the workspace broker. They remain disabled. The app stores validated instruction records in its existing private atomic state and supplies an enabled metadata index through session rules. Authenticated `list_skills`, `read_skill`, and `create_skill` derive their bot from the current run. Bot-delivery recipients retain chat-only behavior. Enabled-content fingerprints replace saved engine sessions after skill changes; old visible messages remain stored.

The upstream MCP result path defaults to a 20,000-byte truncation threshold (`xai-grok-tools/src/util/mcp_truncate.rs`). The app caps serialized skill text at 18 KiB and tests a complete Unicode tail sentinel through actual engine/provider requests. It budgets the final redacted response, rejects over-budget reads explicitly, and paginates skill metadata below that cap. JSON field values are redacted before index serialization; literal data-URI syntax uses lossless JSON escapes to avoid upstream attachment rewriting. Enabled instructions are bounded per bot. Skill admission sizes the exact prospective state and preserves queued dispatch reservations. Strict SKILL.md API import accepts name/description YAML and Markdown, rejects unsupported tool/script/model/hook fields and malformed/duplicate YAML, and grants no execution rights. A skill's required connector, software, or executable runtime is a separate capability.

Upstream workflows are Rhai programs that can spawn agents, write scratch files, and run Git operations through host services. They need their own permission/execution contract and remain disabled. The app's routines schedule ordinary prompts and retain their existing behavior.

Bot creation is tracked separately. Chrome tests reproduce hidden rejected-save feedback and verify visible errors plus successful creation at desktop and mobile widths. The cause of the user's reported creation failure remains undiagnosed. Deterministic providers establish tool wiring and instruction delivery; autonomous real-model relevance selection and instruction following remain unverified.
