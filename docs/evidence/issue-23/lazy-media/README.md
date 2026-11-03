# 실제 native 과거 이미지 lazy-loading 회귀

## 같은 시나리오와 측정

실제 Tauri 창을 tauri-driver/WebKitWebDriver로 열고 같은 **80개 historical image**를 가진 stopped 웹 상태를 가져왔습니다. 두 상태 SHA-256은 `9f363942c3ccd2c2753331e6b4aba65b106302b664f50c244e3b7ea9701e82ba`로 같습니다. 각 PNG는 70 bytes, 총 5,600 bytes인 안전한 synthetic fixture입니다. 대용량 파일을 실제로 수백 MB 할당하거나 heap/RSS를 측정했다고 주장하지 않습니다.

- **Before:** unsigned Linux release artifact `5294e3e9bceae98587134f636e31a14f45da7ae0` (SHA-256 `5d93ede3d9f178279d7ee627c199cb07e22d73a80bc52fb7808fc5dc797a99fb`). 이 artifact의 [이미지 소비자](<../../../../web/Previews.tsx>) file blob은 검토 대상 `e1e5854`의 소비자와 정확히 같았습니다(`69a62788420ffbef841550401eec34958b5d1bee`). 과거 artifact를 최신 앱의 성공 증거로 바꾸어 부르지 않습니다.
- **After:** fix와 main `1283a7c`를 반영한 실제 native 구현 `5a5cb9c4830e92f1a21a7787fb01298c53d1f68d`.
- 같은 1040×760 가상 화면/다크 테마, 동일한 Bot·saved answer·80개 파일·화면상 9개 이미지입니다. `buffered`는 API를 읽어 만든 `blob:` src를 가진 실제 mounted image 수이며, 단순한 마운트 수나 PNG decode 수가 아닙니다.

| 상태 | Mounted | 화면상 이미지 | 초기 Blob 준비 | off-fold 이미지로 스크롤 후 |
| --- | ---: | ---: | ---: | ---: |
| Before | 80 | 9 | 80 | 해당 없음 |
| After | 80 | 9 | 11 | 20 (전체 80이 아님) |

원문 결과는 [Before JSON](<before.json>)과 [After JSON](<after.json>)입니다.

## 동작 검증

수정은 native 인증이 필요한 `loading="lazy"` 이미지에만 적용됩니다. transcript viewport 근처에 들어오거나 사용자가 Open을 명시적으로 선택하기 전에는 API를 읽어 Blob으로 만들지 않습니다. 한 번 본 이미지는 유지하므로 scroll-out마다 높이가 무너지거나 열린 preview가 깨지는 새 동작을 만들지 않습니다. web/data/blob/non-lazy 경로는 기존 동작입니다.

After의 실제 창에서 off-fold 이미지의 스크롤 후 로딩/PNG 디코딩, 기존 Open modal 표시, 대화를 떠날 때 object URL revoke, 가져오기 원본의 바이트 보존을 확인했습니다. 기존 브라우저의 diagram/media/키보드/생성 이미지 시나리오 세 개도 통과했습니다. 새 virtualization/LRU/framework나 대화 데이터 스키마를 추가하지 않았습니다.
