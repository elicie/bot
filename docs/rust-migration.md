# Rust 백엔드 전환 설계안

작성일: 2026-11-01 · 상태: P0–P3 완료, P4 대기

## 목표

Node/TS 백엔드(`server/`)를 Rust 단일 서버로 교체합니다. Grok Build 에이전트를 별도 프로세스가 아니라 같은 프로세스 안에서 실행합니다. 프론트엔드(`web/`, React)와 HTTP 계약은 그대로 둡니다.

해결할 문제:

- 메시지마다 엔진 프로세스를 새로 띄우는 비용 (debug 빌드 약 496MB)
- Node↔Rust ACP, Rust→Node MCP HTTP의 이중 직렬화 경계
- 전역 단일 실행 슬롯 (봇 하나가 실행 중이면 다른 봇과 대화 불가)
- 전역 단일 모델 설정
- 추론 출력(`agent_thought_chunk`)이 화면에 표시되지 않음, 30초 무응답 종료

## 확인한 upstream 사실 (grok-build `2bdd1d6a`)

| 항목 | 결과 | 근거 |
| --- | --- | --- |
| 에이전트 생성 | `MvpAgent::new(gateway, &cfg, auth, models, boot)`는 `pub`이며 `Result`를 반환합니다 | `xai-grok-shell/src/agent/mvp_agent/agent_ops.rs:2279` |
| 프로세스 내 ACP | `run_stdio_agent`는 stdio를 메모리 파이프로 옮긴 뒤 private 함수 `spawn_agent_local`에 넘깁니다. 이 함수는 공개 API만으로 다시 구성할 수 있습니다 (`AgentSideConnection::new` + `GatewayReceiver`) | `agent/app.rs:120-156`, `220-313` |
| 레퍼런스 구현 | upstream 테스트 키트가 `tokio::io::duplex`로 `MvpAgent`와 `ClientSideConnection`을 같은 프로세스에서 연결합니다. 이 방식을 그대로 따릅니다 | `agent/testkit/e2e.rs` (`test-support` feature, 우리는 코드만 참고) |
| 부트스트랩 | `init::resolve_boot_startup_settings`는 `pub`입니다 | `agent/init.rs:210` |
| 스레드 모델 | `MvpAgent`는 `Rc` 기반이라 `!Send`입니다. `LocalSet` 안에서만 실행할 수 있습니다 | `agent/app.rs:144`, testkit 주석 |
| 다중 세션 | 에이전트 하나가 여러 세션을 동시에 관리합니다. 세션별 모델 전환(`set_session_model`)을 지원하며 세션 간 오염 방지 테스트가 있습니다 | `acp_agent.rs:1966`, `mvp_agent/tests.rs:1320` |
| 커스텀 도구 | 외부 크레이트가 네이티브 도구를 주입하는 공개 API는 **없습니다**. MCP 도구는 항상 `search_tool`/`use_tool`을 통해서만 노출됩니다 | `xai-grok-agent/src/builder.rs`, `xai-grok-tools/src/implementations/use_tool` |
| 전역 상태 | `GROK_HOME`(`xai_dirs::grok_home()`), `GROK_*` 환경 변수, fs watcher, OTel은 프로세스 전역입니다 | `xai-dirs/src/lib.rs:81` |

## 목표 구조

```
브라우저 (React, web/ 그대로)
   │ HTTP / SSE  (기존 계약 유지)
┌────────────── grok-bot (Rust 단일 바이너리) ──────────────┐
│ axum HTTP 서버 (multi-thread tokio)                        │
│  ├─ store: 상태 파일, 리비전, SSE 브로드캐스트              │
│  ├─ routines: 스케줄러, FIFO 디스패처                       │
│  ├─ files: 작업 폴더 접근 (openat/O_NOFOLLOW)               │
│  ├─ broker: /internal/mcp/:run (loopback, Bearer)          │
│  └─ computer: 관리형 Chrome (CDP)                           │
│        ▲ mpsc 명령 / 이벤트                                  │
│ engine 스레드 (current_thread runtime + LocalSet)           │
│  ├─ MvpAgent  ◀── tokio::io::duplex ──▶  ClientSideConnection │
│  └─ AppClient (acp::Client 구현: 업데이트 반영, 권한 요청)   │
└──────────────────────────────────────────────────────────┘
   │ Chat Completions
OpenAI 호환 서버
```

