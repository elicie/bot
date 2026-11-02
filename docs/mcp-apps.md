# 대화 안의 MCP 앱

## 지원 범위

Rust 서버의 **사용자 지정 MCP 서버**가 제공하는 자체 포함 HTML 앱을 도구 활동 아래에 표시합니다. 일반 Markdown의 HTML/시각화 코드에는 도구 권한을 추가하지 않습니다. 기존 사용자 지정 MCP와 마찬가지로 Node 서버에서는 MCP 실행을 지원하지 않습니다. Node 상태 리더는 새 선택적 앱 필드를 보존하지만, Rust의 다른 기능까지 포함한 상태 파일의 교차 호환성을 새로 보장하지는 않습니다.

도구 정의의 `_meta.ui.resourceUri`(이전 `_meta["ui/resourceUri"]`도 인식)를 받은 성공한 호출에 대해, 같은 서버 연결의 `resources/read`로 정확히 그 `ui://` URI를 읽습니다. 응답의 URI와 `text/html;profile=mcp-app` MIME을 확인한 뒤 문자열 HTML 또는 UTF-8 base64 HTML을 저장합니다. 실패하거나 한도를 넘으면 원래 도구 결과를 유지하며 앱을 표시할 수 없다는 안내를 붙입니다.

현재 Grok Build 소스 `2bdd1d6a6369`에 고정된 앱 엔진이 공개하는 프록시는 도구의 **텍스트 content**만 온전히 전달합니다. `structuredContent`, 결과 수준 `_meta`, 이미지/오디오 등의 결과와 완전한 앱 전용 도구 목록은 제공하지 않습니다. 이 기능은 그 한계를 숨기거나 텍스트를 파싱해 원래 구조인 것처럼 재구성하지 않습니다. 따라서 해당 기능이 필요한 MCP 앱은 아직 호환되지 않습니다. 표준 전체를 구현한 MCP Apps 호스트라는 주장은 하지 않습니다.

## 저장과 한도

- 앱은 원래 assistant 메시지의 `ToolActivity.mcpApp`에 저장됩니다. HTML, 원래 도구 인수/텍스트 결과, 실행 주체 봇, 서버 레코드 ID·버전, 서버·도구·리소스 URI를 포함합니다. 서버의 URL·명령·환경 변수·헤더 같은 연결 설정을 앱 메타데이터에 복사하지 않습니다. HTML 자체가 포함하는 링크/문자열은 외부 요청이 차단된 상태로 보존되며, 인식된 비밀값은 아래 가림 처리를 적용합니다.
- HTML은 UTF-8 **200,000바이트**, 원래 인수 JSON은 **32 KiB**, 원래 텍스트 결과는 **64 KiB**, 대화 전체 앱은 **16개**까지입니다. 원래 텍스트 결과의 표시용 복사본만 제한 길이로 보관하며 원래 일반 도구 결과는 그대로 남습니다.
- 기존 앱이 없는 상태 파일도 읽습니다. 새 선택적 필드는 엄격히 검증합니다. 저장된 HTML은 새로고침과 서버 재시작 후에도 남고, 삭제·비활성화·수정된 서버의 과거 HTML도 볼 수 있습니다. 이때 과거 앱을 통한 새 호출은 거절합니다.
- 저장은 앱의 HTML과 원래 도구 결과를 보존합니다. iframe 안의 JavaScript 메모리나 DOM 변경을 직렬화하지 않습니다. 앱을 다시 열거나 새로고침하면 저장된 원래 입력/결과로 새 iframe을 시작합니다. 승인된 호출을 자동 재실행하지 않습니다.
- HTML/인수/결과는 저장 전 비밀값을 가립니다. 공개 스냅샷에서도 모든 추가 문자열을 가리며 JSON 인수의 이스케이프된 키/값도 처리합니다. 비밀값이 HTML에 포함되어 있었다면 가림 때문에 그 앱이 정상 동작하지 않을 수 있습니다. 앱에 자격 증명을 넣지 마세요.

## 격리와 외부 리소스 정책

기존 시각화의 `sandbox="allow-scripts"`, 불투명 출처, 첫 요소 CSP, Permissions Policy, 탐색 차단과 전체 화면 컨테이너를 재사용합니다. 원격 스크립트·스타일·이미지·폰트, fetch/XHR/WebSocket, 팝업과 호스트 DOM/저장소 접근은 허용하지 않습니다. 리소스가 선언한 외부 도메인이나 기기 권한도 자동 허용하지 않습니다. 외부 라이브러리가 필요한 앱은 필요한 코드를 리소스에 포함해야 합니다.

