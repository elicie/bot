# Bot 동시 실행 main 통합 뒤 실제 웹 / Tauri 비교

## 같은 리비전·데이터·화면 조건

- **Before:** 변경하지 않은 main `1283a7cb6acb2bdea9d4874ac0c4f9eb564c1ee5`의 별도 source snapshot에서 빌드한 실제 Rust 웹 앱 + 시스템 Chrome.
- **After:** 같은 main과 native lazy-image fix를 반영한 실행 구현 `5a5cb9c4830e92f1a21a7787fb01298c53d1f68d`의 실제 Tauri 앱 + WebKitGTK.
- **1040×760, 다크 테마, 같은 호스트 글꼴, 같은 Bot ID·완료된 답변·메타데이터·PNG**입니다. baseline 웹 상태/이미지 사본을 Tauri `--import-web`으로 가져왔습니다. 원본 SHA-256은 [baseline.json](<baseline.json>)과 [native-verification.json](<native-verification.json>)에서 같습니다.
- 무료 로컬 모의 공급자와 정확한 loopback fixture-origin만 사용했습니다. 이는 기존 debug+test 전용 fixture 경로이며 release SSRF 검증은 바꾸지 않았습니다. 실제 사용자 계정·키·Feishu 콘텐츠는 없습니다.
- Before는 실제 웹 Playwright capture, After는 tauri-driver/WebKitWebDriver의 실제 native 창 capture입니다. 목업이나 일반 Chrome 화면을 Tauri 이전/이후로 재분류하지 않았습니다. 불필요한 포커스 강조는 제거했고 실제 Chrome/WebKit 글꼴 차이는 그대로 둡니다.

## Before / After — 링크 카드

**Before — main 웹**

![Before: 최신 main 웹 카드](<before-preview.png>)

**After — 실제 Tauri**

![After: 실제 native 카드](<after-preview.png>)

웹의 직접 API 이미지 URL은 그대로이고 native는 token-authenticated bytes의 blob URL을 사용합니다. token을 URL에 넣거나 이미지 인증을 면제하지 않습니다.

## Before / After — 일반 설정과 privacy 안내

동일한 활성화 상태, General 탭, 같은 하단 스크롤입니다.

**Before — main 웹**

![Before: 최신 main 일반 설정](<before-general.png>)

**After — 실제 Tauri**

![After: 실제 native 일반 설정](<after-general.png>)

## Before / After — 업데이트

같은 저장 답변을 배경으로 Updates 탭 상단입니다. 웹의 source-build 안내는 유지하고 native는 수동 서명 배포본 교체를 안내합니다.

**Before — main 웹**

![Before: 최신 main 업데이트](<before-updates.png>)

**After — 실제 Tauri**

![After: 실제 native 업데이트](<after-updates.png>)

## 실제 검증

[통과 JSON](<native-verification.json>)은 현재 native 카드의 PNG 디코딩/blob src, 실제 fetch의 token header, 끄기 시 revoke, pending-media abort, reload 뒤 opt-out 지속, 카드 OS-opener stub 전달/native popup 부재, source state의 바이트 보존을 기록합니다. OS-opener는 개인 브라우저를 방해하지 않도록 stub으로 URL과 원래 HOME만 검증했습니다.

과거 이미지가 모두 먼저 buffer되는 회귀는 별도 [lazy-media 측정](<../lazy-media/README.md>)에서 같은 80-image fixture를 실제 native Before/After로 검증했습니다. 초기 buffered images가 80에서 11로 줄고 scroll 후 필요한 이미지를 가져오며 modal과 unmount cleanup이 유지됩니다. scroll-out eviction·LRU·virtualization을 추가하지 않았습니다.

[3ff main 비교](<../main-refresh/README.md>)와 최초 [P5 기능/수명 주기 기록](<../README.md>)은 리비전별 역사로 보존합니다. 이 기록이 서명 완료를 뜻하지 않습니다. 키/인증서가 없어 서명된 DMG/AppImage와 서명 검증은 미완료이며, Linux 실제 창 결과를 macOS 실제 창 실행으로 부르지 않습니다.
