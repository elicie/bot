# Issue #12 리뷰 후 검증

## 비교 기준

- **Before main:** `d6c90d6698a890667a5edbbc3f898b53e26b09bc` — PR #24의 대기 보고 취소와 PR #30을 포함한 최신 main을 별도 빌드했습니다.
- **Before 이전 PR:** `a83ee91d3bb30457fa5c55605a5d7f2ddc222705` — 입력창의 전송 동작을 없앴던 기존 PR27입니다. 이 추가 기준은 main에 없던 PR 자체의 회귀를 비교합니다.
- **After:** `f4587fd66ec2b18b0a2b7fcfe3e8fcfb49ad8ffd` — 리뷰 수정 구현을 고정된 별도 작업 트리/바이너리로 빌드했습니다. 캡처 당시 애플리케이션 소스는 깨끗하고 이후 커밋은 테스트·검증 기록만 추가합니다.
- 실제 Rust 앱과 Chrome, 같은 A→B→C 요청/봇/아바타/승인 2회/초안 `별도 질문 초안`, 1280×900, Dark, ko-KR, UTC, 기기 배율 1입니다. 같은 상황을 390×844/320×844에서도 확인했습니다.
- 화면/API 상태를 덮어쓰지 않은 실제 앱 캡처입니다. 모의 공급자의 답변은 실제 전달된 데이터에 따라서만 달라지며 Before/After 이름으로 정답을 주입하지 않습니다.

## Before / After

| 같은 상황 | Before | After |
| --- | --- | --- |
| 이전 PR의 입력 동작 회귀 | ![Before 이전 PR: Send 없이 Stop만 남음](<before-pr27-input.png>) | ![After: 전송 동작과 독립된 체인 중단](<after-waiting.png>) |
| main에서 C를 기다리는 A | ![Before main: 임시 완료 표시](<before-main-waiting.png>) | ![After: 하위 답변 대기](<after-waiting.png>) |
| C의 최종 답변 후 A | ![Before main: C 결과가 전달되지 않음](<before-main-final.png>) | ![After: B를 거쳐 47초 결과를 보고](<after-final.png>) |

모바일 입력 비교: [Before 390px](<before-pr27-input-390.png>) / [After 390px](<after-waiting-390.png>), [After 320px](<after-waiting-320.png>).

**전송 가능성의 경계:** 캡처의 C는 실제 전역 실행 슬롯을 차지하므로 전송 화살표는 기존 정책대로 비활성화됩니다. 수정은 체인 대기 자체를 `running`으로 취급하지 않고 전송 동작을 유지하며 **Bot 체인 중단**을 별도로 표시하는 것입니다. 슬롯이 비어 있는 대기 상태에서 Enter가 새 메시지를 요청하는지는 명확히 표시된 클라이언트 계약 테스트로, 실제 서버가 별도 사용자 턴을 처리한 뒤 기존 체인을 한 번 보고하는지는 수동 스케줄러/실제 엔진 runtime 회귀로 각각 확인했습니다. 클라이언트 계약의 주입 상태는 위 시각 증거로 사용하지 않았습니다.

## PR #24와 중첩 보고 취소

C 답변을 B가 정리하기 전에 오래된 다른 루틴이 FIFO 슬롯을 차지하도록 했습니다. B의 수신 대화에서 실제 **대기 중인 이어서 답하기 모두 취소**를 누르면 다음 결과가 됩니다.

- C의 완료 답변은 보존하고 그 대기 보고만 취소합니다.
- B의 기다리던 전달은 `cancelled`로 끝나고, 위임 중간 답변을 성공 결과로 A에 복사하지 않습니다.
- 다른 루틴이 끝나면 A는 비성공 상태/빈 답변을 한 번 보고받습니다. B의 정리는 다시 실행되지 않습니다.
- 같은 데이터로 서버를 재시작해도 실행·메시지·보고가 재생되지 않습니다.

[취소 전 B 320px](<after-cancel-queued-b-320.png>) / [취소 후 B 320px](<after-cancelled-b-320.png>) / [A의 비성공 보고](<after-cancelled-a.png>).

