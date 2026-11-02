# Rust 백엔드 전환과 배포

작성일: 2026-11-01 · 갱신: 2026-11-02 · 상태: P0–P4 로컬 검증 완료, PR의 CI·검토 후 병합

## 목표와 현재 범위

Rust 실행 파일 `grok-bot` 하나가 HTTP/SSE, 내장 프론트엔드, Grok Build 엔진을 제공합니다. React [web](<../web/>)와 공개 HTTP 계약, 기존 상태 파일을 유지합니다. Node/TS 백엔드와 별도 엔진 실행 파일은 사용하지 않습니다. Node.js 22.12 이상은 프론트엔드·빌드 스크립트·테스트용이며 release 실행에는 필요하지 않습니다.

당초 전환 설계에서는 메시지별 엔진 프로세스 비용(debug 빌드 약 496MB), Node↔Rust ACP 경계, 전역 실행 슬롯·모델 설정, 추론 출력과 무응답 종료 동작을 검토했습니다. 현재 구현은 프로세스 내 엔진으로 실행 경계를 줄이되 전역 단일 실행 슬롯과 단일 모델 설정은 유지합니다. P0의 동시 세션·세션별 모델 실험 결과는 앱에서 해당 기능을 제공한다는 뜻이 아닙니다. P4는 이 정책을 확장하지 않고 구 백엔드 제거, 단일 실행 파일 패키징, 기본 테스트 경로, 문서 정리에 집중합니다.

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

## 현재 웹 구조