기존 시각화의 브라우저 수준 한계도 그대로 적용됩니다. 거부된 iframe 탐색 전에 Chromium이 DNS 조회/연결을 시도할 수 있고, 자식 about:blank 프레임을 통한 WebRTC 차단은 완전한 OS 네트워크 격리가 아닙니다. 같은 렌더러의 무한 루프는 다른 iframe도 멈출 수 있습니다. 이 기능이 기존 iframe을 OS 수준 샌드박스로 바꾸지는 않습니다.

## 앱 요청과 승인

iframe은 JSON-RPC 2.0 `postMessage`로 `ui/initialize` / `ui/notifications/initialized`를 교환합니다. 호스트는 그 iframe의 정확한 `event.source`와 불투명 `event.origin`을 검사합니다. 초기화가 끝나면 저장된 `ui/notifications/tool-input` 및 `ui/notifications/tool-result`를 보냅니다. `ping`과 승인된 `tools/call`을 지원하며, 그 밖의 요청은 명시적인 미지원 오류를 반환합니다. 호스트는 외부 링크 열기나 임의 리소스 읽기를 광고하지 않습니다.

1. 앱이 도구 이름과 JSON 인수를 요청합니다. iframe의 동일성은 `event.source`로 확인하며, iframe JSON-RPC 요청 ID는 한 연결에서 중복 사용할 수 없습니다. 서버의 승인 ID는 이와 별개인 일회용 ID입니다. 한 연결에서 128개 요청 ID를 사용하면 **코드 보기 → 미리보기**로 다시 연결해야 합니다. 크기 초과, 다른 프레임/호스트 출처 요청은 실행하지 않습니다.
2. 서버가 앱과 대화에 묶인 만료되는 일회용 요청 ID를 만듭니다. 이 단계는 도구를 호출하지 않습니다.
3. **iframe 바깥의 호스트 승인 카드**에 서버·도구·인수가 표시됩니다. 사용자가 **한 번 실행** 또는 **거절**을 선택합니다. 기존 `mcp_tool` 자동 승인 규칙은 적용하지 않습니다.
4. 승인 시에만 실행 슬롯을 직렬화하고 새 임시 ACP(Agent Client Protocol, 앱과 엔진 사이의 내부 통신) 세션을 만듭니다. 세션에는 원래 서버 하나만 포함하며 모델 프롬프트, workspace 브로커 또는 다른 서버를 넣지 않습니다. 현재 엔진 카탈로그에 존재하고 활성화된 같은 서버 도구인지 확인합니다. `_meta.ui.visibility`를 생략하면 앱에도 공개된 것으로 보며, 명시했다면 `"app"`이 포함되어야 합니다. 이름은 서버 접두사 없이 영문·숫자·`_`·`-`·`.`로 128바이트까지이고 `__`는 허용하지 않습니다. 원래 앱 정의의 리소스 연결도 확인합니다. 카탈로그에 없는 앱 전용 도구는 추측해 실행하지 않습니다. `localOnly` 도구 활동에는 앱을 붙이거나 새 결과 경로를 열지 않습니다.
5. 서버/봇/앱 존재 여부, 서버 ID·버전·사용 여부, 제안 시점의 해석된 설정을 실행 전에 다시 확인합니다. 설정·비밀 교체 후 오래된 승인은 실패합니다. 서버의 `updatedAt` 버전은 같은 밀리초나 뒤로 가는 시계에서도 실제 변경마다 증가하므로 껐다 다시 켜도 이전 승인이 되살아나지 않습니다. 결과를 가리고 제한된 형식으로 원래 프레임 요청에 돌려주며 임시 세션을 닫습니다.

승인 대기는 모델 `Conversation.run`에 넣지 않고 명령 대기열도 잡지 않습니다. 실제 실행만 기존 모델/루틴 실행과 직렬화합니다. 업데이트 유지보수 중에는 새 앱 요청과 늦게 도착한 승인을 실행하지 않으며, 앱 실행 중이거나 기존 모델 실행의 마지막 저장이 정리되는 동안 유지보수/다른 실행과 겹치지 않게 합니다. 대화의 캔버스와 앱 기록은 함께 저장·검증되고, 앱 호출은 캔버스 문서나 모델 턴 상태를 바꾸지 않습니다. 요청 ID는 한 번만 소비하며 거절·오류 후 재사용할 수 없습니다. 120초 뒤 만료되고, 새로고침·재시작은 보류된 호출을 재생하거나 승인하지 않습니다. 실행을 이미 승인한 뒤 화면을 닫는 것은 외부 부작용을 되돌리는 취소가 아닙니다.

