# wsx

Git worktree를 위한 프로젝트 중심 터미널 워크스페이스 관리자입니다.

wsx는 **Project → Worktree → Session → Pane** 구조를 키보드 중심 TUI로 표시합니다. Session은 작업 컨텍스트로 항상 보이며, 여러 pane이 있을 때만 하위 pane 행을 표시합니다. 인접한 `wsxd` daemon이 PTY와 고정된 `libghostty-vt` 상태를 소유하므로 client가 종료되어도 터미널은 계속 실행됩니다.

## 주요 기능

- Git project/worktree 검색, 생성, 삭제, 정리, 상태, alias, project group
- 영속 session과 선택적 가로/세로 pane 분할
- application cursor shape, style, resize, keyboard, mouse를 지원하는 실제 터미널 viewport
- 탐색용 Workspace mode와 직접 입력용 Terminal mode
- pane마다 하나의 명시적 쓰기 lease와 명시적 takeover
- typed/versioned/bounded 동일 사용자 local protocol과 authoritative snapshot
- provider-neutral agent 상태 및 capability
- bounded manifest를 사용하는 신뢰된 executable plugin
- 기존 asched 경계를 통한 project routine
- macOS와 Linux 지원

## 설치

Release archive는 인접한 `wsx`, `wsxd` executable을 포함합니다. Homebrew formula는 배포 전에 0.20 archive로 갱신해야 합니다.

소스 빌드에는 Rust 1.96.1과 Zig 0.15가 필요합니다. 개발 test suite를 실행하려면 `cargo-nextest` 0.9.143을 설치합니다.

```bash
cargo install cargo-nextest --version 0.9.143 --locked
```

```bash
git clone https://github.com/vlwkaos/wsx.git
cd wsx
cargo +1.96.1 build --workspace --locked
cargo xtask run
cargo xtask build  # target/wsx-dev/{wsx,wsxd}
```

TUI는 시작할 때 설치된 agent CLI 중 wsx integration이 없거나 오래된 항목을 감지하고 설치 여부를 묻습니다. 거절하면 다음 wsx version까지 다시 묻지 않습니다. 개별 integration은 직접 설치할 수도 있습니다.

```bash
wsx agent install pi
wsx agent install claude
```

설치 후 해당 agent를 다시 시작합니다. Installer는 각 agent의 표준 config directory override를 따르며 관련 없는 설정을 보존합니다. 최신 integration은 provider의 native session ID 또는 path도 보고합니다. wsxd가 완전히 재시작되면 wsx는 새 process에서 provider별 resume command를 실행해 보고된 conversation을 이어갑니다. Reference가 없으면 저장된 generic launch recipe를 사용합니다. Unsupported, malformed, duplicate reference는 새 agent conversation을 시작하지 않고 clean shell을 엽니다.

## 조작

| Context | Key |
|---|---|
| Workspace | `j/k` 이동, `h/l` 접기/펼치기, `Enter` 선택, `i` idle, `a` active, `n` attention session으로 이동 |
| Project | `p` project 추가, `w` worktree 추가, `u` routine 추가, `e` project config 보기/편집, `g` group 지정 |
| Worktree | `s` session 추가, `r` alias, `d` 삭제 |
| Session/Pane | `Enter` Terminal mode, `C` interrupt |
| Pane | `|` 오른쪽 분할, `-` 아래 분할, `d` 닫기 |
| Group | `T` group 열기, `{`/`}` 전환, `g` 선택 project 지정 |
| Global | `/` 검색, `,` global config 편집, `R` 새로고침, `?` 도움말, `q` TUI 종료, `Q` 확인 후 wsxd까지 종료 |

Group은 순서와 이름이 저장되는 project filter이며, 한 project가 여러 group에 속할 수 있습니다. Workspace filter는 한 번에 하나만 선택하며, 선택하지 않으면 모든 project를 표시합니다. 가상 **ungrouped** group은 membership이 없는 project를 표시합니다. **◷ recent**는 최근 24시간 안에 authoritative agent `working` report, session 생성, 또는 terminal session 진입이 있었던 project를 표시합니다. Recent에서 project row에 `d`를 누르면 project 등록은 유지한 채 다음 qualifying touch 전까지 Recent에서만 제거합니다.

Group chip은 Workspace와 Terminal mode 모두에서 유지되는 전체 너비의 한 줄 header에 표시됩니다. Workspace content는 바로 아래에서 시작하고, Terminal mode에서는 기존 breadcrumb가 다음 content row를 차지합니다. 한 줄을 넘으면 `+N` 대신 클릭 가능한 `‹`/`›`와 mouse wheel로 chip 단위 수평 scroll을 합니다. Project assignment mode에서는 여러 membership을 계속 toggle할 수 있으며, 왼쪽 sidebar row가 넘치면 오른쪽 가장자리에 scrollbar가 표시됩니다.