```
브라우저 (React, web/ 그대로)
   │ HTTP / SSE  (기존 계약 유지)
┌────────────── grok-bot (Rust 단일 바이너리) ──────────────┐
│ 내장 프론트엔드 + axum HTTP 서버 (multi-thread tokio)       │
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
3. **엔진의 다중 세션 기능과 앱의 실행 정책을 구분합니다.** P0에서 다중 세션을 검증했지만 앱은 전역에서 한 작업씩 실행합니다. 대화별 ACP 세션과 루틴·봇 메시지의 FIFO 의미를 유지합니다.
4. **앱은 현재 단일 모델 설정을 유지합니다.** P0의 `set_session_model` 실험은 봇별 모델 기능을 제공하기 위한 가능성 검증이며 P4 구현 범위는 아닙니다.
5. **도구는 MCP 브로커로 유지합니다.** 같은 프로세스의 axum 라우트로 바뀔 뿐입니다. 공개 API가 없으므로 `search_tool`/`use_tool` 간접 호출은 **남습니다**. 대신 rules에 정확한 도구 이름과 호출 예시를 넣어 작은 모델의 실패를 줄입니다.
6. **엔진 설정 범위가 프로세스 단위로 바뀝니다.** 기존에는 대화마다 HOME을 분리했습니다. 이제는 앱 전용 `GROK_HOME` 하나를 씁니다. 공급자나 모델 설정이 바뀌면 엔진 스레드를 정리하고 다시 생성합니다. 이때 실행 중인 작업이 있으면 409를 반환합니다.

## 유지하는 것

- React [web](<../web/>)와 [shared/types.ts](<../shared/types.ts>)의 공개 타입, [HTTP 계약](<../ARCHITECTURE.md#http-contract>)을 유지합니다.
- 상태 파일 형식과 위치(`~/.local/share/grok-bot-q`). Rust 서버가 기존 파일을 읽습니다.
- [tests](<../tests/>)의 Playwright e2e를 인수 테스트로 유지합니다. 블랙박스 HTTP 테스트라서 서버 구현과 무관하며 Rust 서버를 기본 대상으로 사용합니다. 프론트엔드 단위 테스트도 유지합니다.
- 승인 정책은 유지합니다. 쓰기·봇 메시지의 기존 승인 및 권한 규칙, 컴퓨터 loopback 전용 승인, Host/Origin 검증, API 키 마스킹이 해당됩니다.

## 잃거나 바뀌는 것 (트레이드오프)

- **엔진만 SIGKILL로 강제 종료할 수 없습니다.** 프로세스 안에서는 ACP `session/cancel`을 사용합니다. 기한 안에 멈추지 않은 세션은 버리고 다음 턴에 새 세션으로 시작합니다. 응답하지 않는 OS 스레드를 안전하게 강제 종료하거나 모든 엔진 실패에서 완전히 복구한다고 보장하지 않습니다.
- **panic 전략은 unwind를 유지합니다.** `MvpAgent::new`를 직접 호출해 upstream의 CLI 종료 경로를 피합니다. `panic = "unwind"`는 기존 스레드·태스크 unwind를 허용하지만 완전한 panic 복구를 검증했다는 뜻은 아닙니다. `panic = "abort"`는 이 경계를 우회하므로 사용하지 않습니다.
- 이전 Node 백엔드의 대화별 엔진 세션 디렉터리는 재사용하지 않습니다. 기존 64 KiB 이력 재전달 경로로 이어갑니다.
- 구 Node 백엔드 전용 단위 테스트는 Rust workspace 테스트로 대체합니다. 프론트엔드 단위 테스트와 Playwright e2e는 삭제하지 않습니다.

## 데스크톱 (Tauri)

P5의 향후 계획입니다. 현재 웹 release 실행 파일과 별개로 Tauri 앱은 아직 구현·검증하지 않았습니다. 웹 서버와 데스크톱 앱이 같은 코어 라이브러리를 쓰도록 설계합니다.

- 코어(`crates/core`)는 axum 라우터와 엔진 호스트를 라이브러리로 제공합니다. 라우터를 만들고 수명 주기를 관리하는 API를 공개합니다.
- [crates/server](<../crates/server/>)는 웹 버전입니다. 지정한 주소에 바인딩하고 내장한 프론트엔드를 서빙합니다. LAN 접근은 기존 정책을 따릅니다.
- `crates/desktop`은 Tauri 버전입니다. 같은 코어를 `127.0.0.1:임의 포트`에 띄우고, webview가 그 주소를 엽니다. 프론트와 HTTP/SSE 계약은 바꾸지 않습니다.
- 데스크톱의 loopback 서버는 실행마다 발급하는 토큰 헤더와 Origin 검사로 보호합니다. 같은 컴퓨터의 다른 프로세스나 브라우저 탭이 접근하는 것을 막기 위해서입니다.
- 앱 데이터 경로는 Tauri의 앱 데이터 디렉터리를 씁니다. 엔진 `GROK_HOME`은 그 아래에 둡니다.
- 엔진 스레드는 Tauri의 tokio 런타임과 분리된 전용 스레드라서 충돌하지 않습니다.
- 데스크톱 버전의 관리형 Chrome은 시스템 Chrome을 CDP로 제어합니다. Tauri webview는 사용하지 않습니다.

## 크레이트 구성

```text
Cargo.toml / Cargo.lock   workspace 전체 의존성과 프로필 고정
engine/                  라이브러리: host.rs, 전용 스레드·LocalSet·duplex ACP
crates/
  model/                 serde 상태 모델
  store/                 원자적 저장, 리비전, 용량 제한
  files/                 작업 폴더 confinement
  core/                  axum HTTP/SSE, 실행 수명 주기, MCP 브로커,
                         루틴·FIFO 복구, 관리형 Chrome, 공급자 중계기
  server/                grok-bot bin: 바인딩, CLI, 프론트엔드 내장
