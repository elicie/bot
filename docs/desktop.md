# Tauri 데스크톱 (P5 — 검증 중)

> 아직 출시 완료가 아닙니다. 서명된 macOS/Linux 배포본과 실제 Tauri 기능 확인이 끝나기 전에는 P5 완료로 간주하지 않습니다.

## 실행과 경계

웹과 같은 `grok-bot-core`를 프로세스 안에서 사용합니다. 별도 Node 서버나 엔진 실행 파일은 포함하지 않습니다. 매번 `127.0.0.1:0`에 실제 리스너를 유지한 채 바인딩하고, 결정된 주소를 Tauri 창에서 엽니다. 기존 HTTP 경로, JSON, SSE `state` 이벤트는 그대로입니다.

```sh
npm ci
npm run dev:desktop
npm run build:desktop -- --bundles appimage  # Linux
npm run build:desktop -- --bundles dmg       # macOS
```

Linux에는 GTK3/WebKitGTK 4.1 개발 패키지, librsvg, patchelf가 필요합니다. macOS에는 Xcode command-line tools, Protocol Buffers compiler, ripgrep가 필요하며 `PROTOC`에 호스트 컴파일러 경로를 지정합니다. 엔진 빌드에 포함되는 ripgrep도 반드시 빌드 대상 OS의 실행 파일이어야 합니다.

공유 workspace의 기본 검사에서는 OS GUI 의존성을 요구하지 않는 경로·가져오기 라이브러리를 검사합니다. 실제 앱은 명시적인 `native` feature로 빌드하고 별도의 Desktop native builds workflow에서 macOS/Linux 양쪽을 확인합니다. 라이브러리 검사만으로 네이티브 앱이 검증됐다고 간주하지 않습니다.

## 로컬 API 보호

호스트가 실행마다 256비트 토큰을 생성하며 URL, HTML, 로그, 상태 파일에는 쓰지 않습니다. 정확히 해당 loopback origin의 최상위 창에만 시작 스크립트로 전달합니다. `/api` 전체의 읽기·쓰기·SSE·미디어가 `X-Grok-Desktop-Token`을 요구합니다. 웹 호스트 기본값은 바꾸지 않습니다. 엔진 `/internal/mcp`와 루틴 webhook은 기존 별도 Bearer 권한을 유지합니다.

데스크톱에서만 JSON 요청과 fetch 기반 SSE가 토큰 헤더를 사용합니다. 이미지·첨부 다운로드는 인증된 바이트를 blob URL로 표시하여 요소 요청에 토큰을 URL로 붙이거나 포트 범위가 없는 쿠키를 쓰지 않습니다. 외부 origin으로 토큰을 보내지 않으며 외부 Tauri 창 이동·팝업과 네이티브 IPC 권한을 열지 않습니다. 일반 HTTP(S) 링크는 자격 정보 없는 URL만 OS 기본 브라우저로 열고, 엔진의 HOME/설정·비밀값을 여는 프로세스에 넘기지 않습니다. 기존 sandbox 미리보기가 쓰는 `about:srcdoc`/`about:blank` 문서는 허용하지만 최상위 정확한 HTTP origin이 아니므로 토큰 bootstrap을 받지 않습니다. 이 보호는 인증 없는 로컬 HTTP 호출을 막기 위한 것이며, 같은 OS 사용자 권한으로 메모리를 읽는 악성 프로그램으로부터 비밀을 격리한다는 보장은 아닙니다.

## 데이터와 가져오기

기본 앱 루트는 Tauri의 앱 데이터 규칙을 따릅니다.

- Linux: `$XDG_DATA_HOME/com.grok-bot.desktop` (기본 `~/.local/share/com.grok-bot.desktop`)
- macOS: `~/Library/Application Support/com.grok-bot.desktop`
- `store/`: 기존 상태·첨부·지식·생성 이미지
- `engine/home/.grok`: 전용 `GROK_HOME`
- `webview/`: Linux UI webview 전용 프로필. macOS의 구버전 WKWebView는 파일 경로 override가 없어 비영구 저장소를 사용하며 브라우저 환경 설정은 종료 시 보존하지 않습니다.

