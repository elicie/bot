# Issue #12 최신 main 통합 검증

## Before / After — 실제 앱

- **Before:** `576aa44ad864b306e51e13d6fead65c251f077df` — 선택한 보고만 취소하는 PR #39, 서비스/업데이트 수명주기, MCP 종료 개선과 미리보기를 포함한 main입니다.
- **After:** `4af531e7ffc12c09430c68d41ae3442363a072e3` — 위 main을 통합한 Bot 체인 구현의 고정 소스/바이너리 빌드입니다.
- 같은 A→B→C 검토 요청, 봇·아바타·일회 승인 2회·초안 `별도 질문 초안`, **1280×900, Dark, ko-KR, UTC, 1×**입니다. 390×844/320×844도 확인했습니다.
- 실제 Rust 앱/엔진과 Chrome에 루프백 모의 공급자를 연결했습니다. UI/API 상태를 덮어쓰지 않았으며, A/B의 최종 답변은 C 결과가 실제 입력에 들어왔는지에 따라서만 달라집니다. 개인 대화·키·작업 폴더를 쓰지 않았습니다.

| 같은 상황 | Before | After |
| --- | --- | --- |
| C가 답변 중인 A 대화 | ![Before: B를 완료로 표시](<before-waiting.png>) | ![After: 하위 답변 대기와 독립된 체인 중단·전송 동작](<after-waiting.png>) |
| C 결과 이후 A 대화 | ![Before: B의 임시 답변만 수신](<before-final.png>) | ![After: C의 47초 결과를 B를 거쳐 한 번 보고](<after-final.png>) |

모바일: [Before 390px](<before-waiting-390.png>) / [After 390px](<after-waiting-390.png>), [After 320px](<after-waiting-320.png>).

캡처에서 C는 실제 전역 슬롯을 차지하므로 **전송은 보이되 기존 정책대로 비활성화**됩니다. 슬롯이 비면 별개 사용자 턴을 보낼 수 있는지는 명확히 구분한 클라이언트 계약과 수동 스케줄러의 실제 엔진 테스트로 확인했습니다. 주입한 클라이언트 테스트 상태는 이 캡처에 사용하지 않았습니다.

## 선택한 내부 보고 취소

B의 수신 대화에서 실제 **C 전문가 답변: 이 보고 취소**를 누릅니다. 요청은 `{deliveryIds:[C의 전달 ID]}`이고 응답은 `{state,cancelledDeliveryIds}`이며 실제 취소한 ID만 반환함을 확인했습니다. 다른 루틴이 슬롯을 차지해 C의 보고가 대기하는 실제 FIFO 상태입니다.

[취소 전 B 320px](<after-queued-b-320.png>) → [취소 후 B 320px](<after-cancelled-b-320.png>) → [A의 비성공 보고](<after-cancelled-a.png>).

선택한 C 보고는 취소되고, 그 결과를 정리할 수 없게 된 B의 부모 전달도 `cancelled`로 끝납니다. B의 임시 위임 문구를 성공 답변으로 복사하지 않고 A에는 비성공 상태/빈 답변을 한 번 전달합니다. 전체 체인 종료 래치를 만들지 않으며 독립 루틴은 정상 종료합니다. 같은 상태로 서버를 재시작해도 메시지나 보고를 재생하지 않습니다. 일반 원래 대화의 다른 도착 보고는 선택 취소에 포함하지 않습니다.

Before 주 모델 요청 A4/B3/C1, 정상 After A4/B4/C1, 선택 취소 After A4/B3/C1/별도 루틴1입니다. 모든 실행은 의도한 메시지 승인 2회만 했으며, 브라우저 오류와 각 폭의 가로 넘침은 0건입니다. 출처·바이너리/Web 해시·결과는 [검증 JSON](<verification.json>)에 있습니다.

## 검사 — 통과

```sh
node scripts/cargo.mjs test --locked -p grok-bot-model -p grok-bot-store -p grok-bot-core
node scripts/cargo.mjs clippy --locked -p grok-bot-model -p grok-bot-store -p grok-bot-core --all-targets -- -D warnings
cargo fmt --all --check
npm run build
npx tsx --test tests/frontend-state.test.ts tests/frontend-composer.test.ts
GROK_BOT_BACKEND=rust GROK_BOT_SERVER_BIN=/absolute/path/to/grok-bot npx playwright test tests/bot-reports.spec.ts
```

최신 통합에서 **Rust 213개**(model 4/store 135/core 74), strict Clippy·format, 프론트 빌드/타입 검사·상태/Composer 9개·브라우저 9개가 통과했습니다. 브라우저에는 선택 취소의 실제 ID 반환, 스케줄러가 먼저 시작했을 때 무변경 안내와 재시작 유지도 포함합니다.

원래 CI에서 발견된 체크리스트 회귀는 다음 턴 구독 전에 이전 턴의 최종 저장/broadcast를 `Store::flush`로 완료해 해결했습니다. 검증 조건을 느슨하게 하거나 테스트를 건너뛰지 않았습니다. 새 main의 비공개 MCP probe 상태는 `HttpContext::new`로 앱과 수동 HTTP 어댑터가 같은 기본값을 초기화하며, 업데이트 maintenance 중에는 만료된 체인도 변경하지 않고 재개 후 처리합니다. Store→runs 잠금 순서와 체인 중단의 runs→Store 중첩을 피하도록 실행 목록을 먼저 복제합니다.

[1차 리뷰 시점의 실제 모델 검증](<../issue-12-review/README.md#수정-구현의-실제-모델-확인>)은 gpt-6.1-sol에서 C→B→A를 36.6초에 한 번 확인한 기록입니다(최초 제출 때 35.1초 실행과 별개). 최신 main 통합에서는 위 실제 앱/엔진 회귀와 로컬 모의 공급자로 재검증했으며, 새로운 외부 모델 실행을 했다고 주장하지 않습니다. 키·사용자 상태는 이 자료에 포함하지 않습니다.
