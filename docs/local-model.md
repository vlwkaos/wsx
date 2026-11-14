# Local model endpoint

How wsx uses a local model as a fallback agent. Why and what was measured: [design/local-model-fallback.md](design/local-model-fallback.md) "2.6". wsx uses it with no entry to add: it is the built-in stand-in that answers the owner while the manager waits for a usage limit ([tasks.md](tasks.md) "Agents", "Local floor"). Nothing here edits your Pi or wsx config.

## Endpoint

Serving and model management belong to kaos, not wsx. wsx starts no server and installs no login job. `kaos serve` will give the stable endpoint; until it exists, the endpoint is whatever serves on the URL below. Today that is kaos's `llama-server` with LFM3-1.7B Q8_0.

| Setting | Value today |
|---|---|
| Base URL | `http://127.0.0.1:8090` (OpenAI-compatible, `/v1`) |
| API key file | `~/.local/state/local-model/api-key` (mode 600) |
| Model id | `lfm3-1.7b` |
| Needs from the server | tool calls (`llama-server --jinja`), thinking off (`--reasoning off`, or `reasoning_effort: "none"` per request), a context window of at least 32768 |

`scripts/local-model.sh health` checks it: exit 0 only when `/health` answers, `/v1/models` lists a model with the key and answers 401 without it. A sleeping `llama-server` answers `/health` and loads the model on the next call. Other URL or key file: set `LOCAL_MODEL_URL` and `LOCAL_MODEL_KEY_FILE`.

## Own entry (optional)

The built-in floor needs none of this: wsx writes the `llama-wsx` provider into each launch's Pi folder from the endpoint above, unless your `~/.pi/agent/models.json` already has one, and counts the floor only while the endpoint answers. Add the entries below to use the model as a worker's or the manager's fallback (`may`), to load the lean runner, or to change the jobs it fills; an entry or list of yours wins over the floor.

Add a provider to `~/.pi/agent/models.json` (wsx copies only this file into each Pi launch). `baseUrl` and the key file path are the endpoint settings above:

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
# The lean runner (pygmalion): loaded as Pi's -e, it gates Pi's file tools by the policy file wsx
# writes for each launch (docs/tasks.md "Pi adapter"). Without this line the entry is a plain Pi.
extension = "/Users/<you>/ws/tui_cli/pygmalion/runner/index.ts"
# Roles come with the evaluation (design section 3).
```

The runner needs Pi on `PATH` and no setup beyond that path: wsx hands it `PYGMALION_POLICY` and `PYGMALION_POLICY_SHA256` itself. When it refuses a tool call, the task's log shows `runner denied write <path>: <reason>` as progress.

## Check it

`python3 -I scripts/local-model-proof.py` runs against the endpoint (same `LOCAL_MODEL_URL` and `LOCAL_MODEL_KEY_FILE`; `LOCAL_MODEL_ID` names the model, else the first one `/v1/models` lists): `/v1/models` with and without the key, one tool call, decode speed, a 4K prompt cold and cached, and one `pi --mode rpc` round trip (bash tool). It writes `proof.json` and `pi-rpc.jsonl` to `.work/local-model/`, or to the folder given as its argument. It starts and stops no server.

`python3 -B scripts/runner-proof.py` runs the real Pi with the real lean runner through wsx (isolated wsxd, inside the Seatbelt boundary, so not inside another sandbox; build `wsx` and `wsxd` first; `--runner` and `--pi` name the files). Its provider is a scripted server on loopback, so it needs no key and no model: one worker writes outside and inside its working copy (the runner denies the first, with a `runner denied` line in the task log), and one runs on a policy file changed after wsx wrote it (the runner denies every call). It writes `.work/runner-proof/report.json`.
