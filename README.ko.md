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
| Workspace | `j/k` 이동, `h/l` 접기/펼치기, `Enter` 선택, `m`으로 선택한 project/session 순서 변경, `i/I` idle, `a/A` active, `n/N` attention iteration |
| Project | `p` project 추가, `w` worktree 추가, `u` routine 추가, `e` project config 보기/편집, `g` group 지정 |
| Worktree | `s` session 추가, `r` alias, `d` 삭제 |
| Session/Pane | `Enter` Terminal mode, `x`로 done 확인 또는 mute 전환, `C` interrupt |
| Pane | `|` 오른쪽 분할, `-` 아래 분할, `d` 닫기 |
| Group | `T` group 열기, `{`/`}` 전환, `g` 선택 project 지정 |
| Global | `/` 검색, `,` global config 편집, `R` 새로고침, `?` 도움말, `q` TUI 종료, `Q` 확인 후 wsxd까지 종료 |

`u`를 눌러 routine을 만들 때 `j/k`로 **Codex**, **Claude**, **Pi**, **Custom** runner를 명시적으로 선택한 다음 생성된 argv, schedule, prompt를 편집합니다. Picker는 function key를 예약하지 않습니다. Custom은 빈 argv로 시작하며 command를 입력하기 전에는 저장할 수 없습니다.

Group은 순서와 이름이 저장되는 project filter이며, 한 project가 여러 group에 속할 수 있습니다. Workspace는 항상 group filter 하나를 적용합니다. 가상 **ungrouped** group은 첫 번째 기본 anti-group이며 membership이 없는 project를 표시합니다. Workspace는 마지막으로 선택한 유효한 group을 복원하고, 저장값이 없거나 malformed이거나 rename/delete된 경우 **ungrouped**를 선택합니다. 설정한 inactivity window 안에 authoritative agent `working` report나 terminal 진입이 하나도 없으면 project는 **stale** 상태입니다. Stale project는 계속 표시하지만 자동으로 접고, 읽기 쉬운 muted marker로 구분합니다. 한 번 펼치면 현재 wsx process에서는 fresh 상태를 유지하며, 이 override는 wsx가 종료되면 reset됩니다.

Group chip은 Workspace와 Terminal mode 모두에서 유지되는 전체 너비의 한 줄 header에 표시됩니다. Workspace content는 바로 아래에서 시작하고, Terminal mode에서는 기존 breadcrumb가 다음 content row를 차지합니다. 한 줄을 넘으면 `+N` 대신 클릭 가능한 `‹`/`›`와 mouse wheel로 chip 단위 수평 scroll을 합니다. Mouse wheel은 보이는 selection을 유지하면서 Workspace tree를 rendered row 3개씩 scroll합니다. Project assignment mode에서는 여러 membership을 계속 toggle할 수 있으며, 왼쪽 sidebar row가 넘치면 오른쪽 가장자리에 scrollbar가 표시됩니다.

`wsx group ls|create|rename|add|remove`로 group을 관리합니다. status, worktree list, session list는 하나의 `--group <name>`을 받습니다. TUI는 group 선택을 일반 workspace cache와 분리해 저장하므로 오래된 client의 관련 없는 save나 종료가 메모리에 남은 선택을 복원하지 못합니다. 기존 tab config와 historical selector field는 계속 무시하며 tab command와 flag는 제거되었습니다. Stale 판정은 trusted `wsx agent report`와 terminal-entry activity를 사용하지만, wsx는 process나 terminal output으로 vendor 또는 semantic activity를 추론하지 않습니다.

Session row는 상태를 icon으로 표시하고 adapter가 보고한 agent 이름을 괄호 안에 덧붙입니다. Authoritative agent working 상태는 연한 녹색 `◎ ◉ ● ◉` pulse로 움직이며, 노란색 `○`는 idle, 빨간색 `◐`는 blocked, 녹색 `✓`는 done, `!`는 error, `·`는 unknown, `⊘`는 muted입니다. Done은 `x`로 dismiss하거나 명시적인 terminal 진입, 입력, interrupt, rename으로 해당 report revision을 확인할 때까지 needs-attention에 남고, 확인 후 daemon의 authoritative state를 바꾸지 않은 채 idle로 표시됩니다. Watch, build, development server처럼 별도 PTY foreground job을 실행하는 일반 shell은 agent 상태로 추론하지 않고 고정된 연한 녹색 `●`로 표시합니다. 일반 shell에는 agent label을 표시하지 않습니다. 감지된 TCP listener는 각 Workspace session row의 오른쪽 끝에 정렬합니다. Terminal header는 `project › worktree › session`, 상태 icon, 알려진 agent, 감지된 TCP listener를 표시합니다. Worktree preview는 session port를 합산합니다. Port 감지는 best effort이며 고유한 controlling TTY를 가진 descendant process group을 지원하고 macOS/Linux에서 `lsof`가 필요합니다.

