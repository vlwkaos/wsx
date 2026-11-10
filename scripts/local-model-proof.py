#!/usr/bin/env python3
"""Real-server proof for the local fallback model (docs/local-model.md). Run: python3 -I scripts/local-model-proof.py [proof|memory] [out_dir]

proof   (default) against the running server (`scripts/local-model.sh start`): /v1/models with and
        without the key, one tool call, decode and 4K-prompt speed, the cached second call, and one
        round trip through `pi --mode rpc`. Writes <out_dir>/proof.json and pi-rpc.jsonl.
memory  starts the server at each context size, fills the window with one prompt, and reads the
        process footprint. It stops the server afterwards. Writes <out_dir>/memory.json.
Output goes to .work/local-model by default. Stdlib only; no mocks.
"""
import json, os, select, shutil, subprocess, sys, tempfile, time, urllib.error, urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SCRIPT = os.path.join(ROOT, 'scripts', 'local-model.sh')
STATE = os.environ.get('LOCAL_MODEL_DIR', os.path.expanduser('~/.local/state/local-model'))
PORT = os.environ.get('LOCAL_MODEL_PORT', '8090')
BASE = f'http://127.0.0.1:{PORT}'
KEY = open(os.path.join(STATE, 'api-key')).read().strip() if os.path.exists(os.path.join(STATE, 'api-key')) else ''
MODEL = 'lfm3-1.7b'


def call(path, body=None, key=KEY, timeout=600):
    req = urllib.request.Request(BASE + path, data=None if body is None else json.dumps(body).encode(),
                                 headers={'Content-Type': 'application/json'})
    if key:
        req.add_header('Authorization', 'Bearer ' + key)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, json.loads(r.read())
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()[:200]


def chat(messages, **extra):
    status, out = call('/v1/chat/completions', {'model': MODEL, 'messages': messages, 'temperature': 0, **extra})
    assert status == 200, (status, out)
    return out


