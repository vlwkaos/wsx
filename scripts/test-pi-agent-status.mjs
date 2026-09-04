import assert from "node:assert/strict";
const source = new URL("../crates/wsx-core/integrations/pi/wsx-agent-status.ts", import.meta.url);
const { observeBlockingUi } = await import(`${source.href}?test=${Date.now()}`);

const deferred = () => {
  let resolve;
  const promise = new Promise((onResolve) => {
    resolve = onResolve;
  });
  return { promise, resolve };
};

const confirm = deferred();
const select = deferred();
const originals = {
  select: () => select.promise,
  confirm: () => confirm.promise,
  input: async () => { throw new Error("input failed"); },
  custom: () => { throw new Error("custom failed"); },
  editor: async () => "edited",
};
const ui = { ...originals, notify() {} };
const deltas = [];
const restore = observeBlockingUi(ui, (delta) => deltas.push(delta));

const confirming = ui.confirm("Confirm", "Continue?");
const selecting = ui.select("Select", ["one"]);
assert.deepEqual(deltas, [1, 1]);
confirm.resolve(true);
assert.equal(await confirming, true);
assert.deepEqual(deltas, [1, 1, -1]);
select.resolve(undefined);
assert.equal(await selecting, undefined);
assert.deepEqual(deltas, [1, 1, -1, -1]);

await assert.rejects(ui.input("Input"), /input failed/);
assert.deepEqual(deltas.slice(-2), [1, -1]);
assert.throws(() => ui.custom(() => {}), /custom failed/);
assert.deepEqual(deltas.slice(-2), [1, -1]);
assert.equal(await ui.editor("Editor"), "edited");
assert.deepEqual(deltas.slice(-2), [1, -1]);

const pending = deferred();
const pendingOriginal = () => pending.promise;
const pendingUi = { ...originals, confirm: pendingOriginal };
const pendingDeltas = [];
const restorePending = observeBlockingUi(pendingUi, (delta) => pendingDeltas.push(delta));
const pendingConfirmation = pendingUi.confirm("Confirm", "Continue?");
assert.deepEqual(pendingDeltas, [1]);
restorePending();
assert.equal(pendingUi.confirm, pendingOriginal);
pending.resolve(false);
assert.equal(await pendingConfirmation, false);
assert.deepEqual(pendingDeltas, [1]);

restore();
for (const method of Object.keys(originals)) assert.equal(ui[method], originals[method]);
assert.equal(typeof ui.notify, "function");
console.log("Pi agent status UI lifecycle passed");