왼쪽 아래 status badge는 navigation, Terminal, input, confirmation, configuration, move, information, routine mode를 semantic background-color family로 구분합니다. Terminal mode에서는 persistent stream으로 일반 keyboard와 mouse 입력을 PTY로 전달합니다. Terminal output을 drag하면 text를 선택해 OSC 52로 복사하며, double-click은 word를, triple-click은 line을 선택합니다. Mouse reporting을 활성화한 application은 일반 pointer event를 계속 받고, Shift-drag는 local selection을 강제합니다. Selection은 controller-local 상태이며 viewport 이동, resize, primary/alternate screen 전환, stream loss, lease handoff 때 clear됩니다. Terminal application이 mouse reporting 또는 alternate-scroll behavior를 활성화하지 않으면 wheel 입력은 Ghostty history를 3줄씩 local scroll합니다. Attached application에서 수락한 standard text clipboard write는 순서대로 OSC 52를 통해 outer terminal에 전달합니다. wsx는 write당 192 KiB, pending FIFO 64개로 제한하며, overflow는 수락한 effect를 덮어쓰지 않고 거부합니다. wsxd는 frame 생성을 8 ms cadence로 coalesce하지만 clipboard와 control effect는 이 cadence를 우회합니다. System resume 후 wsx는 이전 presentation과 stream lease를 폐기하고 authoritative snapshot으로 terminal identity를 확인한 다음 full baseline부터 다시 연결합니다. Debug `wsx`와 `wsxd`를 build한 뒤 `python3 scripts/terminal-latency.py`를 실행하면 narrow, wide, erase-rewrite update의 direct PTY 대비 full-path p50/p95 latency를 출력하며, added p95가 16.7 ms frame budget에 도달하면 실패합니다. Terminal은 한 줄 breadcrumb 아래의 오른쪽 panel을 wsx padding 없이 사용합니다. Desktop Terminal mode에서는 기본적으로 왼쪽 tree를 2-column status rail로 접습니다. Session row는 authoritative state glyph를 유지하고 다른 row type은 compact identity 또는 state glyph를 표시하며, selection과 scrolling은 기존 row 좌표를 유지합니다. Rail을 클릭하면 Workspace mode로 돌아가면서 해당 row를 선택합니다. 전체 32-column tree를 유지하려면 `terminal_sidebar = "expanded"`로 설정합니다. Terminal mode에서 설정한 prefix(기본값 `Ctrl+A`) 다음 `j` 또는 `Down`을 누르면 현재 worktree의 다음 session으로, `k` 또는 `Up`을 누르면 이전 session으로, `n` 또는 `N`을 누르면 active group에서 attention이 필요한 다음 또는 이전 session으로 전환합니다. Target session은 resize된 baseline이 준비된 후 Terminal mode에서 바로 열립니다. Desktop에서는 prefix 다음 `B`로 저장된 설정을 바꾸지 않고 현재 TUI 실행 동안 compact와 expanded를 전환합니다. Mobile Terminal mode는 sidebar와 sidebar toggle 없이 전체 너비를 유지합니다. Terminal footer는 lowercase이고 실제 입력 case와 일치하는 prefix command를 항상 표시합니다. Prefix 입력 대기 중에는 prefix hint를 accent color로 표시하고 `(esc)cancel`을 추가하며, Escape는 PTY로 전달하지 않고 sequence를 취소합니다. Prefix 다음 설정된 Workspace suffix(기본값 `W`)를 누르면 Workspace로 이동합니다. `Ctrl+A` 다음 `Q`를 누르면 wsxd session은 유지한 채 TUI만 종료합니다. 같은 sequence는 Workspace에서도 동작하며, Workspace에서는 prefix 없는 `q`도 종료합니다. Prefix 없는 Terminal `q`는 계속 terminal로 전달합니다. `Ctrl+A Ctrl+A`는 literal `Ctrl+A`를 보내며, 다른 suffix는 prefix와 함께 terminal로 전달합니다. Footer hint는 `(Ctrl+A W)workspace  (Ctrl+A Q)quit` 형식을 사용합니다. Workspace terminal preview는 panel보다 짧은 frame을 위쪽에 정렬하고 더 큰 frame에서는 오래된 위쪽 row를 잘라 자연스러운 shell 배치와 최신 출력을 보존합니다. Terminal 진입 시 stream이 정확한 resized full baseline을 수락할 때까지 Workspace를 표시합니다. 기본 terminal background는 투명하게 유지하고 application이 지정한 ANSI cell background는 보존합니다. Vim 같은 application이 요청한 block, underline, bar cursor shape도 반영합니다.