`wsx group ls|create|rename|add|remove`로 group을 관리합니다. status, worktree list, session list는 하나의 `--group <name>`을 받습니다. TUI는 시작할 때 Recent를 선택하며, 실행 중에는 다른 group으로 전환할 수 있습니다. 기존 tab config와 임시 multi-selection cache data는 한 번 migration한 뒤 다시 저장하며, tab command와 flag는 제거되었습니다. Recent는 trusted `wsx agent report` 입력도 사용하지만, wsx는 process나 terminal output으로 vendor 또는 semantic activity를 추론하지 않습니다.

Session row는 상태를 icon으로 표시하고 adapter가 보고한 agent 이름을 괄호 안에 덧붙입니다. `○` idle, `◐` working, `×` blocked, `✓` done, `!` error, `·` unknown, `⊘` muted입니다. 일반 shell에는 agent label을 표시하지 않습니다. Terminal header는 `project › worktree › session`, 상태 icon, 알려진 agent, 감지된 TCP listener를 표시합니다. Worktree preview는 session port를 합산합니다. Port 감지는 best effort이며 macOS/Linux에서 `lsof`가 필요합니다.

왼쪽 아래 status badge는 navigation, Terminal, input, confirmation, configuration, move, information, routine mode를 semantic background-color family로 구분합니다. Terminal mode에서는 persistent stream으로 일반 keyboard와 mouse 입력을 PTY로 전달하며 한 줄 breadcrumb 아래의 오른쪽 panel을 wsx padding 없이 사용합니다. 왼쪽 panel을 클릭하면 Workspace mode로 돌아가면서 해당 row를 선택합니다. `Ctrl+A`를 누른 다음 `W`를 누르면 Workspace로 focus가 이동하며, Control을 계속 누른 상태의 `W`도 동작합니다. `Ctrl+A` 다음 `Q`를 누르면 wsxd session은 유지한 채 TUI만 종료합니다. 같은 sequence는 Workspace에서도 동작하며, Workspace에서는 prefix 없는 `q`도 종료합니다. Prefix 없는 Terminal `q`는 계속 terminal로 전달합니다. `Ctrl+A Ctrl+A`는 literal `Ctrl+A`를 보내며, 다른 suffix는 prefix와 함께 terminal로 전달합니다. Footer hint는 `(Ctrl+A W)workspace  (Ctrl+A Q)quit` 형식을 사용합니다. 기본 terminal background는 투명하게 유지하고 application이 지정한 ANSI cell background는 보존합니다. Vim 같은 application이 요청한 block, underline, bar cursor shape도 반영합니다.

Linux에서는 `~/.config/wsx/config-v2.toml`, macOS에서는 platform-equivalent wsx config directory에서 escape sequence를 설정할 수 있습니다.

```toml
terminal_escape_chord = "ctrl+a w"
resume_agents_on_restore = true
```

Native agent conversation 복원은 기본으로 활성화됩니다. wsxd restart 후 generic saved launch recipe를 유지하려면 `resume_agents_on_restore = false`로 설정합니다. Global config가 malformed이거나 읽을 수 없으면 해당 startup에서는 복원을 비활성화합니다. 변경은 wsxd를 다시 시작한 뒤 적용됩니다.

한 개의 modified chord 설정은 Workspace focus 용도로 계속 지원하지만 별도의 prefixed quit는 제공하지 않습니다. 두 chord 설정에서는 suffix `q`를 TUI quit 용도로 예약하므로 Workspace-focus suffix로 설정할 수 없습니다. Workspace에서 `,`를 누르면 `$EDITOR`로 global config를 엽니다. Editor가 닫히면 config를 검증하며, 유효한 변경은 다음 실행부터 적용됩니다. 0.20을 처음 실행하면 legacy `config.toml`의 tab/group을 `config-v2.toml`로, `workspace.toml` UI state를 `workspace-v2.toml`로 한 번 import합니다. 기존 파일은 수정하지 않으므로 wsx 0.17이 0.20 state를 덮어쓸 수 없습니다.

## Project config

Project root의 기본 설정 파일은 `wsx.config.yml`입니다.

```yaml
hooks:
  postCreate: cargo build
copy:
  include:
    - .env.example
  exclude:
    - target
git:
  subtrees:
    - vendor/asched
    - vendor/herdr
```

`.gtrconfig`만 있으면 wsx가 legacy 값을 읽고 같은 내용의 `wsx.config.yml`을 atomic하게 생성합니다. 기존 `.gtrconfig`는 검토 후 직접 삭제할 수 있도록 남겨 둡니다. `wsx.config.yml`이 이미 있으면 항상 그 파일을 사용합니다. 64 KiB를 넘거나 malformed/unknown field가 있거나 normalized relative path가 아닌 subtree를 포함한 YAML은 거부합니다. Project에서 `e`를 눌러 config를 확인하고 다시 `e`를 누르면 편집할 수 있습니다. Viewer를 여는 것만으로는 파일을 만들지 않으며, 실제 편집을 시작할 때만 누락되었거나 비어 있는 canonical file을 유효한 schema template으로 초기화합니다.