```

[engine/Cargo.toml](<../engine/Cargo.toml>)은 독립 루트가 아니라 workspace 멤버입니다. 루트 [Cargo.toml](<../Cargo.toml>)에 `async-openai` 패치를 두며 루트 [Cargo.lock](<../Cargo.lock>) 하나로 엔진을 포함한 Rust 그래프를 고정합니다. 별도 엔진 바이너리·lockfile·설치 단계는 없습니다.

## 단계

| 단계 | 내용 | 통과 기준 |
| --- | --- | --- |
| P0 스파이크 | 엔진 스레드, duplex ACP, `AppClient`를 만들고 모의 공급자로 1턴을 실행 | 프롬프트 응답, 세션 load 이어가기, cancel, 권한 요청 왕복, 동시 2세션, 세션별 모델 선택이 Rust 테스트로 통과 |
| P1 코어 서버 | axum, store, SSE, settings/bots/conversations, files, MCP 브로커 | `browser.spec.ts`, `conversation-history.spec.ts`, `bot-creation.spec.ts` 통과 |
| P2 기능 | 루틴, 봇 메시지, 스킬 | `routines*.spec.ts`, `skills.spec.ts` 통과, 해당 단위 테스트 이식 |
| P3 컴퓨터 | 관리형 Chrome, 프록시, 출처 허용 목록 | `computer.spec.ts` 통과, privacy 테스트 이식 |
| P4 정리 | 구 Node 백엔드·전용 테스트·별도 엔진 패키징 제거, 내장 프론트엔드 release, 문서 갱신 | 로컬 전체 Rust·프론트엔드·e2e 및 독립 release 실행 통과. PR의 CI·검토 후 병합 |
| P5 데스크톱 | `crates/desktop` Tauri 앱, loopback 토큰, 앱 데이터 경로 | Tauri 창에서 대화·파일 승인 동작, 웹 버전과 같은 e2e 시나리오 수동 확인 |

초기 설계의 B안은 `LocalSet` 공존이나 다중 세션 실험이 실패할 경우 상주형 엔진 자식 프로세스를 두는 대안이었습니다. 아래 P0 기록에 따라 프로세스 내 라이브러리 호스트를 채택했으며, B안은 현재 실행 경로가 아닙니다.

## P4 빌드·배포 계약

```sh
npm ci
npm run dev            # 프론트엔드 빌드 + Rust debug 빌드·실행, HMR 없음
npm start              # Cargo 래퍼로 Rust debug 실행, 프론트엔드 빌드 안 함
npm run build:release
./target/release/grok-bot
```

- `npm run build`는 프론트엔드만 빌드합니다. `build:release`는 이 명령을 먼저 실행한 뒤 `node scripts/cargo.mjs build --locked --release -p grok-bot-server`를 실행합니다.
- [crates/server/build.rs](<../crates/server/build.rs>)가 [dist/](<../dist/>)를 컴파일 시 내장합니다. Release 빌드는 프론트엔드 빌드를 먼저 해야 합니다. 결과인 `target/release/grok-bot`은 어느 작업 디렉터리에서든 Node/npm, 외부 프론트엔드 폴더, standalone 엔진 없이 실행합니다.
- `GROK_BOT_WEB_ROOT`는 명시적인 외부 화면 override입니다. 지정한 폴더에는 `index.html`이 있어야 합니다. Debug 빌드는 내장 화면을 우선 사용하고, 없으면 현재 작업 디렉터리의 `dist/`를 사용합니다. 둘 다 없으면 API 전용 검사를 실행할 수 있습니다.
- Release 프로필은 `opt-level = 3`, `debug = 0`, `strip = "symbols"`, `panic = "unwind"`입니다. 별도 LTO 튜닝은 하지 않으며 엔진 스레드 unwind를 막는 `abort`는 사용하지 않습니다.
- 단일 실행 파일은 정적 링크나 운영체제 간 실행 호환성을 보장하지 않습니다. 호환되는 Linux 네이티브 라이브러리(libc, zlib 등)가 필요하며 관리형 탐색에는 시스템 Google Chrome과 동작하는 샌드박스가 필요합니다. Node.js 22.12+는 빌드·프론트엔드 테스트 도구입니다.
- `grok-bot --help`, `--version`, `--licenses`로 사용법·앱/소스 식별자·내장 원본 고지를 확인합니다. [engine/NOTICE](<../engine/NOTICE>)와 [engine/licenses](<../engine/licenses/>)를 보존하고 실행 파일을 재배포할 때 필요한 라이선스·고지를 제공합니다. 내장 고지는 최종 Rust/npm 의존성 전체의 고지나 MPL 소스 제공 의무까지 검증했다는 뜻이 아닙니다.

### P4 검증 기록

Linux x86_64, Node.js 24.18.0, Rust 1.98.0과 시스템 Chrome에서 검증합니다. 구현 리비전은 `030794a6c87811e11b2d8dfb223a07e5e209f3a4`이며 최종 head·CI는 [PR #28](https://github.com/elicie/bot/pull/28)에서 확인할 수 있습니다. `:rust` e2e 별칭이나 백엔드 opt-in 환경 변수는 사용하지 않습니다.

| 확인 | 명령 / 증거 | P4 결과 |
| --- | --- | --- |
| 프론트엔드 | 깨끗한 `npm ci`, `npm run typecheck`, `npm test`, `npm run build` | 통과 — 유지한 프론트 테스트 11개, skip 없음 |
| Rust workspace | `umask 0022; npm run test:rust`, fmt, clippy `-D warnings` | 전체 통과 — 상속된 0077 umask에서 드러난 기존 파일 권한 테스트 가정은 [검증 기록](<verification.md>)에 명시 |
| 기본 브라우저 경로 | `npm run build && npm run build:server && npm run test:e2e` | 통과 — 실제 Chrome 전체 57개, skip 없음; [동일 조건 Before/After](<evidence/issue-21/README.md>)도 확인 |
| Release 빌드 | `npm run build:release` | 통과 — 약 181 MiB, 최적화·심볼 제거된 Linux x86_64 ELF, 첫 컴파일 18분 20초 |
| 독립 실행 | `node scripts/verify-release.mjs target/release/grok-bot` | 통과 — 빈 작업 디렉터리, 실행 파일 없는 PATH, 외부 화면·엔진 없이 내장 자산/HTTP 가드/실제 엔진 응답/SIGTERM 확인 |
| CLI와 고지 | `./target/release/grok-bot --version`, `--help`, `--licenses` | 통과 — 앱·upstream 식별자, 사용법, 원본 고지 4개 원문 일치, 잘못된 인자 거절 |
| 외부 화면 override | `GROK_BOT_WEB_ROOT`의 정상 폴더와 `index.html` 없는 폴더 | 통과 — 명시적 override 우선, 잘못된 설정 거절 |

상세 기록과 이전 검증의 범위는 [verification.md](<verification.md>)를 따릅니다. 아래 P0–P3의 명령·수치는 당시 증거를 보존한 것이며 P4 통과를 뜻하지 않습니다.

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
- ~~API 키를 엔진 설정 파일(`<data>/engine/home/.grok/config.toml`, 0600)에도 저장합니다.~~ 2026-11-02부터 엔진은 loopback 공급자 중계기(`crates/core/src/provider_relay.rs`)와 통신하고, 설정 파일에는 프로세스마다 새로 만드는 중계기 토큰만 들어갑니다.
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

## 결정과 남은 범위

웹 프론트엔드는 컴파일 시 바이너리에 내장하고 명시적인 `GROK_BOT_WEB_ROOT`만 외부 override로 둡니다. 기존 상태 파일 호환과 P3의 관리형 Chrome 보호 조건은 유지합니다. P4의 실제 전체 테스트·release·독립 실행 결과는 위 검증 기록에 남겼습니다. 최종 배포 의존성 전체의 고지·소스 제공 점검은 별도 범위로 남깁니다. Tauri(P5), 동시 실행 정책 확장, 봇별 모델, 다른 운영체제와 오래된 libc 호환성은 이번 단일 Linux 실행 파일 정리와 별개입니다.