Linux에서는 `~/.config/wsx/config-v2.toml`, macOS에서는 platform-equivalent wsx config directory에서 escape sequence를 설정할 수 있습니다.

```toml
terminal_escape_chord = "ctrl+a w"
resume_agents_on_restore = true
wake_mode = true
auto_collapse_after_hours = 24
notification_timeout_seconds = 4
show_release_status = true
terminal_sidebar = "compact"
port_visibility = "non_agentic"
```

Native agent conversation 복원은 기본으로 활성화됩니다. wsxd restart 후 generic saved launch recipe를 유지하려면 `resume_agents_on_restore = false`로 설정합니다. macOS에서는 `wake_mode = true`(기본값)가 live generation-authorized agent가 `working`을 보고하는 동안 bounded `caffeinate` assertion을 사용합니다. Runtime settings 또는 `wake_mode = false`로 끌 수 있으며 footer version 앞의 흐린 `☕`는 wake mode 설정이 켜졌음을 나타냅니다. `auto_collapse_after_hours`의 기본값은 `24`이며, `0`으로 설정하면 project 자동 접기를 비활성화합니다. `notification_timeout_seconds`의 기본값은 `4`이고 success, warning, error notice에 적용되며 최소값은 `1`입니다. Footer version/update status를 숨기려면 `show_release_status = false`로 설정합니다. `terminal_sidebar`는 `compact`(기본값)와 `expanded`를 지원하며 desktop Terminal sidebar를 제어하고 wsxd restart 없이 다음 render에 적용됩니다. `port_visibility`는 `hidden`, `non_agentic`(기본값), `all`을 지원하며 session row와 terminal breadcrumb에 적용됩니다. Branch detail은 설정과 관계없이 port를 표시합니다. `,`로 여는 global settings의 Terminal section은 prefix modifier, prefix key, Workspace suffix를 분리된 validated control로 편집하면서 기존 `terminal_escape_chord = "ctrl+a w"` TOML 형식을 유지합니다. Runtime connection banner는 해당 상태가 지속되는 동안 계속 표시됩니다. Global config가 malformed이거나 읽을 수 없으면 해당 startup에서는 복원을 비활성화합니다.

한 개의 modified chord 설정은 Workspace focus 용도로 계속 지원하지만 별도의 prefixed quit는 제공하지 않습니다. 두 chord 설정에서는 suffix `b`, `j`, `k`, `n`, `q`를 Terminal command에 예약하므로 Workspace-focus suffix로 설정할 수 없습니다. Workspace에서 `,`를 누르면 category별 global settings view를 엽니다. Toggle, choice, number, text, editable-list control을 제공하며, 변경하지 않은 draft에서 `e`를 누르면 raw TOML을 열 수 있습니다. 저장한 TUI presentation setting은 즉시 적용되고 wake mode는 몇 초 안에 실행 중인 daemon에 반영되며 그 밖의 daemon startup 동작은 wsxd restart 후 적용됩니다. 0.20을 처음 실행하면 legacy `config.toml`의 tab/group을 `config-v2.toml`로, `workspace.toml` UI state를 `workspace-v2.toml`로 한 번 import합니다. 기존 파일은 수정하지 않으므로 wsx 0.17이 0.20 state를 덮어쓸 수 없습니다.

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
wsx daemon recover
wsx routine ...
```

Agent도 사용자와 같은 validated CLI contract를 통해 routine을 만들 수 있습니다. 예를 들어 평일 09:00에 실행하는 Pi routine은 다음과 같습니다.

```bash
wsx routine add weekday-review \
  --cron "0 9 * * 1-5" \
  --arg=pi --arg=-p --arg="{prompt}" \
  --prompt "Project를 검토하고 실행 가능한 문제를 보고해"