Worktree preview는 Git submodule을 자동으로 발견해 별도 **Submodules** section에 표시합니다. 각 row는 checkout commit이 parent gitlink와 일치하는지, initialize/conflict 상태인지, modified 또는 untracked content가 있는지를 보여 줍니다. 이 검사는 local-only이며 submodule network fetch를 실행하지 않습니다. Git subtree에는 authoritative persistent registry가 없으므로 `git.subtrees`에 normalized relative root를 명시합니다. wsx는 해당 root의 local change를 일반 modified file과 분리해 **Subtrees** section에 표시합니다.

## CLI

```bash
wsx status [--json]
wsx worktree list|create|delete
wsx session list
wsx session send-keys <session-or-pane> <keys>
wsx session send-text <session-or-pane> <text>
wsx session prompt <session-or-pane> <prompt>
wsx session peek <session-or-pane> [-n VISIBLE_LINES] [--trim]
wsx session rename <session-id> <label>
wsx agent install <target>
wsx agent report <pane> --provider <name> --state <state> [--session-id <id>|--session-path <path>] [capabilities]
wsx plugin list [--json]
wsx plugin reload
wsx runtime status [--json]
wsx daemon stop
wsx routine ...
```

`wsx runtime status`와 `wsx daemon stop`은 daemon을 시작하지 않습니다. Workspace에서 `Q`를 누르면 확인 후 wsxd와 모든 live PTY를 정상 종료하고 TUI를 끝냅니다. 일반 wsx 시작은 호환되는 실행 중 daemon을 재사용합니다. local protocol이 바뀌었으면 wsx가 정상 종료를 요청하고 cleanup을 기다린 뒤 인접 executable 또는 `PATH`의 `wsxd`를 시작합니다. cross-version process handoff는 지원하지 않으므로 기존 PTY는 종료되지만, 새 daemon은 wsx session과 pane identity를 유지하고 adapter가 보고한 eligible agent conversation을 resume하며, 그 외에는 저장된 launch command로 process를 다시 생성합니다. `WSX_DAEMON_BIN`, `WSX_SOCKET`은 신뢰된 동일 사용자 override입니다.

## Plugin과 agent

Owner가 관리하는 JSON manifest를 `~/.config/wsx/plugins/`에 둡니다. Manifest는 API version `1`, stable ID, executable argv, event 목록, enabled 상태를 선언합니다. wsxd는 symlink, 잘못된 owner/permission, oversized manifest, invalid token, non-executable command를 거부합니다. Plugin payload와 실행 시간은 제한됩니다.

Agent integration은 `unknown`, `idle`, `working`, `blocked`, `done`, `error` 중 하나와 capability를 보고합니다. 지원 target은 `pi`, `omp`, `claude`, `codex`, `copilot`, `devin`, `droid`, `kimi`, `opencode`, `kilo`, `hermes`, `qodercli`, `qwen`, `cursor`, `mastracode`, `antigravity-cli`, `grok`입니다. Pi, OMP, Kimi, OpenCode, Kilo, MastraCode는 authoritative lifecycle state를 제공합니다. 나머지 hook은 authoritative agent/session identity와 unknown state를 제공합니다. Provider-specific metadata와 conversation 처리는 adapter가 소유합니다. wsx는 terminal motion이나 process tree로 agent 상태를 추측하지 않습니다.

## Runtime과 보안

- Socket과 state directory는 owner-only입니다.
- Terminal pane마다 writable client lease는 하나입니다. 다른 client는 takeover를 명시해야 합니다.
- Event는 revision을 invalidate하며 authoritative snapshot으로 복구합니다.
- Message, frame, command, plugin, resource count는 bounded입니다.
- Terminal frame은 Ghostty wide/spacer occupancy를 보존하고 첫 baseline 이후 synchronized-output 중간 frame을 억제하며 subscribe viewport를 baseline에 적용합니다. Workspace metadata refresh는 수락된 terminal surface를 소유하거나 지우지 않습니다.
- wsxd는 project, worktree, session, pane, terminal, known-agent identity와 검증된 native session reference를 저장합니다. daemon restart 후 eligible agent는 `codex resume <id>`, `pi --session <path>` 같은 direct vendor argv로 conversation을 resume하며 lifecycle state는 adapter가 다시 보고할 때까지 unknown입니다.
- Native resume은 항상 새 process, PTY, terminal buffer를 생성합니다. wsxd supervisor가 provider를 direct argv로 실행하고 provider가 끝나면 fresh shell을 엽니다. 임의 shell process, terminal history, lease, unsupported agent conversation은 보존하지 않습니다.
- Remote access, live daemon handoff, graphics transport, marketplace, original-process restoration은 아직 지원하지 않습니다.

## 개발

```bash
cargo check --workspace --all-targets --locked
cargo nextest run --workspace --locked
cargo test --workspace --locked --doc
cargo clippy --workspace --all-targets --locked -- -D warnings
python3 scripts/runtime-smoke.py
```

Nextest는 각 test를 별도 process에서 실행합니다. Nextest는 doctest를 실행하지 않으므로 `cargo test --doc` 단계는 유지합니다.