원래 OS 경로를 먼저 확정하고 엔진의 전역 환경을 **다른 스레드/Tauri 생성 전** 준비합니다. 같은 데스크톱 데이터 루트를 동시에 열 수 없도록 잠금을 유지합니다. 관리형 Chrome은 기존 시스템 Chrome 및 비공개 CDP pipe를 그대로 사용하고 UI Webview를 조작하지 않습니다. Chrome의 샌드박스·네트워크 검사가 실패하면 안전하지 않은 대체 실행을 하지 않습니다.

기존 웹 서버를 먼저 정상 종료하고, **첫 실행 전에만** 가져옵니다.

```sh
./grok-bot-desktop --import-web /absolute/path/to/grok-bot-q
# 별도 검증 데이터 루트
./grok-bot-desktop --data-dir /absolute/path/to/desktop-test --import-web /absolute/path/to/web-backup
```

원본은 수정하지 않고 비공개 staging 복사본을 검사한 뒤 새 `store/`로 승격합니다. 앱 루트와 웹 원본이 같거나 중첩되면 엔진·프로필·잠금 파일 생성이나 권한 변경 전에 거절합니다. 이미 있는 목적지와 병합하거나 덮어쓰지 않습니다. 링크·특수 파일과 복사 한도를 벗어나는 입력은 거절합니다. 실행 중인 웹 디렉터리의 일관된 실시간 복사를 보장하지 않으므로 반드시 서버를 종료하거나 일관된 백업을 사용하세요. 이전 실행 세션을 가져오는 대신 세션 참조를 제거해 기존 대화 이력 재전달 경로로 이어갑니다. 작업 폴더는 외부 절대 경로이므로 다른 컴퓨터에서는 다시 선택해야 합니다. 브라우저의 테마·배치·사운드 localStorage는 서버 데이터가 아니므로 이 가져오기에 포함하지 않습니다.

## 패키징·서명·업데이트 결정

P5는 **수동 업데이트**입니다. 자동 업데이트 플러그인, 업데이트 서버, 백그라운드 다운로드는 설치하지 않습니다. 앱을 종료하고 검증한 새 서명 배포본으로 교체하며, 데이터 루트는 유지합니다.

Desktop native builds는 PR에서 명확히 `unsigned-test-only`로 표시한 AppImage/DMG 빌드 증거를 제공합니다. 배포 서명은 명시적인 workflow_dispatch의 `signed=true`에서만 실행하고 필요한 secret이 없으면 실패합니다. unsigned 빌드는 사용자 배포용 서명 완료를 의미하지 않습니다.

- macOS: 저장소 Secrets의 `APPLE_CERTIFICATE`(base64 PKCS#12), `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`. 임시 keychain에만 가져오고 종료 시 제거합니다. codesign 검증을 수행합니다. 공증은 별도 Apple 자격 증명/배포 검토가 필요하며, 현재 자동 공증을 주장하지 않습니다.
- Linux: 저장소 Secret `LINUX_SIGNING_KEY`로 AppImage의 `SHA256SUMS`를 GPG 분리 서명합니다. 공개 검증 키와 지문은 소유자가 신뢰할 경로로 배포해야 합니다. AppImage 내부 서명이나 updater 서명을 주장하지 않습니다. 사용자는 신뢰하는 공개 키로 `gpg --verify SHA256SUMS.asc SHA256SUMS`, 이어서 `sha256sum --check SHA256SUMS`로 확인합니다.

비밀 키/인증서는 댓글·소스·아티팩트에 올리지 마세요. 서명되지 않은 테스트 빌드를 출시 완료로 표시하지 않습니다.

## 실제 창 검증 기록

Before는 P4 병합 revision의 실제 웹 앱, After는 같은 시나리오/1040×760/다크 테마/임시 데이터의 실제 Tauri 앱으로 기록합니다. 브라우저 스크린샷을 Tauri 창 증거로 바꾸어 부르지 않습니다. 대화, 파일 승인/거부 및 디스크 내용, 루틴, 봇 간 전달, SSE 재연결, 미디어, 종료·재시작·가져오기를 확인한 결과를 후속 검증 기록에 명시합니다. 아직 캡처/플랫폼 결과가 없는 항목은 미검증입니다.