일반 체인은 A4/B4/C1의 주 모델 요청으로 C→B→A를 한 번씩 완료했습니다. 중첩 보고 취소 시에는 A4/B3/C1/다른 루틴1로 B를 다시 실행하지 않고 취소를 보고했습니다. 두 실행 모두 브라우저 오류와 모든 캡처 폭의 가로 넘침이 0건입니다. 공개 가능한 결과·출처/바이너리/Web 해시는 [verification.json](<verification.json>)에 있습니다.

## 검사와 저장/용량 경계

```sh
node scripts/cargo.mjs test --locked -p grok-bot-model -p grok-bot-store -p grok-bot-core
node scripts/cargo.mjs clippy --locked -p grok-bot-model -p grok-bot-store -p grok-bot-core --all-targets -- -D warnings
cargo fmt --all --check
npm run build
npx tsx --test tests/frontend-state.test.ts tests/frontend-composer.test.ts
GROK_BOT_BACKEND=rust GROK_BOT_SERVER_BIN=/absolute/path/to/grok-bot npx playwright test tests/bot-reports.spec.ts
```

리뷰 이후 실제 엔진 runtime, 전체 변경 크레이트 테스트, strict lint/format, 프론트 빌드·상태/Composer 9개와 브라우저 7개를 검증했습니다. Rust는 model 4개/store 128개/core 72개로 총 204개이며, store에는 새 저장 회귀 8개가 포함됩니다. 브라우저에는 기존 PR #24의 1040px/390px 보고 취소·재시작 시나리오도 포함됩니다.

새 저장 회귀는 PR #9 보고 상태에서 최초 `waiting_children`/`chainTermination` 직전 별도 `state.json.pre-bot-chains.bak`(0600)이 생기는 것, 이전 백업/새 백업 불변성, 심볼릭 링크 실패 시 이전 상태 파일 보존, 실제 쓰기 실패를 숨기지 않는 것, 정확한 보고 입력 크기에 따른 원자적 거절, 금고 데이터를 포함한 용량 계산, 14건째 수락/동일 요청 재시도/15건째 거절과 과거 큰 체인 로드를 확인합니다.

빈 저장소의 4-hop가 413이라는 주장은 재현되지 않았습니다. 최악의 JSON 제어문자 답변/요청을 쓰는 회귀에서 마지막 수신 완료 전 실제 상태 13,208,224 B + 이전 예약 19,757,056 B = 32,965,280 B로 64MiB보다 작았습니다. 모든 역순 보고 뒤 상태는 24,416,535 B였습니다. 예약은 capped 답변·오류/교환 복사본과 메타데이터로 줄이고, 실제 숨김 보고 입력(이 테스트에서 401,467 B)은 시작 직전 별도 검사합니다. 임의 도구 출력 등 모든 미래 쓰기까지 보장하는 주장은 하지 않습니다.

## 수정 구현의 실제 모델 확인

`f4587fd` 바이너리에서 `gpt-6.1-sol`로 2026-11-02 **07:19:13.198–07:19:49.841 UTC**, 약 **36.6초**에 A→B→C를 한 번 재검증했습니다. 최초 제출 때의 35.1초 실행과 별개인 리뷰 후 검증이며 자동 재시도는 하지 않았습니다.

```text
C_RESULT=C-E6ABC68431  (07:19:43.039 UTC)
B_FINAL=C-E6ABC68431   (07:19:46.642 UTC)
A_FINAL=C-E6ABC68431   (07:19:49.802 UTC)
```

C만 아는 검증용 임의 토큰이 각 단계에서 한 번씩 전달됐고, 같은 체인 hop 1/2·승인 2회·양쪽 보고 완료를 확인했습니다. 작업 폴더를 연결하지 않은 일회용 앱이며 공급자 키는 메모리 중계에서만 사용했습니다. 원래 사용자 데이터의 해시·소유권·0600 권한은 유지됐고 임시 서버/중계/브라우저/데이터는 정리했습니다. 사용자 대화·공급자 키·요청 원문은 이 기록에 포함하지 않습니다.