## 예제와 검증

[실행 가능한 카운터 예제](<../examples/mcp-counter.ts>)와 이를 재사용하는 [테스트 fixture](<../tests/mcp-app-fixture.ts>)는 실제 HTTP `tools/list`, `tools/call`, `resources/read` 및 iframe RPC를 사용합니다. 자동화 테스트는 자체 로컬 모의 모델 공급자와 임시 Rust 앱 서버도 시작하므로 API 키나 외부 공급자 없이 재현합니다. 반면 예제 서버만 직접 실행하는 대화식 경로는 이미 실행 중인 Rust 앱과 구성된 모델 공급자가 필요합니다(로컬 공급자도 사용 가능).

```sh
# 예제 서버의 MCP JSON을 복사해 봇에 추가하고 켠 뒤 카운터 앱을 열도록 요청합니다.
npx tsx examples/mcp-counter.ts
```

키 없는 자동 재현에는 Node.js 22.12+, Rust 빌드 도구, ripgrep과 시스템 Google Chrome이 필요합니다. 아래 순서로 의존성·웹·Rust 서버를 빌드한 뒤 테스트를 실행합니다. 테스트가 띄운 임시 서버와 모의 공급자는 테스트 종료 시 정리됩니다.

```sh
npm ci
npm run build
npm run build:server
GROK_BOT_BACKEND=rust npx playwright test tests/mcp-apps.spec.ts tests/visualizations.spec.ts
npx tsx --test tests/mcp-app-protocol.test.ts tests/mcp-app-store.test.ts
node scripts/cargo.mjs test --locked -p grok-bot-model -p grok-bot-store -p grok-bot-core
```

[PR #31의 검증 기록](<https://github.com/elicie/bot/pull/31>)에 실제 실행 결과를 남겼습니다. 캔버스·대화 라우팅·현재 CI 변경을 통합한 구현 리비전 `b0d06f78bb7e33a0fc244f2cff5027d53b40fa0c` 기준으로 Rust 모델/저장소/코어 테스트 249개, 전체 Node 테스트 233개, 브라우저 인수/회귀 테스트 40개와 엄격한 Clippy·포맷·웹/서버 빌드가 통과했습니다. 시나리오는 실제 MCP 리소스 인식·표시, 매번 승인/거절, 승인 전 무호출, 결과 상관관계, 자동 승인 규칙 무시, 새로고침·디스크 저장, 전체 화면, 데스크톱/모바일, 출처·ID 재사용·늦은 응답·변경된 서버·크기/MIME/URI 거절을 포함합니다. 브라우저의 접근성 의미/키보드는 확인했지만 실제 보조기기 음성 출력은 검증하지 않았습니다.

## 시각 비교

동일한 자체 포함 카운터 시나리오, dark 테마, 1280×900 / 390×844를 캡처했습니다. **Before**는 앱을 표시하지 않던 기준 리비전 `c07a1cfec41e84ab86e566e7c72e3684dae1c31c`의 실제 서버/웹 화면입니다. **After**는 [구현 리비전 `b0d06f7`](<https://github.com/elicie/bot/commit/b0d06f78bb7e33a0fc244f2cff5027d53b40fa0c>)의 변경된 실제 앱 화면입니다. 추가 승인 화면은 비교의 After를 대신하지 않습니다.

| 뷰포트 | Before | After |
| --- | --- | --- |
| 데스크톱 | ![Before: 도구 결과만 표시](<evidence/mcp-apps/before-1280.png>) | ![After: 대화 안의 MCP 앱](<evidence/mcp-apps/after-1280.png>) |
| 모바일 | ![Before: 모바일 도구 결과](<evidence/mcp-apps/before-390.png>) | ![After: 모바일 MCP 앱](<evidence/mcp-apps/after-390.png>) |

프로토콜 근거: [공식 MCP Apps 명세 2026-01-26](<https://github.com/modelcontextprotocol/ext-apps/blob/cf599cc462406accc90ec3ca7879e3a22a9fc2a8/specification/2026-01-26/apps.mdx>). 명세는 참고 계약이며 이 구현의 추가적인 격리 제한 및 위 지원 범위를 대체하지 않습니다.