```

각 `--arg`는 direct argv item 하나이며 wsx는 shell을 실행하지 않습니다. 신뢰하지 않는 command를 enable 또는 run하기 전에 `wsx routine show weekday-review`로 결과를 확인해야 합니다.

`wsx runtime status`와 `wsx daemon stop`은 daemon을 시작하지 않습니다. `wsx daemon recover`는 shared crash budget을 명시적으로 초기화하고 wsxd를 시작합니다. Workspace에서 `Q`를 누르면 확인 후 wsxd와 모든 live PTY를 정상 종료하고 TUI를 끝냅니다. 일반 startup은 owner-only coordinator lock으로 여러 client의 probe, recovery, replacement, spawn, readiness를 직렬화하고 daemon lock을 최종 single-owner fence로 유지합니다. 사라진 daemon은 60초 동안 최대 세 번 자동 재시작합니다. 인증된 same-login daemon이 응답하지 않거나 unsafe data를 반환하면 자동 종료하지 않고 문제를 보고합니다. Intentional stop과 login 종료 marker는 background client가 해당 경계를 되돌리지 못하게 합니다.

macOS에서는 kernel에서 연결된 daemon의 owner와 audit session을 확인합니다. 이전 login이 남긴 daemon은 자동으로 정상 종료한 뒤 저장된 session을 현재 login security context에서 다시 시작합니다. Lifecycle-capable daemon은 protocol version이 달라도 startup binary identity를 알립니다. 새 build가 발견되면 기존 daemon과 live PTY를 계속 사용하고 live runtime이 모두 끝날 때까지 replacement를 연기합니다. 이후 daemon이 스스로 종료되며 다음 elected client가 인접한 fixed daemon을 시작합니다. Pre-lifecycle compatible daemon은 다음 자연스러운 stop까지 유지됩니다. Pre-lifecycle incompatible daemon은 보호되며 matching wsx binary 또는 명시적인 `wsx daemon stop`이 필요합니다. Cross-version process handoff는 지원하지 않으므로 crash나 replacement 후에는 저장된 recipe에서 새 PTY와 terminal buffer를 만들되 wsx identity는 유지합니다. `WSX_DAEMON_BIN`, `WSX_SOCKET`은 신뢰된 동일 사용자 override입니다.

## Plugin과 agent

Owner가 관리하는 JSON manifest를 `~/.config/wsx/plugins/`에 둡니다. Manifest는 API version `1`, stable ID, executable argv, event 목록, enabled 상태를 선언합니다. wsxd는 symlink, 잘못된 owner/permission, oversized manifest, invalid token, non-executable command를 거부합니다. Plugin payload와 실행 시간은 제한됩니다.

Agent integration은 `unknown`, `idle`, `working`, `blocked`, `done`, `error` 중 하나와 capability를 보고합니다. 지원 target은 `pi`, `omp`, `claude`, `codex`, `copilot`, `devin`, `droid`, `kimi`, `opencode`, `kilo`, `hermes`, `qodercli`, `qwen`, `cursor`, `mastracode`, `antigravity-cli`, `grok`입니다. Pi, OMP, Claude, Kimi, OpenCode, Kilo, MastraCode는 authoritative lifecycle state를 제공합니다. 나머지 hook은 authoritative agent/session identity와 unknown state를 제공합니다. Provider-specific metadata와 conversation 처리는 adapter가 소유합니다. wsx는 terminal motion이나 process tree로 agent 상태를 추측하지 않습니다.

## Runtime과 보안

- Socket과 state directory는 owner-only입니다.
- Terminal pane마다 writable client lease는 하나입니다. 다른 client는 takeover를 명시해야 합니다.
- Event는 revision을 invalidate하며 authoritative snapshot으로 복구합니다.
- Message, frame, command, plugin, resource count는 bounded입니다.
- Terminal frame은 Ghostty wide/spacer occupancy를 보존하고 첫 baseline 이후 synchronized-output 중간 frame을 억제하며 subscribe viewport를 baseline에 적용합니다. Workspace metadata refresh는 수락된 terminal surface를 소유하거나 지우지 않습니다.
- wsxd는 project, worktree, session, pane, terminal, known-agent identity와 검증된 native session reference를 저장합니다. User-intent mutation은 live state와 event를 publish하기 전에 저장합니다. Runtime observation은 persistence가 일시적으로 실패해도 실제 상태를 표시하고 daemon loop에서 저장을 재시도합니다. State replacement는 file과 parent directory를 sync하고 검증된 last-known-good backup 하나를 유지하며 malformed primary만 quarantine합니다. Unsafe file은 fail closed로 처리합니다. daemon restart 후 eligible agent는 `codex resume <id>`, `pi --session <path>` 같은 direct vendor argv로 conversation을 resume합니다. 저장된 identity는 resume 계획에만 사용하며 replacement runtime은 현재 runtime generation으로 adapter가 다시 보고할 때까지 agent label을 표시하지 않습니다.
- Native resume은 항상 새 process, PTY, terminal buffer를 생성합니다. wsxd supervisor가 provider를 direct argv로 실행합니다. Provider가 끝나면 supervisor는 정확히 일치하는 runtime generation의 agent state를 clear한 뒤 fresh shell을 열어 delayed report가 shell을 다시 agent로 표시하지 못하게 합니다. 임의 shell process, terminal history, lease, unsupported agent conversation은 보존하지 않습니다.
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
