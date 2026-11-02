# 원격 MCP OAuth — 미완료 초안

## 범위와 현재 상태

[이슈 #15](https://github.com/elicie/bot/issues/15)는 원격 MCP 서버의 OAuth 로그인, 토큰 수명 주기, 설정 화면의 연결 상태·로그인·로그아웃, loopback 전용 로그인 시작을 요청합니다.

이 초안은 자격 증명을 다루지 않는 **[메타데이터 디스커버리 모듈](<../crates/core/src/mcp_oauth/mod.rs>)**을 먼저 분리합니다. 아직 설정 화면이나 실행 경로에 연결되지 않았으므로, 이 변경만으로 OAuth 서버에 로그인하거나 도구를 호출할 수는 없습니다. 기존 stdio·수동 HTTP 헤더·시크릿 참조 방식은 변경하지 않습니다.

## 먼저 확정할 보안 계약

현재 보관함의 값은 사용자 전용 권한의 비공개 상태 파일에 평문으로 저장되며, 공개 상태 응답·SSE에서는 빠지고 문자열에서도 가려집니다. 이는 저장 시 암호화와는 다른 보호입니다. 현재 동작은 [보관함 설계](<../ARCHITECTURE.md#vault-secrets-and-custom-mcp-servers>)와 [저장소 회귀 테스트](<../crates/store/tests/vault.rs#L19-L67>)에 명시되어 있습니다.

원문의 “평문 노출 금지”와 “토큰이 state·로그·SSE에 노출되지 않음”이 비공개 디스크 파일까지 포함하는지, 공개 상태 응답을 뜻하는지는 확정되지 않았습니다. 디스크 암호화가 필요하다면 토큰 저장소뿐 아니라 키 보관·접근·재시작 시 해제 정책도 필요합니다. 기존 평문 보관함을 그대로 쓰거나 전체 보관함을 암호화하는 결정을 이 초안에서 임의로 내리지 않습니다. 실제 계정 로그인과 토큰 영구 저장은 이 경계가 확정되기 전에는 수행하지 않습니다.

## 디스커버리의 경계

- 구성된 HTTPS 리소스에 대한 protected-resource 메타데이터를 먼저 확인하고, 반환된 `resource`의 일치를 검증한 뒤에만 authorization server를 조회합니다.
- 리소스 경로를 반영하는 well-known 위치와 루트 위치, authorization server의 RFC 8414·OIDC 메타데이터 위치를 구분합니다. 발견하지 못한 엔드포인트를 추측하지 않습니다.
- 잘못된 `resource`나 issuer를 정상 데이터로 취급하지 않으며, 신뢰할 수 없는 응답 본문·네트워크 오류의 상세를 사용자 오류 문자열에 싣지 않습니다.
- OAuth 메타데이터와 게시된 엔드포인트는 HTTPS만 허용합니다. 실제 PKCE 공개 클라이언트 로그인에 사용할 `S256` 지원 여부도 검사합니다.
- 요청 수·응답 크기·대기 시간을 제한하고, 메타데이터 요청에 자격 증명이나 기존 MCP 헤더를 보내지 않습니다.
- 메타데이터 요청은 리다이렉트와 환경 프록시를 따르지 않습니다. DNS 응답이 모두 공개 주소인지 검사하고, 검사한 주소를 실제 TLS 연결에 고정합니다. TLS 인증서와 호스트 이름 검증은 유지합니다.

이는 OAuth 전용의 제한된 초기 정책입니다. 기존 수동 HTTP MCP 연결의 주소·인증 호환성을 변경하는 정책이 아닙니다. 아직 `WWW-Authenticate`의 `resource_metadata` 포인터를 읽는 경로는 없으며, well-known 조회만으로 전체 MCP 인증 디스커버리가 완성되었다고 보지 않습니다.

`resource` 식별자와 MCP 전송 엔드포인트도 구분해야 합니다. 예를 들어 [Notion 공개 메타데이터](https://mcp.notion.com/.well-known/oauth-protected-resource)는 리소스를 `https://mcp.notion.com`으로 게시하며 전송 경로 `/mcp`와 문자열이 다릅니다. 이 초안은 일치 검증을 느슨하게 해 이 차이를 감추지 않습니다. 실제 서버 통합에서는 서버가 공표한 식별자를 어떻게 안전하게 선택하고 전송 주소에 묶을지도 검증해야 합니다.

## 남은 통합

저장 계약 확정 뒤에도 동적 등록 또는 수동 client ID, PKCE·일회성 state·loopback 콜백, 취소·시간 초과·종료 정리, 토큰 갱신 및 철회·만료 시 다시 로그인 안내가 필요합니다. 로그인 대기 중에는 전역 명령 큐를 점유하지 않아야 합니다. 봇과 리소스의 자격 증명 범위를 묶고, 설정 변경·삭제·로그아웃 뒤의 재사용도 방지해야 합니다.

기존 공개 상태 가리기 목록과 실행별 가리기 목록은 새 OAuth 토큰을 자동으로 알지 못합니다. 저장·갱신된 토큰을 MCP 응답, 모델 출력, 실패 메시지, 상태 응답과 SSE에서도 안전하게 처리하는 연결이 필요합니다. 로그아웃이 로컬 자격 증명 삭제인지 공급자의 토큰 철회까지 수행하는지 구분해야 합니다.

설정 화면은 기존 MCP 행을 확장하되 로그인 여부와 서버 사용 여부를 구분해야 합니다. LAN에서 로그인 시작은 UI 표시뿐 아니라 실제 소켓 기반 서버 검사로 거절해야 합니다. UI 변경 시에는 같은 시나리오의 실제 Before/After 캡처가 별도로 필요합니다.

## 검증의 한계

[디스커버리 회귀 테스트](<../crates/core/src/mcp_oauth/tests.rs>)는 잘못된 리소스·issuer, HTTPS·PKCE 조건, JSON 구조·생략값, 리다이렉트, 응답·요청·시간 제한을 확인합니다. [네트워크 정책 테스트](<../crates/core/src/mcp_oauth/fetch.rs>)는 빈 DNS 응답과 사설·혼합 주소 거절을 확인합니다. 기본 테스트에서는 외부 서비스에 접속하지 않으며, 선택적인 `live_notion_metadata` 테스트만 공개 Notion 메타데이터를 익명 HTTPS로 조회합니다.

디스커버리 테스트는 로그인이나 실제 OAuth 서버의 도구 호출 성공을 대체하지 않습니다. 전체 완료에는 격리된 앱 데이터, 서버 컴퓨터의 loopback 콜백에 도달할 수 있는 브라우저, 사람이 직접 동의하는 실제 계정, 보안 검토, 실제 MCP 도구 호출과 토큰 비노출 검증이 필요합니다.

## 표준 참고

- [RFC 9728: OAuth 2.0 Protected Resource Metadata](https://www.rfc-editor.org/rfc/rfc9728.html)
- [RFC 8414: OAuth 2.0 Authorization Server Metadata](https://www.rfc-editor.org/rfc/rfc8414.html)
- [MCP authorization](https://modelcontextprotocol.io/specification/latest/basic/authorization)