def filler(tokens):
    # ^ Distinct lines, so the text does not compress into a few repeated tokens. Calibrated once
    # with the server's own /tokenize.
    def text(n):
        return '\n'.join(f'Ledger line {i}: task {i * 7 % 997} moved to step {i % 13}, owner note {i * 31 % 101}.' for i in range(n))
    n = max(tokens // 24, 1)
    got = len(call('/tokenize', {'content': text(n)})[1]['tokens'])
    n = max(int(n * tokens / got), 1)
    return text(n)


TOOL = {'type': 'function', 'function': {
    'name': 'run_wsx', 'description': 'Run one wsx command and return its output.',
    'parameters': {'type': 'object', 'properties': {'argv': {'type': 'array', 'items': {'type': 'string'}}}, 'required': ['argv']}}}


def speed(out):
    t = out['timings']
    return {'prompt_tokens': out['usage']['prompt_tokens'], 'cached_tokens': t.get('cache_n', 0),
            'prompt_ms': round(t['prompt_ms'], 1), 'prompt_tok_s': round(t['prompt_per_second'], 1),
            'decode_tokens': t['predicted_n'], 'decode_tok_s': round(t['predicted_per_second'], 1)}


def pi_round_trip(out_dir):
    """One `pi --mode rpc` run against the server with a models.json that holds the key as !cat."""
    home = tempfile.mkdtemp(prefix='wsx-pi-proof-')  # like #351: a folder with models.json only
    work = tempfile.mkdtemp(prefix='wsx-pi-work-')
    models = {'providers': {'llama-wsx': {
        'baseUrl': BASE + '/v1', 'api': 'openai-completions', 'apiKey': '!cat ' + os.path.join(STATE, 'api-key'),
        'models': [{'id': MODEL, 'name': 'LFM3 1.7B Q8_0 (wsx local)', 'reasoning': False, 'input': ['text'],
                    'contextWindow': 32768, 'maxTokens': 2048,
                    'compat': {'supportsStore': False, 'maxTokensField': 'max_tokens'}}]}}}
    with open(os.path.join(home, 'models.json'), 'w') as f:
        json.dump(models, f, indent=2)
    env = {**os.environ, 'PI_CODING_AGENT_DIR': home, 'PI_OFFLINE': '1'}
    argv = ['pi', '--mode', 'rpc', '--no-session', '--model', 'llama-wsx/' + MODEL, '--no-extensions', '--no-mcp',
            '--no-prompt-templates', '--no-themes', '--no-context-files', '--no-skills', '--no-approve', '--thinking', 'off']
    proc = subprocess.Popen(argv, cwd=work, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    prompt = 'Use the bash tool to run: echo wsx-local-ok . Then answer with the word it printed.'
    proc.stdin.write(json.dumps({'id': 'p1', 'type': 'prompt', 'message': prompt}) + '\n')
    proc.stdin.flush()
    events, deadline = [], time.time() + 180
    while time.time() < deadline:
        if not select.select([proc.stdout], [], [], 5)[0]:
            continue
        line = proc.stdout.readline()
        if not line:
            break
        events.append(json.loads(line))
        if events[-1].get('type') == 'agent_settled':
            break
    proc.stdin.close()
    proc.terminate()
    err = proc.stderr.read()
    proc.wait()
    shutil.rmtree(home, ignore_errors=True)
    shutil.rmtree(work, ignore_errors=True)
    with open(os.path.join(out_dir, 'pi-rpc.jsonl'), 'w') as f:
        f.writelines(json.dumps(e) + '\n' for e in events)
    kinds = [e.get('type') for e in events]
    tools = [e.get('toolName') for e in events if e.get('type') == 'tool_execution_start']
    final = [m for m in (e.get('message') for e in events if e.get('type') == 'message_end') if m and m.get('role') == 'assistant']
    text = ''.join(b.get('text', '') for b in (final[-1]['content'] if final else []) if isinstance(b, dict))
    result = {'argv': argv, 'event_kinds': sorted(set(k for k in kinds if k)), 'settled': 'agent_settled' in kinds,
              'tools': tools, 'final_text': text, 'stop_reason': final[-1].get('stopReason') if final else None,
              'error': final[-1].get('errorMessage') if final else None, 'stderr_tail': err[-300:]}
    return result


def proof(out_dir):
    res = {}
    res['models_with_key'] = call('/v1/models')
    res['models_without_key'] = call('/v1/models', key='')[0]
    assert res['models_with_key'][0] == 200 and res['models_without_key'] == 401, res
    # Tool call, reasoning off per request as well as by the server flag.
    out = chat([{'role': 'user', 'content': 'List the open tasks by running the wsx task list command'}],
               tools=[TOOL], reasoning_effort='none')
    msg = out['choices'][0]['message']
    call0 = msg['tool_calls'][0]['function']
    res['tool_call'] = {'finish_reason': out['choices'][0]['finish_reason'], 'name': call0['name'],
                        'arguments': json.loads(call0['arguments']), 'reasoning_content': msg.get('reasoning_content'),
                        'speed': speed(out)}
    assert res['tool_call']['name'] == 'run_wsx' and isinstance(res['tool_call']['arguments']['argv'], list), res['tool_call']
    # Speed: a 4K-token prompt cold, then the same prefix with a new tail.
    system = filler(4000)
    cold = chat([{'role': 'system', 'content': system}, {'role': 'user', 'content': 'Reply with one short sentence about the ledger.'}], max_tokens=64)
    warm = chat([{'role': 'system', 'content': system}, {'role': 'user', 'content': 'Now reply with one short sentence about the owner notes.'}], max_tokens=64)
    res['prompt_4k_cold'] = speed(cold)
    res['prompt_4k_cached'] = speed(warm)
    res['thinking_on_long_prompt'] = bool(cold['choices'][0]['message'].get('reasoning_content'))
    res['pi_rpc'] = pi_round_trip(out_dir)
    with open(os.path.join(out_dir, 'proof.json'), 'w') as f:
        json.dump(res, f, indent=2)
    print(json.dumps(res, indent=2))


def footprint(pid):
    out = subprocess.run(['footprint', str(pid)], capture_output=True, text=True).stdout
    vals = {}
    for line in out.splitlines():
        for name in ('phys_footprint_peak', 'phys_footprint'):
            if line.strip().startswith(name + ':'):
                vals[name] = line.split(':')[1].strip()
                break
    return vals


def memory(out_dir):
    rows = []
    sh = lambda *a, **env: subprocess.run([SCRIPT, *a], env={**os.environ, **env}, check=True, capture_output=True, text=True)
    sh('stop')
    for ctx in (16384, 32768, 65536, 131072):
        sh('start', LOCAL_MODEL_CTX=str(ctx))
        pid = int(open(os.path.join(STATE, 'server.pid')).read())
        idle = footprint(pid)
        out = chat([{'role': 'user', 'content': filler(ctx - 800) + '\nReply with one word.'}], max_tokens=8)
        rows.append({'context': ctx, 'prompt_tokens': out['usage']['prompt_tokens'], 'prompt_tok_s': speed(out)['prompt_tok_s'],
                     'footprint_after_load': idle, 'footprint_after_full_window': footprint(pid)})
        print(rows[-1], flush=True)
        sh('stop')
    with open(os.path.join(out_dir, 'memory.json'), 'w') as f:
        json.dump(rows, f, indent=2)


if __name__ == '__main__':
    mode = sys.argv[1] if len(sys.argv) > 1 else 'proof'
    out_dir = sys.argv[2] if len(sys.argv) > 2 else os.path.join(ROOT, '.work', 'local-model')
    os.makedirs(out_dir, exist_ok=True)
    {'proof': proof, 'memory': memory}[mode](out_dir)
