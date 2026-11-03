# 최신 main 통합 뒤 실제 웹 / Tauri 비교

## 리비전과 같은 조건

- **Before:** 변경하지 않은 main `3ff99e8ddad7f3278c519c8d600c39d3f3496b81`의 별도 snapshot에서 빌드한 실제 Rust 웹 앱 + 시스템 Chrome.
- **After:** main을 반영한 desktop 구현 `ea4f8863796ccc4f67c690654cb7a43ef0ebbb04`의 실제 Tauri 앱 + WebKitGTK. 이 기록 이후 문서/이미지 추가는 실행 코드를 바꾸지 않습니다.
- **1040×760, 다크 테마, 같은 호스트/OS 글꼴, 같은 Bot ID와 완료된 답변·제목·설명·썸네일**입니다. 동일한 stopped 웹 state와 generated-images 사본을 `--import-web`으로 가져왔습니다. 원본 state SHA-256은 [baseline.json](<baseline.json>), native에서 확인한 같은 해시는 [native-verification.json](<native-verification.json>)에 있습니다.
- 무료 로컬 모의 공급자와 정확한 loopback HTML/PNG fixture만 사용했습니다. 서버의 기존 **debug + NODE_ENV=test 전용** 정확한 fixture-origin 허용 경로를 사용했으며, production/release의 SSRF 검증을 완화하지 않았습니다. 실제 계정/키/Feishu 자료를 포함하지 않습니다.
- Before는 Playwright의 실제 웹 캡처, After는 tauri-driver/WebKitWebDriver의 **실제 Tauri 창** 캡처입니다. 불필요한 포커스 강조를 제거해 같은 UI 상태로 맞췄습니다. Chrome/WebKit의 실제 글꼴·렌더링 차이를 보정하거나 목업으로 바꾸지 않았습니다.

## Before / After — 활성화된 링크 카드와 이미지

웹은 기존 `/api/generated-images/<uuid>` 직접 요소 URL을 유지합니다. Tauri는 같은 API의 인증된 바이트를 blob URL로 표시합니다. 토큰을 URL에 붙이거나 이미지 API를 인증에서 제외하지 않습니다.

**Before — main 웹**

![Before: main 웹의 실제 썸네일](<before-preview.png>)

**After — 실제 Tauri**

![After: Tauri의 인증된 실제 썸네일](<after-preview.png>)

## Before / After — 일반 설정의 opt-in과 개인정보 안내

같은 활성화 상태, 같은 General 탭과 스크롤 위치입니다. 기본 꺼짐/서버 접속 안내와 토글 동작을 유지합니다.

**Before — main 웹**

![Before: main 일반 설정](<before-general.png>)

**After — 실제 Tauri**

![After: Tauri 일반 설정](<after-general.png>)

## Before / After — 업데이트 안내

같은 저장된 답변을 배경으로 Updates 탭을 맨 위에서 엽니다. 웹의 소스 빌드 안내는 유지하고 Tauri에서는 수동 서명 설치 파일 교체를 안내합니다.

**Before — main 웹**

![Before: main 웹 업데이트](<before-updates.png>)

**After — 실제 Tauri**

![After: Tauri 수동 업데이트](<after-updates.png>)

## 새 소비자 회귀 검증

실제 native 카드에서 다음을 확인했습니다([JSON 기록](<native-verification.json>)).

- 저장된 답변을 가져온 뒤 thumbnail이 `blob:<desktop-origin>/…`이며 실제 PNG 디코딩 완료/자연 너비가 0보다 큼.
- 카드 소비자의 실제 이미지 fetch에 실행 토큰 헤더가 있고, raw 인증 없는 API img 요청에 의존하지 않음.
- 끄면 카드가 제거되고 실제 소비자의 object URL이 revoke됨. pending 썸네일 상태에서 끄면 해당 AbortSignal이 취소됨.
- opt-out이 실제 native reload 후에도 저장되어 카드가 다시 나타나지 않음.
- 카드의 정상 HTTP 링크를 클릭하면 자격 정보 없는 URL과 원래 HOME이 검증용 OS-opener stub으로 전달되고 native 팝업은 추가되지 않음. 개인 브라우저를 실제로 띄운 결과로 부르지 않음.
- 가져오기 원본 state가 검증 전후 바이트 단위로 같음.

기존 [P5 기능/수명 주기 기록](<../README.md>)은 별도 보존합니다. 이번 검사는 최신 main의 카드 소비자 통합을 검증하며, 과거 Chrome 캡처를 native 증거로 재분류하지 않습니다. 서명 키/인증서가 없어 서명된 DMG/AppImage 검증은 미완료입니다. Linux 실제 창 결과를 macOS 실제 창 실행으로 주장하지 않습니다.
