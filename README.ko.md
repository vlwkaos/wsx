# wsx

[ENG](README.md) | **한국어**

git worktree와 tmux 세션을 위한 TUI 워크스페이스 관리자.

<!-- screenshot -->
![Screen Recording 2026-02-27 at 9 00 58 AM_1](https://github.com/user-attachments/assets/325dfaca-5f18-458b-944f-ce143e32cd51)

## 개요

프로젝트 → 워크트리 → tmux 세션을 사이드바에서 실시간으로 확인합니다.
각 세션의 상태가 실시간으로 표시되어, 들어가지 않아도 어디에 주의가 필요한지 알 수 있습니다.
`n`을 누르면 주의가 필요한 세션을 순회합니다.
내부적으로 tmux를 사용하며 세션을 나가는 detach 단축키는 `ctrl+a - d` 로 커스텀됩니다.

자세한 내용은 아래를 참고합니다.

```
▼ project
  ▾ main * ↑2
      ◉ wsx_cc_main
  ▸ feature-auth ↓1
      ○ wsx_cc_auth
```

```mermaid
flowchart LR
  P[프로젝트] --> W1[워크트리 main]
  P --> W2[워크트리 feature-auth]
  W1 --> S1[세션: nvim]
  W1 --> S2[세션: dev]
  W2 --> S3[세션: dev]
```

## 가이드

| 기능 | 스크린샷 |
|---|---|
| **프로젝트 설정** 저장소 루트의 `.gtrconfig` 파일을 생성하면 명세에 따라, 새 워크트리에 env 파일 자동 복사와 훅을 실행합니다. `e`로 수정 가능합니다. | <img width="473" height="245" alt="image" src="https://github.com/user-attachments/assets/41a1ef82-9ebb-49aa-993e-4ae9f1ea0a83" /> |
| **프로젝트 추가** `p`를 누르고 경로 입력하여 프로젝트를 추가할 수 있습니다. Tab 자동완성 지원. | <img width="457" height="221" alt="image" src="https://github.com/user-attachments/assets/b6c0c7bf-7252-4281-bee4-8dfa4c8d4529" /> |
| **새 워크트리** 프로젝트 선택 후 `w`, 브랜치 이름 입력하면 워크트리가 추가됩니다. `r` 입력으로 워크트리에 별명을 붙일 수 있습니다. | <img width="459" height="52" alt="image" src="https://github.com/user-attachments/assets/8280c712-29a1-43d6-8504-0c7161ab9b86" /> <img width="264" height="90" alt="image" src="https://github.com/user-attachments/assets/c8183cf6-4de8-414a-88e2-1ceac1722080" /> |
| **세션** 워크트리 선택 후 `s`. 용도별 이름 `shell`, `claude`, `build`을 지정할 수 있습니다. `d`로 삭제, `r`로 이름 변경이 가능합니다. | <img width="270" height="68" alt="image" src="https://github.com/user-attachments/assets/41569337-057f-44b8-bd39-8f1d2ffa6a1f" /> |
| **순회** `n` / `N`으로 응답이 필요한 `●` 세션을 순회합니다. 순회하고 싶지 않은 세션은 `x`로 해제하거나, 음소거 `⊘`시킬 수 있습니다. `a`로 뭔가 진행중인 `◉` 세션만도 순회가 가능합니다. | ![Screen Recording 2026-02-27 at 9 35 16 AM](https://github.com/user-attachments/assets/46c6b7be-34b2-4f73-b959-6205d81d1a66) |
| **원격 제어** `S`로 세션에 진입하지 않고도 명령어를 보낼 수 있습니다. `C`로 Ctrl+C 전송. 돌고있는 프로세스를 빠르게 중단할때 유용합니다. | <img width="464" height="57" alt="image" src="https://github.com/user-attachments/assets/6d466d85-4d92-44c7-abe8-93ec4337f480" /> |
| **복귀** 세션 안에서 `Ctrl+a d`로 wsx로 돌아갈 수 있습니다 | - |

## 설치

**macOS (Homebrew)**
```sh
brew tap vlwkaos/tap
brew install wsx
```

**macOS / Linux (cargo)**
```sh
cargo install wsx
```

**소스에서 빌드**
```sh
cargo install --path .
```

## 사용법

```sh
wsx
```

### 탐색

| 키 | 동작 |
|-----|--------|
| `j/k` `↑/↓` | 커서 이동 |
| `h/l` `←/→` | 접기 / 펼치기 |
| `Enter` | 펼치기 · 세션 접속 |
| `[` / `]` | 이전 / 다음 프로젝트로 이동 |
| `a` | 다음 활성 세션 `◉` |
| `n` / `N` | 다음 / 이전 대기 세션 `●` |
| `x` | 해제 · 음소거 |
| `/` | 검색 |
| `?` | 전체 키 목록 |

마우스 클릭 지원: 행 클릭으로 선택, 미리보기 클릭으로 접속.

### 워크스페이스

| 키 | 동작 |
|-----|--------|
| `p` | 프로젝트 추가 |
| `w` | 새 워크트리 |
| `s` | 새 세션 |
| `m` | 프로젝트 또는 세션 순서 변경 |
| `r` | 별칭 설정 |
| `d` | 삭제 |
| `g` | Git 팝업 (pull / push / rebase / merge) |
| `c` | 병합된 워크트리 정리 |
| `e` | `.gtrconfig` 보기 |
| `S` | 세션에 명령 전송 |
| `C` | 세션에 Ctrl+C 전송 |

### tmux 설정

별도의 tmux 설정 파일이 없는 경우:
- prefix를 `ctrl+a`로 설정합니다.
- 접속 시 `status-right`를 `project/worktree`로 설정합니다. `~/.tmux.conf` 커스텀:
- 마우스 설정 활성화

```
set -g status-right "#{@wsx_project}/#{@wsx_alias}"
```

## 모바일 / SSH

```sh
wsx --mobile
```

미리보기 패널을 숨기고 간결한 키 힌트를 표시합니다 — 세로 모드 SSH 환경(폰, 좁은 터미널)에 적합합니다. `mobile_detach_key` 설정으로 no-prefix tmux detach 단축키를 지정할 수 있습니다:

```toml
# ~/.config/wsx/config.toml
mobile_detach_key = "C-q"
```

## CLI

### 머신 로컬 루틴

`wsx routine`은 분리 실행되는 단일 데몬을 통해 프로젝트별 direct-argv 예약 명령을 관리합니다. 정의는 git 밖에 저장하고 canonical main worktree에서 실행합니다.

```sh
wsx routine add nightly --cron "0 2 * * *" --arg codex --arg exec --arg=--json --arg '{prompt}' --prompt "유지보수를 실행해 줘" -p wsx
wsx routine list -p wsx
wsx routine run nightly -p wsx
wsx routine logs nightly -p wsx
wsx routine daemon status
```

cron은 데몬 호스트 로컬 시간의 숫자 5필드이며 `*`, 쉼표 목록, 포함 범위, 양수 `/step`을 지원합니다. 일요일은 `0` 또는 `7`이고 일과 요일을 모두 제한하면 일반 cron처럼 OR로 판정합니다. 놓친 분은 재실행하지 않으며 실행 전 epoch-minute claim을 영속화하고 같은 루틴의 동시 실행을 막습니다.

설정은 `~/.config/wsx/routines/projects/` 아래 canonical 프로젝트별 버전 TOML입니다. 안정적인 FNV-1a-128 파일 키와 저장된 전체 경로를 대조해 충돌을 거부합니다. claim과 최근 20개 실행 로그는 별도 저장합니다. 정확히 일치하는 `{prompt}` argv는 치환하고 없으면 stdin으로 전달합니다. 원본 stdout/stderr와 Codex/Claude 최종 응답 추출 결과를 보존하며 `--revision`으로 stale write를 거부할 수 있습니다.

```sh
# 워크트리
wsx worktree create <branch> [-p <project>]
wsx worktree delete <branch> [-p <project>]
wsx worktree list  [-p <project>] [--json]

# 세션
wsx session send-keys <session> <keys>
wsx session peek <session> [-n <lines>] [-o <offset>] [-a]
wsx session rename <old> <new>
wsx session list   [-p <project>] [--json]

# 상태
wsx status [--json]
```

`peek`은 pane 출력을 캡처합니다. `-n`은 스크롤백 줄 수(기본값: 현재 화면), `-o`는 아래에서 건너뛸 줄 수, `-a`는 ANSI/장식 문자 제거(에이전트/LLM 입력용).

## 설정

전역 설정: `~/.config/wsx/config.toml`. 프로젝트별 설정은 `e` 키로 확인.

### .gtrconfig 명세

워크 트리 생성시 사용

```ini
[hooks]
  postCreate = npm install

[copy]
  include = .env
  include = .env.local
  exclude = .env.production
```

## 영감

- [git-worktree-runner](https://github.com/coderabbitai/git-worktree-runner)
- [agent-of-empires](https://github.com/njbrake/agent-of-empires)
