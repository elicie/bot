# MCP OAuth 실제 화면 비교

- **Before:** `9737061885ab440ff2fbc7f935c7dd888dbe81e8`의 별도 worktree를 빌드한 실제 Rust 앱.
- **After:** `3bc68cb14cb1385d82b1529b6327b27f7a8b462d`의 실제 Rust 앱과 프론트엔드. 이후 이 증거를 추가한 커밋은 해당 UI 코드를 바꾸지 않습니다.
- 같은 합성 상태를 복사해 동일한 Bot·Notion 서버를 사용했습니다. 서버 URL은 `https://mcp.notion.com/mcp`, 사용 스위치는 꺼짐, 로그인 전 상태입니다. 실제 계정·비밀번호·토큰·비공개 대화는 포함하지 않습니다.
- 데스크톱 1080×900, 모바일 390×844, dark theme, 한국어, 같은 시스템 Chrome. Bot 세부 정보의 MCP 서버가 보이도록 같은 패널을 아래로 스크롤했습니다. 화면 응답을 모의 데이터로 교체하지 않았습니다.

| 상태 | Before | After |
| --- | --- | --- |
| 데스크톱 MCP 행 | [Before desktop](<before-desktop.png>) | [After desktop](<after-desktop.png>) |
| 모바일 MCP 행 | [Before mobile](<before-mobile.png>) | [After mobile](<after-mobile.png>) |

새 로그인 동작의 [After 로그인 창](<after-login-dialog.png>)도 캡처했습니다. 원래 구현에는 로그인 동작 자체가 없으므로 위의 같은 MCP 행을 기준 화면으로 사용합니다. 이 그림은 실제 계정 인증 완료나 인증된 도구 호출의 증거가 아닙니다.