핵심 결정:

1. **ACP는 프로세스 안에서 그대로 유지합니다.** upstream이 외부에 약속한 안정적인 인터페이스가 ACP이기 때문입니다. 세션 actor 같은 내부 API를 직접 호출하면 upstream 버전을 올릴 때마다 깨질 수 있습니다. 파이프만 stdio에서 `tokio::io::duplex`로 바꿉니다.
2. **엔진은 전용 OS 스레드에서 실행합니다.** `!Send` 제약이 있으므로 그 스레드 안에 `LocalSet`을 둡니다. axum 쪽과는 `mpsc` 명령 채널과 이벤트 채널로 통신합니다.
3. **에이전트 하나가 여러 세션을 동시에 처리합니다.** 대화마다 ACP 세션 하나를 씁니다. 전역 단일 슬롯은 설정 가능한 동시 실행 수로 바꿉니다. 루틴과 봇 메시지는 기존 FIFO 의미를 유지합니다.
4. **봇별 모델을 지원합니다.** `config.toml`에 `[model.<id>]`를 여러 개 생성하고, 세션 시작 시 `set_session_model`로 선택합니다.
5. **도구는 MCP 브로커로 유지합니다.** 같은 프로세스의 axum 라우트로 바뀔 뿐입니다. 공개 API가 없으므로 `search_tool`/`use_tool` 간접 호출은 **남습니다**. 대신 rules에 정확한 도구 이름과 호출 예시를 넣어 작은 모델의 실패를 줄입니다.
6. **엔진 설정 범위가 프로세스 단위로 바뀝니다.** 기존에는 대화마다 HOME을 분리했습니다. 이제는 앱 전용 `GROK_HOME` 하나를 씁니다. 공급자나 모델 설정이 바뀌면 엔진 스레드를 정리하고 다시 생성합니다. 이때 실행 중인 작업이 있으면 409를 반환합니다.

## 유지하는 것

- `web/` 전체와 `shared/types.ts`의 공개 타입. HTTP 계약(`ARCHITECTURE.md`의 표)도 그대로 둡니다.
- 상태 파일 형식과 위치(`~/.local/share/grok-bot-q`). Rust 서버가 기존 파일을 읽습니다.
- **Playwright e2e(`tests/*.spec.ts`)를 그대로 인수 테스트로 씁니다.** 블랙박스 HTTP 테스트라서 서버 구현과 무관합니다.
- 승인 정책은 모두 유지합니다. 쓰기 1회 승인, 봇 메시지 승인, 컴퓨터 loopback 전용 승인, Host/Origin 검증, API 키 마스킹이 해당됩니다.

## 잃거나 바뀌는 것 (트레이드오프)

- **강제 종료 수단이 없어집니다.** 지금은 엔진이 멈추면 SIGKILL로 끝낼 수 있습니다. 프로세스 안에서는 `session/cancel`만 가능합니다. 대응책은 다음과 같습니다.
  - 취소 기한을 넘기면 해당 세션을 버리고 대화를 `interrupted`로 표시합니다.
  - 엔진 스레드 자체가 응답하지 않으면 스레드를 새로 생성합니다.
  - 그래도 위험하면 엔진을 상주형 자식 프로세스 하나로 두는 대안(B안)을 씁니다.
