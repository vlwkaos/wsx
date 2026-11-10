# Local model server

How to run LiquidAI's LFM3 as wsx's local fallback model. Why and what was measured: [design/local-model-fallback.md](design/local-model-fallback.md) "2.6". Nothing here edits your Pi or wsx config; you add the two entries below by hand.

## Server

`scripts/local-model.sh` runs `llama-server` (Homebrew build 11938 or later; LFM3 needs it) with `LFM3-1.7B-Q8_0.gguf`, 1.8 GB, from [LiquidAI/LFM3-1.7B-GGUF](https://huggingface.co/LiquidAI/LFM3-1.7B-GGUF).

| Command | What it does |
|---|---|
| `scripts/local-model.sh start` | Starts it in the background and waits until `/health` answers. Makes the key file on the first run |
| `scripts/local-model.sh stop` / `status` / `health` | Stops by pid file; shows pid, port, key file mode, launchd state, context; `health` exits 0 only when `/health` is up, `/v1/models` lists the model with the key and answers 401 without it |
| `scripts/local-model.sh install [--no-load]` | Writes `~/Library/LaunchAgents/com.wsx.local-model.plist` and loads it. It starts at login and restarts after a crash. `uninstall` removes it |

Defaults, all overridable: `LOCAL_MODEL_PORT=8090`, `LOCAL_MODEL_CTX=32768`, `LOCAL_MODEL_IDLE_SECONDS=600`, `LOCAL_MODEL_DIR=~/.local/state/local-model` (holds `api-key` mode 600, `server.log`, `server.pid`), `LOCAL_MODEL_FILE` (the Hugging Face cache copy), `LOCAL_MODEL_SERVER`.

Flags: `--jinja` (tool calls), `--reasoning off` (LFM3 thinks by default; `reasoning_effort: "none"` per request also works), `-np 1`, prompt cache on, `--cache-ram 1024`, `--sleep-idle-seconds 600` (the model leaves memory when idle and loads again on the next call), `--host 127.0.0.1`, `--api-key-file`.

If the model file is missing, download this one file and check it against `8f6b24182258205e9e01e21f1f24a1f208c1e8115578825d202ce39976af20b6`:

```bash
hf download LiquidAI/LFM3-1.7B-GGUF LFM3-1.7B-Q8_0.gguf --local-dir <new empty folder>
shasum -a 256 <folder>/LFM3-1.7B-Q8_0.gguf
```

then start with `LOCAL_MODEL_FILE=<folder>/LFM3-1.7B-Q8_0.gguf`.

Memory on a 24 GiB Mac: about 2.4 GB with the 32768 window (0.6 GB footprint plus the weights), 81 MB footprint when asleep. A larger window costs about 14 MB per 1000 tokens (65536: +0.5 GB).

## Pi

Add a provider to `~/.pi/agent/models.json` (wsx copies only this file into each Pi launch):

```json
"llama-wsx": {
  "baseUrl": "http://127.0.0.1:8090/v1",
  "api": "openai-completions",
  "apiKey": "!cat /Users/<you>/.local/state/local-model/api-key",
  "models": [{
    "id": "lfm3-1.7b", "name": "LFM3 1.7B Q8_0 (wsx local)", "reasoning": false,
    "input": ["text"], "contextWindow": 32768, "maxTokens": 2048,
    "compat": { "supportsStore": false, "maxTokensField": "max_tokens" }
  }]
}
```

Add an agent to `config-v2.toml`, then list its name in `tasks.manager.fallback` or `tasks.agent.fallback` ([tasks.md](tasks.md) "Registry"):

```toml
[[agents]]
name = "lfm3-local"
command = ["pi"]
adapter = "pi"
model = "llama-wsx/lfm3-1.7b"
# extension = "<absolute path>" loads a lean runner as Pi's -e (docs/tasks.md "Pi adapter").
# Roles come with the evaluation (design section 3); until then this is a plain Pi entry.
```

## Check it

`python3 -I scripts/local-model-proof.py` runs against the started server: `/v1/models` with and without the key, one tool call, decode speed, a 4K prompt cold and cached, and one `pi --mode rpc` round trip (bash tool). It writes `proof.json` and `pi-rpc.jsonl` to `.work/local-model/`. `python3 -I scripts/local-model-proof.py memory` measures the footprint at four window sizes; it stops the server first.
