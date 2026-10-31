# Grok Bot Local

Grok Bot의 데스크톱 구성을 참고해 만든 로컬 웹 클라이언트입니다. Grok Build가 대화와 도구 실행을 담당합니다. OpenAI 호환 모델 서버를 연결하고, 브라우저에서 로컬 작업 폴더의 텍스트 파일을 읽고 수정합니다.

## 실행

Node.js 22.12 이상과 PATH에서 실행할 수 있는 Grok Build 1.0.62가 필요합니다. 개발·검증 환경은 Node.js 24와 Linux입니다. 다른 Grok Build 버전은 파일 도구 제한을 재검증한 뒤 활성화해야 하므로 현재 실행을 거절합니다.

```sh
npm install
npm run dev
```

브라우저에서 `http://127.0.0.1:3000`을 엽니다. 포트를 바꾸려면 `PORT=3100 npm run dev`로 실행합니다.

네트워크에서 접속하려면 `HOST=0.0.0.0 npm run dev` 또는 빌드 후 `HOST=0.0.0.0 npm start`로 실행합니다. 이 컴퓨터의 IPv4 주소, 컴퓨터 이름, `<컴퓨터 이름>.local`로 접속할 수 있습니다. Tailscale DNS 등의 추가 이름은 `GROK_BOT_ALLOWED_HOSTS=devbox.example.net,other.example.net`으로 지정합니다. 호스트 이름은 대소문자와 마지막 마침표를 정규화하며, 동일 출처 검사는 유지합니다. 로그인 기능이 없으므로 신뢰하는 네트워크에서 사용합니다.

배포용 빌드를 같은 컴퓨터에서 실행하려면 다음 명령을 사용합니다.

```sh
npm run build
npm start
```

## 모델과 작업 폴더 연결

1. 왼쪽 아래 **설정**을 엽니다.
2. **Models**에서 Base URL과 Model ID를 입력합니다. 예를 들어 OpenAI의 기본 주소는 `https://api.openai.com/v1`입니다. 로컬 모델 서버의 주소는 해당 서버 설정을 따릅니다.
3. API 키가 필요한 서버는 키를 입력합니다. 저장된 키는 서버에만 보관하며 브라우저로 다시 보내지 않습니다.
4. **연결 테스트**로 `/models` 응답을 확인합니다.
5. **Local files**에서 작업 폴더의 절대 경로를 입력합니다.
6. **설정 저장**을 누릅니다.

API 기본 주소에는 `/chat/completions`를 붙이지 않습니다. 모델은 Chat Completions 스트리밍과 함수 도구 호출을 지원해야 합니다. 키 없는 로컬 서버에는 Grok Build 1.0.62의 시작 인증을 위해 비밀이 아닌 Bearer 값을 보냅니다. 헤더 자체를 거절하는 로컬 서버는 별도 호환 설정이 필요합니다. 실제 공급자별 호환성은 사용하는 주소와 모델로 확인해야 합니다.

## 대화와 파일 편집

- 사이드바의 **+**로 봇을 만듭니다. 봇 이름과 지침을 설정할 수 있습니다.
- 하단 입력창에서 Enter로 메시지를 보냅니다. Shift+Enter는 줄바꿈입니다.
- 상단 **새 대화**와 **대화 기록**으로 대화를 나눕니다. 실행 중인 작업은 **작업 중단**으로 취소합니다.
- 상단 **컴퓨터** 버튼이나 입력창의 **+**로 파일 탐색기를 엽니다.
- 텍스트 파일을 선택하고 내용을 편집한 뒤 **저장**을 누릅니다. 저장은 실제 디스크에 반영됩니다.
- AI가 제안한 파일 생성·수정은 대화의 승인 카드에서 내용을 확인한 뒤 한 번씩 승인합니다. 거절하면 파일을 변경하지 않습니다.
- 파일이 외부 편집기에서 변경되면 오래된 내용의 저장을 거절합니다. 입력 내용을 복사한 뒤 새로고침해 충돌을 해결합니다.

