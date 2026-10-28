# Darak (다락)

Windows용 디스크 공간 시각화 도구입니다. 드라이브나 폴더를 빠르게 스캔하고, 폴더 안에 폴더가 겹쳐 들어간 트리맵으로 어디에 공간이 쓰였는지 보여 줍니다. 큰 항목에 삭제 표시를 하고, 검토한 뒤 휴지통으로 보내거나 영구 삭제할 수 있습니다.

<p align="center"><img src="crates/darak/assets/icon-256.png" width="96" alt="Darak 아이콘"></p>

## 기능

- **빠른 스캔**
  - 관리자 권한이 있으면 NTFS 드라이브 루트의 MFT를 직접 읽습니다.
  - 관리자 권한이 없으면 폴더를 병렬로 순회합니다.
  - MFT를 쓰지 못한 이유(일반 권한, exFAT, 네트워크 드라이브 등)를 상태 바에 보여 줍니다.
- **트리맵 탐색**
  - 클릭이나 휠로 확대·축소하고, 이동 경로와 뒤로·앞으로를 지원합니다.
  - 상세도는 5단계이고, 작은 항목은 '작은 항목'으로 묶어 보여 줍니다.
  - 파일 종류별로 차분한 범주 색을 씁니다.
- **검색**: 이름, 크기, 수정일 조건으로 찾습니다.
- **안전한 정리**
  - 삭제는 '표시 → 검토 → 실행' 순서로만 진행됩니다.
  - 드라이브 루트와 Windows 폴더는 안쪽까지 지울 수 없게 막습니다. Program Files·ProgramData·사용자 폴더는 폴더 자체만 막고, 관리자 권한에서는 그 바로 아래 폴더(설치된 앱, 다른 사용자 프로필과 기본 폴더)까지 막습니다.
  - 휴지통으로 보낼 수 없는 항목(휴지통이 없는 볼륨, 260자 이상 경로, 휴지통보다 큰 항목)은 미리 알리고 실행을 막습니다.
  - 실행 파일, 스크립트, 설치 파일은 더블클릭해도 실행하지 않고 탐색기에서 위치만 보여 줍니다.
- **스캔 중지**: 중지하면 이전 결과로 바로 돌아갑니다. 읽지 못한 폴더에는 ⚠를 붙여 크기가 실제보다 작을 수 있다고 알립니다.
- **한국어·영어**: 처음에는 Windows 표시 언어를 따르고(한국어가 아니면 영어), 툴바 오른쪽 🌐에서 바꿀 수 있습니다.

## 요구 사항

- Windows 10 이상 (Windows 전용)
- 빌드: Rust 1.99 이상 (edition 2024)

## 빌드와 실행

```powershell
cargo build --release -p darak
.\target\release\darak.exe            # 환영 화면에서 드라이브 선택
.\target\release\darak.exe D:\Games   # 경로를 바로 스캔
```

MFT 스캔을 쓰려면 앱 안의 '관리자로 실행'을 누르거나 관리자 권한으로 실행합니다.

직접 빌드하지 않아도 됩니다. GitHub Actions의 [Windows build](https://github.com/soira237-risu/darak/actions/workflows/build.yml) 실행 결과(Artifacts)에서 main의 최신 `darak.exe`를 받을 수 있습니다(GitHub 로그인 필요). 서명되지 않은 파일이라 처음 실행할 때 SmartScreen 경고가 뜰 수 있습니다.

## 키보드

| 키 | 동작 |
|---|---|
| Enter | 폴더 열기 / 파일 열기 |
| Backspace, Alt+↑ | 상위 폴더 |
| Alt+← / Alt+→ | 뒤로 / 앞으로 |
| Space, X, Delete | 삭제 표시 켜기·끄기 |
| `-` `[` / `+` `]` | 상세도 낮추기 / 높이기 |
| Ctrl+F, `/` | 검색 |
| F5 | 다시 스캔 |
| Esc | 선택 해제 |

## 명령줄 도구

`darak-cli`는 스캔 성능을 재고 결과를 확인하는 개발용 도구입니다.

```powershell
cargo run --release -p darak-cli -- C:\ --top 20
cargo run --release -p darak-cli -- C:\ --compare   # MFT와 일반 순회 비교 (관리자)
```

## 구조

```
crates/
  darak-core/   스캔(MFT·순회), 트리, 레이아웃, 검색, 삭제·보호 정책 (UI 없음)
  darak/        egui 앱: 트리맵, 검색 패널, 삭제 검토, 드라이브 정보
  darak-cli/    벤치마크·디버깅용 CLI
scripts/        CI 스모크 테스트 (성능 측정, 검색 비교)
tools/          아이콘 생성 스크립트
design/icon/    아이콘 원본
docs/PLAN.md    설계 문서와 진행 상황
```

## 개발

PR과 main 푸시마다 GitHub Actions가 아래 검사(`windows.yml`)와 릴리스 빌드(`build.yml`)를 돌립니다. 로컬에서 같은 검사를 하려면:

```powershell
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-features   # perf 기능 포함
```

## 주의

Darak은 파일을 지우는 도구입니다. 보증 없이 제공되며, 영구 삭제는 되돌릴 수 없습니다. 보호 규칙은 실수를 줄이는 장치일 뿐이니 지우기 전에 검토 화면을 꼭 확인하세요. 네트워크로 아무것도 보내지 않고 레지스트리 값을 쓰지 않습니다. 디스크에 남기는 설정은 🌐에서 고른 언어 하나뿐이며, 바꿀 때만 `%APPDATA%\Darak\settings.toml`에 씁니다.

## 라이선스

MIT. [LICENSE](LICENSE)를 보세요.
