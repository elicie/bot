# Issue #12 최초 제출 검증 기록

이 디렉터리는 첫 제출 시점의 역사적 기록입니다. PR #24 병합 위로 rebase하고 입력·백업·중첩 취소를 수정한 **최신 비교와 검증**은 [리뷰 이후 기록](<../issue-12-review/README.md>)을 참고하세요.

## 비교 대상과 조건

- **Before:** `95ffc464e8d55c1a315fddb31101fadb9c9c5b7b` — PR #9 병합 기준의 Rust 서버와 프론트엔드를 별도로 빌드했습니다.
- **After:** `ae355d97f15968e9c506975023cac7dbb640d2da` — 구현 커밋 그대로 빌드했습니다. 캡처 시 애플리케이션 작업 트리는 깨끗하며, 이 기록과 이미지를 넣는 이후 커밋은 앱 코드를 바꾸지 않습니다.
- 실제 앱과 Chrome, 일회용 0700 데이터 디렉터리/0600 상태 파일, 1280×900, 기기 배율 1, Dark, ko-KR, UTC입니다. 이름·아바타·사용자 입력·A→B/B→C 메시지·승인 2회가 같습니다.
- 모의 모델은 C의 권장 재시도 시간을 **47초**로 답합니다. A/B가 이 값을 답할지는 실제 모델 입력에 하위 결과가 도착했는지로 결정하며, Before/After 표지로 정답을 주입하지 않습니다.
- C 응답을 보류한 같은 시점의 A 대화와, C를 해제한 뒤 모든 작업이 끝난 A 대화를 각각 비교했습니다. 개인 대화·계정·자격 증명을 사용하지 않았습니다.

## Before / After

| 상황 | Before | After |
| --- | --- | --- |
| C가 답하는 동안 A 대화 | ![Before: 임시 답변을 완료로 처리하고 부모 Stop 없음](<before-waiting.png>) | ![After: 하위 답변 대기, 대기 안내, 부모 Stop](<after-waiting.png>) |
| C 답변 후 A 대화 | ![Before: C의 결과가 A에 도착하지 않음](<before-final.png>) | ![After: B 정리 후 A가 47초 결과를 보고](<after-final.png>) |

추가 상태: [320px 대기](<after-waiting-320.png>), [부모 Stop 이후](<after-stop.png>).

Before에서는 C가 답해도 B의 임시 위임 문구가 A에 올라오고 B의 후속 턴이 없습니다. After에서는 C→B→A 순서로 각 정리가 한 번 실행되고 숨김 보고 입력은 화면에 노출되지 않습니다. 대기 중 A의 실제 Stop 버튼을 눌러 두 전달이 취소되고 보고가 `skipped`로 끝나는 것을 확인했습니다. 뒤늦게 C 응답을 해제해도 대화/실행 ID/메시지 수가 바뀌지 않았습니다. Stop 뒤에는 “작업 완료” 대신 “Bot 작업을 중단했습니다.”가 표시됩니다.

320px/390px에서 가로 넘침이 없고, 완료/Stop 캡처 모두 브라우저 오류가 0건입니다. 공개 가능한 캡처 조건·출처 해시·결과는 [verification.json](<verification.json>)에 있습니다. 서버 바이너리·개인 상태·공급자 요청 원문·키는 포함하지 않습니다.

## 자동 검사

```sh
node scripts/cargo.mjs test --locked \
  -p grok-bot-model -p grok-bot-store -p grok-bot-core
node scripts/cargo.mjs clippy --locked \
  -p grok-bot-model -p grok-bot-store -p grok-bot-core \
  --all-targets -- -D warnings
cargo fmt --all --check
npm run build
npx tsx --test tests/frontend-state.test.ts
GROK_BOT_BACKEND=rust GROK_BOT_SERVER_BIN=/absolute/path/to/grok-bot \
  npx playwright test tests/bot-reports.spec.ts
```

184개 Rust 테스트(model/store 118개, core 66개), 프론트 상태 5개, Playwright 3개와 모든 빌드/검사가 통과했습니다. Rust runtime 통합 테스트는 프로세스 전역 엔진 환경 때문에 여러 시나리오를 하나의 순차 테스트 안에서 실행합니다.

회귀 범위는 A→B→C 한 번씩 수합, 4-hop 역순 보고, 내부 정리 중 재전달의 hop 유지, 대기/승인/추론/보고 중 Stop과 기한, 종료 직후 크래시 복구, 오래된 스케줄러 시작 거절, 실패 후 고아 보고 차단, 과거 완료 보고 보존, 기존 혼합 보고의 hop 동률 계보, 이전 저장 상태, 일회 백업/비밀 마스킹입니다. 프론트엔드의 기존 큰 번들 경고는 남아 있지만 빌드는 통과합니다.

## 실제 모델 3단계 스모크 — 1회 성공

- 모델: `gpt-6.1-sol`.
- 시각: 2026-11-02 05:40:58.875–05:41:34.012 UTC, 약 35.1초.
- 별도 일회용 앱에서 A→B/B→C 메시지만 일회 승인했습니다(2회). 파일 작업 폴더는 연결하지 않았고, 공급자 키는 메모리 중계에서만 사용했습니다. 기존 앱 데이터의 해시·소유권·권한이 유지됐고 임시 프로세스/데이터는 정리했습니다.
- C의 지침에만 둔 임시 시험 토큰이 아래처럼 각각 한 번씩 전달됐습니다. A/B의 초기 지침과 전달에는 이 값이 없었습니다.

```text
C: C_RESULT=C-CED177F769  (05:41:26.931 UTC)
B: B_FINAL=C-CED177F769   (05:41:30.457 UTC)
A: A_FINAL=C-CED177F769   (05:41:33.957 UTC)
```

같은 체인의 hop 1/2 전달 2건, 보고 2건 모두 완료, A/B 각각 숨김 보고 입력 1개와 최종 답변 1개를 확인했습니다. 이 토큰은 검증용 임의 문자열이며 사용자 비밀이 아닙니다. 실제 모델 검증은 이 한 번의 실행에 한정합니다.
