# Bot 메모리 · 지식 · 템플릿

작성일: 2026-11-01 · Rust 서버 전용 (Node `server/`는 변경하지 않음)

## 데이터

`Bot`에 선택 필드 두 개를 추가했습니다. 값이 없으면 직렬화하지 않으므로 기존 `state.json`과 호환됩니다.

| 필드 | 내용 | 제한 |
| --- | --- | --- |
| `memories` | `{id, text, source: user\|bot\|import, createdAt}` | 항목 500자, 봇당 200개, 엔진에 넘기는 `[{id,text}]` JSON 16 KiB |
| `knowledge` | `{id, name, bytes, hash, createdAt}` (내용은 디스크) | 파일 512 KiB UTF-8, 봇당 50개·8 MiB, 이름 200자 |

지식 파일 내용은 `<data>/knowledge/<botId>/<fileId>.txt`에 저장합니다(디렉터리 0700, 파일 0600, 원자적 쓰기). 읽을 때 `O_NOFOLLOW`, 크기, SHA-256을 확인합니다. 저장소를 열 때 메타데이터가 없는 내용과 삭제된 봇의 디렉터리를 정리합니다. Bot 삭제 기능도 이 정리에 의존할 수 있습니다.

## HTTP (`crates/core/src/http_bots.rs`)

| 경로 | 동작 |
| --- | --- |
| `POST /api/bots/:botId/memories` `{texts, imported?}` | 추가(중복은 대소문자·공백 무시하고 건너뜀). `{added, existing, state}` |
| `POST /api/bots/:botId/memories/preview` `{text}` | 붙여넣은 텍스트를 항목으로 나누고 중복·오류 표시. 저장하지 않음 |
| `DELETE /api/bots/:botId/memories/:id` | 삭제 |
| `POST /api/bots/:botId/knowledge` `{name, content}` | 추가 |
| `GET /api/bots/:botId/knowledge/:id` | `{file, content}` (API 키 가림) |
| `DELETE /api/bots/:botId/knowledge/:id` | 삭제 |
| `GET /api/bots/:botId/template?memories=true` | 템플릿 JSON |
| `POST /api/bot-templates` `{template}` | 새 Bot 생성. `{bot, state}` |

사용자 변경은 스킬과 같이 실행 중인 작업이 없을 때만 허용합니다(409).

## 모델 도구 (`runtime/broker/bot_data.rs`)

`list_memories`, `save_memory`, `forget_memory`, `list_knowledge`, `read_knowledge`. 저장·삭제는 1회 승인 카드를 거칩니다. 권한 규칙 기능은 `MEMORY_SAVE_CATEGORY = "memory_save"`, `MEMORY_FORGET_CATEGORY = "memory_forget"`로 연결합니다(`approve_memory`). 이미 기억하는 내용을 다시 저장하면 승인 없이 기존 항목을 돌려줍니다. 한 턴에 최대 5개 저장합니다. `read_knowledge`는 바이트 offset으로 페이지를 나눠, 이스케이프 후 응답이 18 KiB 이하가 되게 합니다. 수신 대화(봇 메시지)에서는 도구를 쓸 수 없고 규칙에도 넣지 않습니다.

세션 규칙에 메모리(`Bot memories (data)`)와 지식 색인(`Knowledge file index (data)`)을 JSON 데이터로 넣고 API 키를 가립니다. 메모리나 지식이 바뀌면 세션 fingerprint가 바뀌어 다음 턴은 새 엔진 세션과 최근 대화 이어가기로 시작합니다. 메모리와 지식이 없는 봇의 fingerprint는 그대로라 기존 세션이 유지됩니다.

## 템플릿 결정

`{format: "grok-bot.bot-template", version: 1, bot: {name, color, instructions, shape?}, skills: [{name, description, instructions, enabled}], memories?: [{text}]}`.

- 메모리는 사용자가 "메모리 포함"을 선택할 때만 넣습니다.
- 지식 파일은 넣지 않습니다. 내용이 크고 설치별 데이터이며, 이름만 넣으면 가져온 Bot이 없는 파일을 가리키게 됩니다.
- ID, 시각, revision, 대화, 루틴, 고정·숨김 상태, 자격 증명은 넣지 않습니다. 가져올 때 모든 ID를 새로 만듭니다.
- 모르는 필드는 거절하고, 각 값은 직접 편집할 때와 같은 규칙으로 검증합니다. 하나라도 틀리면 아무것도 만들지 않습니다.
- 내보낸 텍스트에서는 API 키를 가립니다.

## 원본과 다른 점

- 원본 메모리는 팀 공유 메모리와 개인 메모리로 나뉘고 모델이 승인 없이 기록하는 것으로 보입니다("팀 기억 · …"). 여기서는 봇별 메모리 하나이고, 모델 저장·삭제마다 승인을 받습니다.
- 원본 "Grok Bot으로 메모리 가져오기"는 설정 화면에서 텍스트를 붙여넣어 Bot에게 추가를 요청하는 흐름으로 보입니다. 여기서는 세부 정보 패널에서 붙여넣은 내용을 항목으로 나누고, 사용자가 확인한 항목만 바로 저장합니다.
- 원본 지식은 팀 파일·커넥터 문서를 포함할 수 있습니다. 여기서는 UTF-8 텍스트 파일만 지원합니다(PDF·이미지 없음).
- 원본 템플릿은 클라우드에 게시·공유하는 링크 방식입니다. 여기서는 JSON 파일 내보내기·가져오기만 합니다.

## 원본에서 확인할 것

메모리 목록이 표시되는 위치와 항목 편집 가능 여부, 모델이 메모리를 기록할 때 승인을 묻는지, 지식 파일 형식·크기 제한, 템플릿에 메모리·지식이 포함되는지.