- **엔진 panic이 서버까지 내릴 수 있습니다.** 엔진 스레드를 `catch_unwind`로 감싸고, panic이 나면 엔진만 재생성합니다. upstream이 `process::exit`를 호출하는 경로는 `MvpAgent::new`를 직접 호출해 피합니다.
- 기존 대화별 엔진 세션 디렉터리는 재사용하지 않습니다. 기존 64KiB 이력 재전달 경로로 이어갑니다.
- TS 백엔드 단위 테스트(`tests/*.test.ts`, 약 200KB)는 Rust 테스트로 이식해야 합니다.

## 데스크톱 (Tauri)

웹 서버와 Tauri 데스크톱 앱이 같은 코어 라이브러리를 씁니다.

- 코어(`crates/core`)는 axum 라우터와 엔진 호스트를 라이브러리로 제공합니다. 라우터를 만들고 수명 주기를 관리하는 API를 공개합니다.
- `crates/server`는 웹 버전입니다. 지정한 주소에 바인딩하고 `dist/`를 서빙합니다. LAN 접근은 기존 정책을 따릅니다.
- `crates/desktop`은 Tauri 버전입니다. 같은 코어를 `127.0.0.1:임의 포트`에 띄우고, webview가 그 주소를 엽니다. 프론트와 HTTP/SSE 계약은 바꾸지 않습니다.
- 데스크톱의 loopback 서버는 실행마다 발급하는 토큰 헤더와 Origin 검사로 보호합니다. 같은 컴퓨터의 다른 프로세스나 브라우저 탭이 접근하는 것을 막기 위해서입니다.
- 앱 데이터 경로는 Tauri의 앱 데이터 디렉터리를 씁니다. 엔진 `GROK_HOME`은 그 아래에 둡니다.
- 엔진 스레드는 Tauri의 tokio 런타임과 분리된 전용 스레드라서 충돌하지 않습니다.
- 데스크톱 버전의 관리형 Chrome은 시스템 Chrome을 CDP로 제어합니다. Tauri webview는 사용하지 않습니다.

## 크레이트 구성

```
Cargo.toml                (workspace; 기존 engine/ 대체)
crates/
  core/       라이브러리: axum 라우터 구성, 엔진 호스트, 상태 수명 주기
  server/     bin: 웹 버전 (바인딩, dist/ 서빙)
  desktop/    bin: Tauri 앱 (core를 loopback에 띄우고 webview 연결)
  store/      상태 모델(serde), 원자적 저장, 리비전, 용량 제한
  engine/     엔진 스레드, ACP 클라이언트, 세션/실행 수명 주기, 프롬프트 리터럴화
  broker/     MCP JSON-RPC 서버, 도구 스키마, 승인 대기열
  files/      작업 폴더 confinement
  routines/   스케줄(once/interval/daily+IANA), 클레임 복구
  computer/   관리형 Chrome (CDP 파이프 클라이언트, loopback 프록시, 출처 정책)
```

`async-openai` 패치와 `Cargo.lock` 고정 방식은 기존 `engine/Cargo.toml`을 그대로 따릅니다.

## 단계

| 단계 | 내용 | 통과 기준 |
| --- | --- | --- |
| P0 스파이크 | 엔진 스레드, duplex ACP, `AppClient`를 만들고 모의 공급자로 1턴을 실행 | 프롬프트 응답, 세션 load 이어가기, cancel, 권한 요청 왕복, 동시 2세션, 세션별 모델 선택이 Rust 테스트로 통과 |
| P1 코어 서버 | axum, store, SSE, settings/bots/conversations, files, MCP 브로커 | `browser.spec.ts`, `conversation-history.spec.ts`, `bot-creation.spec.ts` 통과 |
| P2 기능 | 루틴, 봇 메시지, 스킬 | `routines*.spec.ts`, `skills.spec.ts` 통과, 해당 단위 테스트 이식 |
| P3 컴퓨터 | 관리형 Chrome, 프록시, 출처 허용 목록 | `computer.spec.ts` 통과, privacy 테스트 이식 |
| P4 정리 | `server/` TS 삭제, release 빌드, 문서 갱신 | 전체 e2e 통과, 바이너리 하나로 실행 |
| P5 데스크톱 | `crates/desktop` Tauri 앱, loopback 토큰, 앱 데이터 경로 | Tauri 창에서 대화·파일 승인 동작, 웹 버전과 같은 e2e 시나리오 수동 확인 |

