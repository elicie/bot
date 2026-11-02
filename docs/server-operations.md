# 7777 Rust 서버 운영 (Linux, systemd 사용자 서비스)

이 절차는 기존 Rust 서버를 release 빌드로 운영합니다. Node 백엔드 제거·정적 자산 내장은 별도 P4 작업이며, 여기서는 Rust 바이너리와 같은 빌드의 `dist/`를 하나의 설치 릴리스로 묶습니다. 프론트 화면/HTTP 데이터 형식은 바꾸지 않습니다.

## 준비와 첫 전환

- 기존 개발 실행과 동일한 일반 사용자로 실행합니다. root로 앱이나 설치 스크립트를 실행하지 마세요.
- Node.js 22.12 이상, Rust stable, C/C++ 빌드 도구, `pkg-config`, zlib 개발 파일, `unzip`, `ripgrep`, `tar`, systemd 사용자 관리자와 `loginctl`이 필요합니다([빌드 환경](../README.md#실행)). `npm ci`로 잠금 파일에 맞는 빌드 의존성을 준비합니다. 실행 중인 서비스 자체에는 Node나 Cargo가 필요하지 않습니다. 아래 명령은 모두 이 저장소 **체크아웃 루트**에서 실행하며, URL의 7777은 설정한 실제 포트로 바꿉니다.
- 먼저 현재 서버의 **실제 데이터 폴더**(`GROK_BOT_DATA_DIR`, 미지정 시 `~/.local/share/grok-bot-q`)를 확인합니다. 새 빈 폴더로 잘못 전환하면 대화가 사라진 것처럼 보입니다. `state.json`에는 API 키가 포함될 수 있으므로 내용이나 백업을 Git에 올리지 마세요.
- 기존 7777 수동 서버는 설치기가 종료하지 않습니다. 실행 중인 답변과 승인 대기를 끝내거나 앱에서 중단하고, 기존 서버를 띄운 터미널에서 Ctrl+C로 종료합니다. 자동 업데이트의 maintenance API가 없는 이전 버전에서는 이 첫 전환만 수동입니다.

로그인하지 않아도 부팅 시 사용자 서비스가 시작되도록 먼저 lingering을 활성화합니다. 호스트 정책에 따라 관리자 권한이 필요할 수 있습니다. 설치기는 이 값이 `yes`가 아니면 설치를 거절하며 권한을 우회하지 않습니다.

```sh
loginctl enable-linger "$USER"
loginctl show-user "$USER" --property=Linger
```

필요하면 설치 **전에** `~/.config/grok-bot/server.env`를 만듭니다. 없으면 설치기가 아래 기본값으로 생성합니다. 경로 예시의 `/home/alice`는 실제 홈 경로로 바꾸세요.

```ini
HOST=127.0.0.1
PORT=7777
GROK_BOT_ALLOWED_HOSTS=
GROK_BOT_DATA_DIR=/home/alice/.local/share/grok-bot-q
GROK_BOT_BACKUP_DIR=/home/alice/.local/state/grok-bot/backups
GROK_BOT_BACKUP_KEEP=7
```

환경 파일은 `KEY=값` 리터럴 형식만 허용합니다. 주석은 줄 전체 `#`로 쓰며 중복 키, 따옴표, 역슬래시, `$HOME`·`~` 등의 확장 표기는 사용하지 않습니다. 경로는 절대 경로이고, 백업은 데이터 폴더 밖이어야 합니다. 지원 키는 위 여섯 개뿐입니다. 기본값과 다른 데이터 폴더를 사용했다면 반드시 기존 경로를 적습니다. 파일 권한을 `chmod 600 ~/.config/grok-bot/server.env`로 제한합니다.

리뷰한 체크아웃에서 설치합니다. Git fetch/pull은 설치·업데이트가 자동으로 하지 않습니다. 첫 설치도 기존 `state.json`이 있으면 전체 데이터 백업을 완료한 뒤에만 새 서버를 시작합니다. 새 빈 데이터 폴더에는 초기 백업이 없습니다. 이전 수동 바이너리는 설치 릴리스에 복사하지 않으므로 첫 전환을 되돌릴 때 사용할 기존 체크아웃/빌드는 별도로 보존합니다.

```sh
npm ci
node scripts/service.mjs install
systemctl --user is-enabled grok-bot.service
systemctl --user status grok-bot.service
curl --fail http://127.0.0.1:7777/ >/dev/null
```

설치 위치:

- 사용자 유닛: `~/.config/systemd/user/grok-bot.service`
- 실행 파일·화면: `~/.local/lib/grok-bot/releases/<빌드>/`와 이를 가리키는 `current` 심볼릭 링크
- 설정: `~/.config/grok-bot/server.env`
- 기본 데이터: `~/.local/share/grok-bot-q` (기존 위치 유지)
- 기본 백업: `~/.local/state/grok-bot/backups`

기존 사용자 유닛 파일이 있으면 설치기는 덮어쓰지 않고 중단합니다. 직접 관리하던 유닛이 있다면 내용을 보존하고 충돌을 정리한 뒤 설치하세요. `default.target`에 enable하고 lingering을 확인하므로 재부팅 후 로그인이 없어도 시작하도록 구성됩니다. 실제 호스트에서 재부팅한 뒤 `loginctl show-user "$USER" --property=Linger`, `systemctl --user is-enabled grok-bot.service`, `systemctl --user status grok-bot.service`, 브라우저 접속으로 확인하세요. 설치 테스트만으로 실제 재부팅·호스트 네트워크 정책까지 검증한 것은 아닙니다. 설치 후 health 확인에 실패하면 유닛을 중지·disable하며 원인을 수정한 뒤 `systemctl --user enable --now grok-bot.service`로 다시 확인합니다.

## 한 번의 업데이트 명령

원하는 코드 리비전을 체크아웃하고 의존성 변경이 있으면 `npm ci`를 실행한 뒤:

```sh
node scripts/service.mjs update
```

명령의 순서는 다음과 같습니다.

1. 사용자별 잠금으로 중복 설치·업데이트를 거절합니다. 새 릴리스 폴더에 화면을 빌드하고 `--locked --release` Rust 서버를 빌드합니다. 이 단계의 실패는 현재 서비스와 자산을 건드리지 않습니다.
2. loopback 전용 `POST /api/maintenance`로 **실행 중 답변이 없음을 확인하면서 새 실행을 막습니다**. `starting`, `running`, `awaiting_permission`이면 HTTP 409로 중단합니다. 수동 대화·첨부 대화·예약/웹훅·Bot 후속 답변이 같은 실행 차단 상태를 확인하므로 단순 상태 조회 후 stop 사이의 새 답변 시작 경쟁을 막습니다.
3. systemd로 서비스를 정상 종료하고 완전히 멈췄는지 확인합니다. 이 뒤에 데이터 폴더 **전체**를 압축합니다. `state.json`, 마이그레이션 이전 `.bak`, 첨부/생성 이미지, 엔진 세션·설정 등이 포함됩니다. 연결한 외부 작업 폴더의 파일은 포함하지 않습니다. 데이터 폴더 밖을 가리키는 심볼릭 링크는 tar 기본 동작대로 링크만 보관합니다.
4. 임시 아카이브를 읽어 검증하고 파일을 디스크에 동기화한 뒤 `backup-<UTC시각>-<고유값>.tar.gz`로 확정하며 백업 디렉터리도 동기화합니다. 백업 실패 시 이전 릴리스를 다시 시작하고 새 릴리스로 전환하지 않습니다. 백업 폴더 0700/아카이브 0600으로 API 키와 세션을 보호합니다. 디스크가 부족하면 오래된 정상 백업을 먼저 지우지 않고 실패합니다.
5. `current`를 새 릴리스로 원자적으로 전환해 시작합니다. systemd `active`, Rust `/api/state`의 런타임 가용성, `/`의 실제 화면 HTML을 확인합니다. 모델 공급자 유료 호출은 하지 않습니다.
6. 성공한 뒤에만 정상 백업의 보관 개수를 적용합니다(기본 7, 1~1000). 다른 이름의 파일이나 심볼릭 링크는 삭제하지 않습니다. 새 백업은 반드시 보존합니다.

아직 시작하지 않은 예약 실행/Bot 전달/그룹 후속 턴/후속 보고는 디스크에 유지되며 새 서버에서 이어서 처리합니다. **실행 중 답변 없음**은 대기 작업까지 모두 비었다는 뜻이 아닙니다. 중단 시간에 놓친 반복 루틴의 처리는 기존 스케줄러 정책을 따릅니다.

빌드 중에는 이전 서버를 그대로 사용합니다. 서버가 살아 있는 maintenance 단계에서는 새 변경 요청과 웹훅에 503을 반환하며 상태 조회는 유지합니다. 실제 stop/백업/start 구간에는 접속·상태 조회가 일시적으로 불가능합니다. maintenance는 파일에 저장하지 않아 정상 재시작 시 해제됩니다. stop 전 오류(응답 유실 포함)에는 스크립트가 `DELETE /api/maintenance`로 해제합니다. 강제 종료/전원 장애로 pause만 남은 경우 새 업데이트를 재시도하거나, 다른 업데이트가 실행 중이지 않은지 확인하고 아래 명령으로 해제합니다.

```sh
curl --fail -X DELETE http://127.0.0.1:7777/api/maintenance \
  -H 'Origin: http://127.0.0.1:7777' -H 'Content-Type: application/json' \
  -H 'X-Grok-Bot: 1' --data '{}'
```

업데이트 잠금이 남으면 `~/.local/lib/grok-bot/update.lock/pid`를 확인하고, 해당 설치·업데이트 프로세스가 없다는 것을 확인한 뒤에만 그 잠금 폴더를 제거합니다. 자동으로 다른 실행의 잠금을 빼앗지 않습니다. 빌드별 릴리스 폴더는 자동 삭제하지 않습니다. 디스크 정리는 `current`가 가리키는 폴더와 복구에 사용할 이전 릴리스를 보존하면서 수동으로 합니다.

## 실패 복구와 백업 복원

새 릴리스 활성화 후 시작/health 확인에 실패하면 **서비스를 disable·중지한 뒤 이전 실행 파일을 선택하되 자동 시작하지 않습니다**. 재부팅해도 복원 전 구 버전이 자동 실행되지 않도록 disable이 먼저 수행됩니다. 새 코드가 데이터를 마이그레이션했을 수 있기 때문에 구 버전 바이너리를 변경된 데이터에 자동으로 실행하거나 데이터를 자동 덮어쓰지 않습니다. 오류에 표시한 **이전 릴리스 경로와 백업**을 한 쌍으로 사용합니다. 자동 복원은 새로 기록된 대화를 잃을 수 있습니다.

업데이트마다 새 릴리스의 `deployment.json`에 `backup`과 `previousRelease` 경로를 기록합니다. 성공 출력에도 두 경로를 표시합니다. 다른 시점의 백업을 복원하려면 해당 기록의 `previousRelease`를 선택하세요. 첫 설치 기록의 `previousRelease`는 `null`이므로 그 초기 백업은 별도로 보존한 이전 수동 빌드와 사용합니다. 호환되는 실행 파일을 식별할 수 없으면 데이터를 덮어쓰거나 서비스를 시작하지 마세요.

아래는 **경로를 실제 값으로 바꾼 뒤** 실행하는 예입니다. 오류가 나면 즉시 중단하며, 마지막 enable은 복원에 성공한 경우에만 수행합니다. 먼저 `current`가 오류/기록에 표시된 이전 릴리스를 가리키는지 확인합니다. 다른 릴리스를 선택해야 한다면 서비스를 멈춘 상태에서 `ln -sfn /절대경로/이전-릴리스 ~/.local/lib/grok-bot/current`로 선택하고 다시 확인합니다.

```sh
(
set -eu
systemctl --user disable --now grok-bot.service
readlink -f ~/.local/lib/grok-bot/current
# 위 출력이 선택한 백업의 previousRelease와 같은지 직접 확인한 뒤 다음 블록을 실행합니다.
)
```

```sh
(
set -eu
# 실제 설정의 경로와 오류/기록에 표시된 백업을 사용합니다. 아래는 예시입니다.
# 기존 .failed가 있으면 중단합니다. 현재 데이터를 보존한 뒤 빈 폴더에 복원합니다.
test ! -e /home/alice/.local/share/grok-bot-q.failed
mv /home/alice/.local/share/grok-bot-q /home/alice/.local/share/grok-bot-q.failed
install -d -m 700 /home/alice/.local/share/grok-bot-q
tar -xzf /home/alice/.local/state/grok-bot/backups/backup-<시각>-<고유값>.tar.gz \
  -C /home/alice/.local/share/grok-bot-q
test -f /home/alice/.local/share/grok-bot-q/state.json
systemctl --user reset-failed grok-bot.service
systemctl --user enable --now grok-bot.service
systemctl --user status grok-bot.service
)
```

복원한 대화/설정을 브라우저에서 확인하세요. 외부 작업 폴더의 이미 승인된 변경은 앱 데이터 복원으로 되돌려지지 않습니다.

## 실패 재시작과 로그

```sh
journalctl --user -u grok-bot.service -f
journalctl --user -u grok-bot.service -b
systemctl --user restart grok-bot.service
```

표준 출력/오류는 journald로 보냅니다. 실제 journal 보존 기간·부팅 간 보관 여부는 호스트의 journald 설정을 따릅니다. 프로세스 실패는 5초 후 재시작하며, 60초에 5번 이상 시작하면 무한 루프 대신 중단합니다. 원인을 수정한 뒤 `systemctl --user reset-failed grok-bot.service`로 제한을 해제합니다. 일반 `restart`/`stop`은 진행 중 답변을 취소할 수 있으므로 업데이트에는 위 스크립트를 사용하세요.

## LAN 접근 정책

- 기본 `HOST=127.0.0.1`은 이 컴퓨터에서만 연결할 수 있습니다. 이 모드에서 `GROK_BOT_ALLOWED_HOSTS`로 이름을 추가해도 LAN 바인딩이 열리지 않습니다.
- 신뢰하는 LAN에서만 사용하려면 설정에 `HOST=0.0.0.0`을 씁니다. **모든 IPv4 인터페이스**에서 수신하므로 LAN 전용 방화벽/접근 제어는 호스트에서 별도로 적용합니다. 이 앱에는 로그인이 없습니다. 공인망/인증 없는 리버스 프록시로 공개하지 마세요.
- LAN 모드에서는 localhost/127.0.0.1 외에 서버의 IPv4 주소, 호스트 이름, `<호스트 이름>.local`이 Host 허용 목록에 들어갑니다. Tailscale/별도 DNS 이름은 `GROK_BOT_ALLOWED_HOSTS=devbox.example.net,other.example.net`으로 추가합니다. URL·포트·경로·와일드카드는 쓰지 않습니다. 이름은 대소문자/마지막 마침표를 정규화합니다.
- 자동 IPv4 목록은 **프로세스 시작 시 한 번** 수집합니다. lingering 서비스가 DHCP보다 먼저 시작하거나 나중에 주소가 바뀌면 IP 주소 URL이 403이 될 수 있습니다. 부팅 순서에 의존하지 않도록 고정 DNS 이름으로 접속하고 그 이름을 설정에 명시하세요. IP URL이 필요하면 고정/예약한 IPv4도 `GROK_BOT_ALLOWED_HOSTS=devbox.example.net,192.168.1.50`처럼 명시합니다. 동적으로 새 주소를 자동 추가하지는 않습니다.
- `GROK_BOT_ALLOWED_HOSTS`는 **HTTP Host 헤더 검증**입니다. 클라이언트 IP 허용 목록, 인증, 방화벽, DNS 등록 또는 리스닝 인터페이스 설정이 아닙니다. 브라우저의 동일 출처·포트 검사는 그대로 유지됩니다.
- 관리형 컴퓨터 제어와 maintenance는 loopback 소켓만 허용합니다. 전달 헤더는 로컬 권한을 주지 않습니다. 로컬에 연결하는 프록시로 LAN 요청을 전달하면 이 경계가 사라질 수 있으므로 그런 프록시로 서비스를 공개하지 마세요.
- 설정 변경은 다음 시작에 적용됩니다. `node scripts/service.mjs update`는 실행 중인 포트로 maintenance에 접속하므로 `PORT` 변경은 답변 종료 후 수동 stop → 환경 수정 → start로 적용합니다. 기본값 7777을 권장합니다. 브라우저/Fetch가 금지하는 포트(예: 6000)는 숫자 범위가 유효해도 접속·업데이트 확인에 사용할 수 없습니다.

LAN 사용 예시(환경 파일):

```ini
HOST=0.0.0.0
PORT=7777
GROK_BOT_ALLOWED_HOSTS=devbox.example.net
```

접속 주소는 `http://<서버 LAN IPv4>:7777` 또는 `http://devbox.example.net:7777`입니다. 포트 7777은 서비스의 기본값일 뿐 앱 자체의 기본 포트 3000은 변경하지 않습니다.
