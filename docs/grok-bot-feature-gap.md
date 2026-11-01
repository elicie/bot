# Grok Bot 기능 차이 분석 (0.78.1)

작성일: 2026-11-01

## 근거

`elicie@mac-mini`에 설치된 Grok Bot 0.78.1(`com.anysphere.sand`, Electron 42)을 읽기 전용으로 조사했습니다.

- 실행 중인 프로세스의 등록 스킴: `sand-media`, `sand-canvas`, `sand-html-preview`, `sand-avatar`, `sand-mcp-app`, `sand-visualization`
- `app.asar`의 구성: `renderer`(화면), `node-agent-coordinator`(봇 실행 조정), `local-exec-daemon`(로컬 실행)
- 조정기 RPC 이름: `createGroup`, `setGroupMembers`, `setMainAgent`, `getSubagents`, `setAgentUnread`, `setAgentNotifyOnUpdates`, `setAgentHiddenFromSidebar`, `getAgentTasks`, `setAgentTaskPinned`, `createThreadFromMessage`, `readAttachmentText`, `readAttachmentImage`, `getAgentAutomations`, `runAutomationWebhook`, `createAgentFromTemplate`, `deleteAgent`, `readMcpAppResource` 등
- 화면 기능 묶음 이름과 한국어 문구 5,089개

원본 번들·자산·코드는 저장소에 포함하지 않습니다. 문구와 동작은 참고 자료로만 쓰고, 구현은 독립적으로 작성합니다. 원격 앱에서는 메시지 전송, 설정 변경, 클라우드 컴퓨터 깨우기를 하지 않았습니다.

## 로컬 앱으로 옮길 기능 (1차)

| 기능 | 원본 근거 (문구·RPC) | 로컬 구현 |
| --- | --- | --- |
| Bot 메모리 | "이 Bot이 … 기억하는 내용", "Grok Bot으로 메모리 가져오기", 메모리 목록 | 봇별 메모리 저장, 모델의 기억·잊기 도구, 세부 정보에서 보기·삭제, 텍스트 가져오기 |
| 지식 파일 | "지식", "파일은 Bot에 공유 컨텍스트를 제공하고 … 답변하도록 돕습니다" | 봇별 텍스트 지식 파일, 모델 읽기 도구, 세부 정보에서 추가·삭제 |
| Bot 템플릿 | `createAgentFromTemplate`, "Bot 템플릿" | 봇 설정(이름·지침·모양·색·스킬)을 JSON으로 내보내기·가져오기 |
| 권한 규칙 | "Grok Bot이 다음을 하려고 할 때:", 규칙 동작·저장·삭제, "내 규칙으로 자동 승인됨" | 작업 종류별 "항상 묻기 / 묻지 않고 허용" 규칙과 자동 승인 표시 |
| 루틴 웹훅 | "웹훅 키", "웹훅이 발생할 때", `runAutomationWebhook` | 루틴별 웹훅 키와 인증된 실행 엔드포인트 |
| 그룹 채팅 | `createGroup`, `setGroupMembers`, "그룹 이름", "그룹 아바타 편집" | 여러 봇이 참여하는 그룹 대화, 발화자 표시, 순서대로 응답 |
| 스레드 | `createThreadFromMessage`, "스레드에서 답장", "메인 채팅 옆에 스레드 열기" | 메시지에서 스레드를 만들어 옆 패널에서 이어가기 |
| 읽음 상태·알림 | `setAgentUnread`, "읽지 않음으로 표시", "읽지 않은 활동", 메시지 소리 | 봇별 안 읽음 표시, 컨텍스트 메뉴, 브라우저 알림·소리 설정 |
| Bot 삭제 | `deleteAgent` | 컨텍스트 메뉴 삭제(확인 후, 대화 기록 정리 규칙 포함) |
| 첨부 | "놓아서 첨부", "사용자 첨부 파일", `readAttachmentText`/`readAttachmentImage` | 입력창 끌어다 놓기·선택으로 텍스트·이미지 첨부 |
| 수식·다이어그램 | `markdown-math-katex`, `sand-diagram-render` | 채팅의 KaTeX 수식, Mermaid 다이어그램 표시 |

## 2차 이후 또는 제외

- 2차: 사용자 지정 MCP 서버와 보관함 시크릿, 하위 에이전트/메인 Bot, 작업(할 일) 목록, HTML·캔버스·시각화 미리보기, 관리형 Chrome(P3)
- 제외(클라우드 계정·외부 서비스 의존): 결제·충전·가상 카드, 전화·음성, iMessage·메일, Slack·Teams, 1Password, 마켓플레이스, 팀 공유·서비스 계정, Grok Bot 클라우드 컴퓨터, 장소 검색