P0이 실패하면, 즉 `LocalSet` 공존이나 다중 세션이 실제로 동작하지 않으면 B안으로 전환합니다. B안은 axum 서버에 상주형 엔진 자식 프로세스 하나를 두고 stdio ACP를 쓰는 구조입니다. B안으로 가도 Rust 단일 언어와 프로세스 재사용 이점은 유지됩니다.

## P0 결과 (2026-11-01)

`engine/src/host.rs`(엔진 호스트)와 `engine/tests/inprocess.rs`(모의 OpenAI 호환 서버와 MCP 서버 포함)로 검증했습니다. 4회 연속 통과했습니다.

| 시나리오 | 결과 |
| --- | --- |
| 엔진 시작 (스레드 생성, 부트스트랩, ACP initialize) | 약 150ms, `loadSession` 지원 |
| 한 턴 응답 | 약 50ms (모의 공급자 기준) |
| MCP 도구 (`search_tool` → `use_tool` → HTTP MCP 서버) | 도구 실행 1회, ACP 권한 요청 1회 왕복 |
| 공급자에 노출되는 도구 | 메인 턴은 `search_tool`, `use_tool`만. 보조 요청(세션 제목)은 `session_title` |
| 동시 2세션, 세션별 모델 | 1.5초 지연 두 개가 1.53초에 함께 완료, 세션별로 `model-a`/`model-b` 사용 |
| 취소 | `session/cancel` 후 즉시 `Cancelled` 반환 |
| 엔진 재시작 후 세션 load | 같은 `GROK_HOME`에서 이전 턴 맥락 유지 |

API 키는 모델별 `api_key` 설정으로 넘기므로, 실행 중에 프로세스 환경 변수를 바꿀 필요가 없습니다.

## P1–P2 결과 (2026-11-01)

Rust 서버(`crates/server`, 실행 파일 `grok-bot`)가 기존 HTTP 계약, 상태 파일, 루틴, 봇 메시지, 스킬, 파일 승인을 그대로 제공합니다. 실행 중인 작업은 여전히 전역에서 하나로 제한합니다(동작 이식이 우선).

| 검증 | 결과 |
| --- | --- |
| `npm run test:rust` (workspace 전체) | 통과. 저장소 43, 파일 15, core 25 + HTTP API·런타임 통합, 엔진 P0 |
| `npm run test:e2e:rust` (Rust 서버 + Chrome) | 19개 통과 (당시 `computer.spec.ts` 제외, P3에서 포함) |
| 기존 Node 백엔드 `npm test` | 94개 통과 (P4에서 삭제 전까지 유지) |
| 기존 `~/.local/share/grok-bot-q/state.json` | Rust 서버에서 그대로 로드. Rust가 쓴 파일도 Node `Store.open`에서 로드 |

TS 대비 의도된 차이:

- 엔진 세션은 턴이 끝날 때 닫고 다음 턴에 디스크에서 다시 load합니다. 턴마다 MCP 브로커 주소와 토큰이 새로 발급되기 때문입니다.
- 취소 기한 안에 엔진이 멈추지 않으면 세션을 버리고 다음 턴은 새 세션으로 시작합니다. 강제 종료할 프로세스가 없기 때문입니다.
- 세션 fingerprint 태그를 `broker-v7-inprocess`로 바꿨습니다. 이전 엔진 세션은 load하지 않고 최근 대화 64KiB를 넘겨 이어갑니다.
- API 키를 엔진 설정 파일(`<data>/engine/home/.grok/config.toml`, 0600)에도 저장합니다.
- 공급자 오류 메시지에 엔진이 보고한 상세(상태 코드 등)가 함께 표시됩니다.
- 파일 목록 정렬은 ICU `localeCompare`를 근사합니다. ASCII와 일반 문자는 같고, 드문 기호나 로캘별 규칙은 다를 수 있습니다.
- 시간대는 chrono-tz(tz database)를 씁니다. ICU만 아는 일부 별칭(예: `US/Pacific-New`)은 거절합니다.

