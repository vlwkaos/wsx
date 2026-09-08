# wsx

[![CI](https://github.com/vlwkaos/wsx/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/vlwkaos/wsx/actions/workflows/ci.yml)

Git worktree를 위한 project 중심 terminal workspace 관리자입니다.

wsx는 **Project → Worktree → Session → Pane** 구조를 keyboard 중심 TUI로 표시합니다. 인접한 `wsxd` daemon이 PTY와 terminal state를 소유하므로 client나 SSH 연결이 끊겨도 session은 계속 실행됩니다.

![실행 중인 agent와 terminal preview를 표시하는 wsx Workspace](docs/screenshots/01-workspace-overview.png)

## 주요 기능

- Git project/worktree 검색, group, 상태, alias, 생성, 확인 후 삭제
- 영속 session과 선택적 가로/세로 pane 분할
- keyboard, mouse, selection, clipboard, cursor를 지원하는 Ghostty 기반 terminal viewport
- provider-neutral agent 상태, native conversation resume, project routine
- typed, versioned, bounded 동일 사용자 local protocol
- macOS와 Linux 지원

## 제품 둘러보기

| Workflow | 화면 |
|---|---|
| **Attention 순회**<br>`a/A`는 active session을, `n/N`은 확인이 필요한 session을 순회합니다. | ![Blocked Codex session을 선택한 attention 순회](docs/screenshots/02-attention-iteration.png) |
| **Group 분류**<br>영속 group으로 project를 필터링하고 비활성 project는 stale로 계속 표시합니다. | ![Stale project만 표시하는 group](docs/screenshots/03-groups-and-stale.png) |
| **분할 terminal**<br>Terminal mode에서도 pane 상태, foreground job, 감지된 port를 확인합니다. | ![분할된 server session을 표시하는 Terminal mode](docs/screenshots/04-terminal-and-panes.png) |
| **Routine 예약**<br>Agent template을 선택한 뒤 보이는 argv, schedule, prompt를 편집합니다. | ![Pi routine editor](docs/screenshots/05-routine-editor.png) |
| **wsx 설정**<br>Typed setting으로 workspace, view, terminal, runtime, agent integration을 관리합니다. | ![Global settings](docs/screenshots/06-global-settings.png) |

## 설치

Release archive와 Homebrew formula는 인접한 `wsx`, `wsxd` executable을 함께 설치합니다.

Build에는 Rust 1.96.1과 Zig 0.15.2가 필요합니다. 개발 test는 `cargo-nextest` 0.9.143을 사용합니다.

```bash
cargo install cargo-nextest --version 0.9.143 --locked
git clone https://github.com/vlwkaos/wsx.git
cd wsx
cargo +1.96.1 build --workspace --locked
cargo xtask run
```

`target/wsx-dev/`에 host-native bundle을 만듭니다.

```bash
cargo xtask build
```

## Agent integration과 routine

`u`를 눌러 routine을 만듭니다. 문서로 확인된 one-shot agent template 또는 Custom을 선택합니다. Template은 다음 form에 보이는 command argv를 교체합니다. argv는 계속 편집할 수 있고 Custom은 빈 값으로 시작합니다.

wsx는 설치되어 있고 setup이 필요한 agent를 사용자가 명시적으로 선택했을 때만 integration 설치를 제안합니다. 거절하면 **Global Settings → Runtime → Agent integrations**에서 직접 설치할 때까지 해당 agent prompt를 영구적으로 숨깁니다. PATH 감지나 남은 config file만으로는 prompt를 표시하지 않습니다.

필요하면 직접 설치할 수 있습니다.

```bash
wsx agent install pi
wsx agent install claude
```

Installer는 관련 없는 hook을 보존하고 표준 config-directory override를 따릅니다. 설치 후 해당 agent를 다시 시작합니다. Codex authoritative lifecycle 보고에는 Codex 0.150.0 이상이 필요합니다. Pi는 표준 blocking dialog를 별도 wiring 없이 blocked로 보고합니다.

## 조작

| Context | Key |
|---|---|
| Workspace | `j/k` 이동, `h/l` 접기/펼치기, `Enter` 선택, `m` 순서 변경, `i/I` idle, `a/A` active, `n/N` attention |
| Project | `p` project 추가, `w` worktree 추가, `u` routine 추가, `e` config, `g` group 지정 |
| Worktree | `s` session 추가, `r` alias, `d` 삭제 |
| Session/Pane | `Enter` Terminal, `x` 확인 또는 mute, `C` interrupt |
| Pane | `|` 오른쪽 분할, `-` 아래 분할, `d` 닫기 |
| Group | `T` 관리, `{`/`}` 전환, `g` 지정 |
| Global | `/` 검색, `,` settings, `R` 새로고침, `?` 도움말, `q` TUI 종료, `Q` wsxd 종료 후 나가기 |

Terminal mode는 기본 `Ctrl+A` prefix를 사용합니다. 이어서 `j/k`는 인접 session, `i/I`는 idle, `a/A`는 active, `n/N`은 attention session으로 이동합니다. `B`는 desktop sidebar 전환, `W`는 Workspace, `Q`는 TUI만 종료합니다. `Ctrl+A Ctrl+A`는 literal prefix를 보냅니다.

Group은 순서가 있는 project filter입니다. 기본 **ungrouped** anti-group은 membership이 없는 project를 표시합니다. 설정한 시간 동안 trusted agent 작업이나 terminal 진입이 없으면 project는 stale이 됩니다. wsx는 terminal output이나 process tree로 agent 상태를 추론하지 않습니다.

## 설정

`,`로 typed Global Settings를 엽니다. Linux 설정 파일은 `~/.config/wsx/config-v2.toml`이며 macOS는 같은 이름의 application-support 경로를 사용합니다.

```toml
terminal_escape_chord = "ctrl+a w"
resume_agents_on_restore = true
wake_mode = true
auto_collapse_after_hours = 24
notification_timeout_seconds = 4
show_release_status = true
terminal_sidebar = "compact"
terminal_title_position = "bottom"
port_visibility = "non_agentic"
```

Project root 설정 파일은 `wsx.config.yml`입니다.

```yaml
hooks:
  postCreate: cargo build
copy:
  include: [.env.example]
  exclude: [target]
git:
  subtrees: [vendor/asched, vendor/herdr]
```

wsx는 file을 검증하고 unknown field와 unsafe subtree path를 거부합니다. Canonical YAML이 없을 때만 legacy `.gtrconfig`를 migration합니다.

## CLI

```text
wsx status [--json]
wsx worktree list|create|delete
wsx session list|send-keys|send-text|prompt|peek|rename
wsx group ls|create|rename|add|remove
wsx routine ...
wsx agent install <target>
wsx agent report <pane> --provider <name> --state <state> [--session-id <id>|--session-path <path>]
wsx plugin list|reload
wsx runtime status [--json]
wsx daemon stop|recover
```

Routine의 각 `--arg`는 direct argv item 하나입니다. wsx는 shell을 실행하지 않습니다. 신뢰하지 않는 routine은 enable 또는 run하기 전에 `wsx routine show <name>`으로 확인합니다.

wsx-managed terminal 안에서는 plain `wsx`와 `wsx --mobile`이 nested TUI startup을 거부합니다. 명시적인 subcommand는 계속 사용할 수 있습니다. `wsx runtime status`와 `wsx daemon stop`은 daemon을 시작하지 않습니다.

## Runtime과 보안

- wsxd는 login session이 아니라 host와 Unix user에 귀속됩니다. 동일 사용자의 SSH 재연결은 live PTY와 buffer를 재사용합니다.
- Owner-only socket과 peer-UID 검사로 다른 사용자의 접근을 거부합니다.
- Pane마다 writable lease는 하나입니다. Event는 revision을 invalidate하고 client는 authoritative snapshot으로 복구합니다.
- Message, frame, command, plugin, listener, resource count는 bounded입니다.
- Compatible wsx version은 daemon 하나를 공유합니다. 교체는 다른 TUI build와 fresh authoritative `working` report가 사라질 때까지 기다립니다.
- Native resume은 검증된 provider reference로 새 process, PTY, terminal buffer를 만듭니다. Unsupported reference는 clean shell을 엽니다.
- Remote access, live cross-version process handoff, graphics transport, marketplace, original-process 복원은 지원하지 않습니다.

## 개발

```bash
cargo check --workspace --all-targets --locked
cargo nextest run --workspace --locked
cargo test --workspace --locked --doc
cargo clippy --workspace --all-targets --locked -- -D warnings
python3 scripts/runtime-smoke.py
```

Vendored terminal dependency는 `THIRD-PARTY-NOTICES.md`에서 확인할 수 있습니다.
