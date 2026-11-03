# P5 실제 웹 → Tauri 비교와 검증

최신 main 통합 뒤 링크 카드·설정·업데이트의 현재 비교는 [main-refresh](<main-refresh/README.md>)에 있습니다. 아래는 최초 P4 기반 검증 이력이며 새 캡처로 바꾸어 부르지 않습니다.

## 비교 조건과 출처

- **Before baseline:** P4 병합 `e7de2f2d4f10539e0e25ee39752dd47e205c06ba`의 별도 소스 snapshot에서 빌드한 실제 Rust 서버 + 기존 React bundle, 시스템 Chrome.
- **After implementation:** native navigation/link 보완이 포함된 `daefd99fea71ac564358502a73ea20b593db794f`의 실제 Tauri 바이너리(리뷰어 credit로 메시지만 다시 쓴 동등 tree commit은 `c12aec8e47ab5ff6df4f4b703ba91f7e3cda7cdd`), Linux WebKitGTK. 이후 테스트 전용 FIFO fixture·의존성 feature·쓰기 전 import 경로 preflight 수정은 이 비교 시나리오의 시각 동작을 바꾸지 않습니다.
- **같은 데이터/상태:** baseline 서버에서 모의 공급자와 완료한 한국어 1턴을 정상 저장한 뒤, 같은 `state.json` 사본을 Tauri `--import-web`으로 가져왔습니다. 새 랜덤 Bot ID로 아바타가 달라지는 비교를 피했습니다. Bot/대화/아바타/본문이 같습니다. 원본 사본 SHA-256은 [baseline.json](<baseline.json>)에 기록했습니다. 실제 API 키/개인 데이터가 아닌 일회용 fixture입니다.
- **같은 시나리오:** 저장된 대화 화면, 이어서 설정 → 업데이트. 1040×760 콘텐츠 viewport, 다크 테마, 동일 호스트의 OS 글꼴. Chrome과 WebKitGTK의 렌더링 차이는 숨기지 않습니다.
- Before는 Playwright의 실제 웹 페이지 캡처, After는 tauri-driver → WebKitWebDriver가 실제 Tauri WebKit 창에서 얻은 screenshot입니다. After를 일반 Chromium 브라우저나 목업으로 대체하지 않았습니다.

## Before / After — 저장된 대화

**Before (P4 웹)**

![Before — P4 웹의 동일 저장 대화](<before-initial.png>)

**After (Tauri)**

![After — 실제 Tauri로 가져온 동일 대화](<after-initial.png>)

## Before / After — 설정 → 업데이트

같은 저장 대화를 배경으로 설정의 업데이트 탭을 엽니다. 웹의 소스 빌드 안내는 유지되고 데스크톱에서는 수동 서명 설치 파일 교체를 안내합니다.

**Before (P4 웹)**

![Before — 웹의 업데이트 안내](<before-updates.png>)

**After (Tauri)**

![After — 데스크톱의 수동 업데이트 안내](<after-updates.png>)

## 기능 검사와 제한

현재 확인 결과는 [native-verification.json](<native-verification.json>)의 실제 통과 항목을 따릅니다. 실제 창에서 웹 상태 가져오기, 대화, 파일 Allow/Deny, 재로딩, Bot 승인 전달·수신 응답·발신 후속 턴, 1회 예약 루틴과 결과 대화, sandbox 시각화, 인증된 이미지·텍스트 첨부 다운로드를 확인했습니다. 원본 웹 상태는 가져오기 전후 바이트 일치를 확인했습니다. 진행 중이거나 실패한 시도는 통과 항목에 넣지 않습니다. 일반 Markdown 링크의 OS 브라우저 전달은 사용자의 브라우저를 방해하지 않도록 검증용 opener stub으로 URL과 원래 HOME만 확인했습니다. 실코어와 무료 로컬 모의 모델을 사용했고 유료 모델/API나 개인 계정은 사용하지 않았습니다. 파일 승인은 UI 선택 전후의 실제 임시 작업 파일 바이트를 비교했습니다.

별도 native boundary probe는 기존 `sandbox="allow-scripts"` srcdoc 프레임의 준비 메시지와 같은-origin blob 다운로드의 정확한 바이트 저장을 확인했습니다. iframe을 같은 HTTP origin만 허용하던 초기 구현에서 준비 메시지가 오지 않는 회귀를 재현했고, 필요한 `about:srcdoc`/`about:blank`만 허용한 뒤 같은 probe가 통과했습니다. 외부 native 창·IPC·iframe token 주입을 열지 않았습니다. 다운로드 fixture는 원래 OS Downloads에 저장된 내용을 확인한 뒤 그 파일만 제거했습니다.

추가 [수명 주기 기록](<lifecycle-verification.json>)은 실제 창의 정상 종료 뒤 loopback 리스너 종료, 저장된 Bot ID 유지와 새 토큰 발급/이전 토큰 거절, 기존 시스템 Chrome의 보호된 시작·CDP PNG를 인증 blob으로 표시·중지를 확인했습니다. [관리형 Chrome 실제 창](<after-managed-chrome.png>)도 별도로 캡처했습니다.

[가져오기 사전 검사](<import-preflight-verification.json>)는 실제 CLI에서 같은 원본·중첩 경로·원본/목적지 alias를 startup 전에 거절하고 원본 파일 목록과 권한·엔진/프로필/잠금 파일 부재를 확인했습니다. 기존 디렉터리의 device/inode도 비교해 macOS의 대소문자·Unicode alias가 문자열 비교를 우회하지 않게 합니다. macOS 조건부 회귀 테스트는 CI에서 실행하며 Linux 결과를 macOS 실제 실행으로 부르지 않습니다.

macOS 실제 창 검증, 배포 서명된 DMG/AppImage 검증은 아직 없으며 Linux 캡처로 대체하지 않습니다. 네이티브 CI 결과와 정확한 최종 리뷰 head는 PR에 따로 기록합니다. 이 비교가 있다는 이유만으로 P5의 모든 배포 완료 조건이 충족된 것은 아닙니다.