## P3 결과 (2026-11-01)

관리형 Chrome을 `crates/core/src/computer/`로 이식했습니다. `server/computer.ts`, `server/computer-network.ts`, `server/computer-api.ts`의 메시지, 상태 코드, 한도, 보안 속성을 그대로 따릅니다. 모델 도구 `computer_observe`/`computer_act`는 `runtime/broker/computer.rs`, HTTP 경로는 `computer_http.rs`에 있습니다.

- Chrome 제어: 외부 CDP 크레이트 없이 `--remote-debugging-pipe`(fd 3/4, NUL 구분 JSON)로 직접 통신합니다. 디버깅 포트를 열지 않으므로 같은 컴퓨터의 다른 프로세스가 붙을 수 없습니다(Playwright와 같은 전송 방식). 시스템 Google Chrome(`/opt/google/chrome/chrome`)을 실행마다 새 0700 프로필로 띄우고, Node와 같은 보호 플래그·명령줄 검증·`chrome://sandbox` seccomp 확인·프록시/DNS 탐침을 통과해야 시작합니다. 실패하면 대체 실행 없이 중지합니다.
- 프록시: `tokio` + `httparse`로 만든 인증 loopback 포워드 프록시입니다. 모든 DNS 응답이 공인 주소여야 하고, 검증한 주소에 직접 연결합니다(재해석 없음).
- 테스트 전용 주입: `GROK_BOT_COMPUTER_TEST_ORIGINS`(loopback fixture 출처)는 debug 빌드를 `NODE_ENV=test`로 실행할 때만 읽습니다. release 빌드에서는 켤 수 없습니다.

| 검증 | 결과 |
| --- | --- |
| `cargo test --workspace` | 통과. core 단위 43(관리형 컴퓨터 16 포함), `computer_http` 통합(loopback 전용, LAN 투영) |
| `npm run test:e2e:rust` | 42개 통과 (`computer.spec.ts` 3개 포함) |

TS 대비 의도된 차이:

- 페이지 관찰 스크립트(조작 요소 추출, 클릭/입력 대상 검사, 본문 읽기)는 페이지의 메인 world가 아니라 매번 새로 만든 격리 world에서 실행합니다. 페이지 스크립트가 DOM 함수를 바꿔 민감한 입력란을 숨길 수 없습니다.
- 새 페이지(팝업)는 실행되기 전에 닫고, 자식 프레임과 워커는 요청 가드(프레임은 초기화 스크립트도)를 건 뒤에만 실행합니다. 서비스 워커와 공유 워커는 일시 정지 상태로 둡니다. 파일 선택 대화상자는 가로채서 열지 않습니다.
- 렌더러 충돌도 페이지 닫힘과 같이 세션을 중지합니다.
- Chrome 프로필(겸 TMPDIR)은 `/tmp` 아래에 만듭니다. 엔진이 프로세스 TMPDIR을 데이터 폴더 깊이 바꿔 Chrome 싱글톤 소켓 경로가 Unix 소켓 길이 제한을 넘기 때문입니다.
- 프록시는 응답 본문을 원래 프레이밍(`Content-Length`/`Transfer-Encoding`) 그대로 중계하고, `Proxy-Connection` 헤더도 홉 헤더로 제거합니다.

## 결정이 필요한 것

1. 웹 버전에서 프론트 빌드 결과를 바이너리에 내장할지, `dist/` 폴더를 서빙할지 (현재 `dist/` 서빙, 데스크톱은 Tauri가 번들링)

결정됨: 기존 상태 파일 호환 유지, 관리형 Chrome은 P3에서 이식(완료).