## 로컬 실행과 보안 범위

웹 서버와 Grok Build는 파일이 있는 컴퓨터에서 실행해야 합니다. 기본 바인딩은 `127.0.0.1`이며 `HOST=0.0.0.0`으로 네트워크 접속을 활성화합니다. 연결한 브라우저는 서버 컴퓨터의 파일을 편집합니다.

작업 폴더 아래의 UTF-8 일반 텍스트 파일을 지원합니다. 직접 여는 파일의 크기 제한은 1 MiB입니다. AI의 파일 생성·수정 제안은 전체 내용을 확인할 수 있도록 160 KiB로 제한합니다. 경로 이탈, 심볼릭 링크, 하드 링크, 바이너리 파일을 거절합니다. 에이전트는 앱의 제한된 MCP 파일 도구를 사용합니다. 이 버전은 셸 실행, 원격 데스크톱 제어, 외부 플러그인 설치를 제공하지 않습니다.

설정·대화·Grok 실행 세션은 기본적으로 `~/.local/share/grok-bot-q`에 저장됩니다. `GROK_BOT_DATA_DIR`로 다른 위치를 지정할 수 있습니다. API 키가 포함된 상태 파일은 사용자 전용 권한으로 저장됩니다. OS 비밀 저장소를 사용하는 암호화 보관 기능은 포함되지 않습니다. 앱 데이터 폴더를 작업 폴더로 연결하지 않습니다.

Grok Build는 별도 앱 전용 HOME과 설정을 사용합니다. 기존 사용자의 Grok 로그인·설정·대화 파일을 수정하지 않습니다. 앱을 종료해도 이미 승인해 저장한 파일 변경은 유지됩니다. 중단은 변경 취소 기능이 아닙니다.

이 앱은 개인용 로컬 실행을 대상으로 합니다. 공개 포트나 리버스 프록시를 통한 인터넷 공개에는 별도의 인증과 배포 보안 설계가 필요합니다.

## 검증 명령

```sh
npm run typecheck
npm test
npm run build
npm run test:e2e
```

테스트는 임시 작업 폴더와 로컬 OpenAI 호환 모의 서버를 사용합니다. 실제 Grok Build를 사용하는 통합 테스트도 포함합니다. API 키나 유료 모델 호출은 필요하지 않습니다. 실제 Grok 통합 테스트는 설치된 실행 파일이 있을 때 실행됩니다.

브라우저 테스트는 Chromium을 사용합니다. Google Chrome이 설치되어 있어야 합니다. 테스트 출력은 `/tmp/grok-bot-playwright-results`에 저장됩니다.

## 구현 범위와 출처

화면은 `../grok-bot-0.18-reconstructed`의 스크린샷과 관찰한 레이아웃을 기준으로 직접 구현했습니다. 원본 renderer 번들·폰트·이미지 자산은 포함하지 않습니다. 전체 원본 제품의 모든 화면에 대한 픽셀 일치 검증은 수행하지 않았습니다.

- [Grok Build](https://github.com/xai-org/grok-build)
- [ACP 문서](https://github.com/xai-org/grok-build/blob/main/crates/codegen/xai-grok-pager/docs/user-guide/15-agent-mode.md)
- [OpenAI 호환 모델 설정](https://github.com/xai-org/grok-build/blob/main/crates/codegen/xai-grok-pager/docs/user-guide/11-custom-models.md)

이 프로젝트는 xAI 또는 원본 제품의 공식 클라이언트가 아닙니다. Grok Build와 사용한 의존성의 라이선스 및 모델 서비스 이용 조건은 각각 적용됩니다. 구조와 데이터 소유권은 [ARCHITECTURE.md](ARCHITECTURE.md)에 정리했습니다. 검증 결과와 확인 범위는 [docs/verification.md](docs/verification.md)에서 확인할 수 있습니다.
