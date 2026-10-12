// Reference wsx renderer plugin for `mermaid` fenced blocks.
// ^ docs/plugins.md "Fenced-block renderers": one JSON request on stdin, one JSON response on
// stdout. Any failure exits non-zero, and wsx then shows the block source.
import { render } from "grok-mermaid";

const MAX_INPUT_BYTES = 128 * 1024;
const TONES = { border: "muted", text: "normal", edge: "accent", edgeLabel: "muted", title: "accent", none: "normal" };

async function readRequest() {
	const chunks = [];
	let size = 0;
	for await (const chunk of process.stdin) {
		size += chunk.length;
		if (size > MAX_INPUT_BYTES) throw new Error("request is too large");
		chunks.push(chunk);
	}
	const request = JSON.parse(Buffer.concat(chunks).toString("utf8"));
	if (request.api_version !== 1 || request.language !== "mermaid") throw new Error("unsupported request");
	if (typeof request.source !== "string" || !Number.isInteger(request.columns) || request.columns < 1) {
		throw new Error("invalid request");
	}
	return request;
}

// grok-mermaid may center a graph in blank outer columns; measure and emit its visible bounds.
function trimIndent(art) {
	const content = art.plain.filter((line) => line.trim().length > 0);
	const indent = content.length === 0 ? 0 : Math.min(...content.map((line) => line.match(/^ */u)[0].length));
	if (indent === 0) return art;
	const styled = art.styled.map((row) => {
		let remaining = indent;
		return row.flatMap((span) => {
			if (remaining === 0) return [span];
			const removed = Math.min(remaining, span.text.length);
			remaining -= removed;
			const text = span.text.slice(removed);
			return text ? [{ ...span, text }] : [];
		});
	});
	return { ...art, styled, width: art.width - indent };
}

function draw(source) {
	const art = render(source);
	return art ? trimIndent(art) : undefined;
}

// A left-to-right flowchart that is too wide often fits when it is laid out top-down.
function topDown(source) {
	let direction;
	const changed = source.replace(/^(\s*(?:flowchart|graph)[ \t]+)(LR|RL)\b/iu, (_match, prefix, value) => {
		direction = value.toUpperCase();
		return `${prefix}TD`;
	});
	return direction ? { source: changed, direction } : undefined;
}

function lines(art) {
	return art.styled.map((row) => {
		const spans = row.filter((span) => span.text.length > 0).map((span) => ({ text: span.text, tone: TONES[span.cls] ?? "normal" }));
		return spans.length > 0 ? spans : [{ text: "" }];
	});
}

const request = await readRequest();
let art = draw(request.source);
if (!art) throw new Error("mermaid source could not be drawn");
let note;
if (art.width > request.columns) {
	const retry = topDown(request.source);
	const narrower = retry ? draw(retry.source) : undefined;
	if (!narrower || narrower.width > request.columns) throw new Error("diagram is wider than the view");
	art = narrower;
	note = `Layout changed from ${retry.direction} to TD to fit the view.`;
}
if (art.warnings.length > 0) {
	const more = art.warnings.length > 1 ? ` (+${art.warnings.length - 1} more)` : "";
	note = [note, `Partly drawn: ${art.warnings[0]}${more}`].filter(Boolean).join(" ");
}
process.stdout.write(JSON.stringify({ lines: lines(art), ...(note ? { note: note.replace(/[\u0000-\u001f\u007f]/gu, " ") } : {}) }));
