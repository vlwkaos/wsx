// One rule for the two swipes on a phone (docs/web-client-chat.md "Replies", a swipe right on a
// message, and docs/web-client.md "Screens", a slide left on a row): where a swipe starts, and
// when its release counts. A thumb arcs and a person who means a swipe does not stop to measure.
import { motion } from "./lib.js";

const SLOP_PX = 10;
const FLICK_SPEED = 0.35; // px/ms
const FLICK_SHARE = 0.75; // of `need`: a flick shorter than this is a tap's twitch
const TRAIL_MS = 100;

// ^ The browser takes a vertical drag itself (touch-action) from 45° up and ends the touch with
// a pointercancel; a swipe is the finger that has moved SLOP_PX and is not steeper than that.
export const swipes = (dx, dy) => dx >= SLOP_PX && dx >= Math.abs(dy);

// A swipe that moved `px` of the `need` it takes opens when it went the whole way, or was a
// flick, or was past `need` and has not come back below half of it.
export const commits = (px, need, { speed = 0, past = false } = {}) =>
  px >= need || (speed >= FLICK_SPEED && px >= need * FLICK_SHARE) || (past && px >= need / 2);

// The finger's last TRAIL_MS as [time, progress] pairs, progress growing toward the action.
export const track = (trail, time, progress) => [...trail.filter(([at]) => time - at < TRAIL_MS), [time, progress]];
export function speedAt(trail, time) {
  if (trail.length < 2 || time - trail.at(-1)[0] >= TRAIL_MS) return 0;
  const [from, to] = [trail[0], trail.at(-1)];
  return to[0] > from[0] ? (to[1] - from[1]) / (to[0] - from[0]) : 0;
}

// A row that slides left to show Remove (.row-slide). The browser's own scroll moves it; at the
// end of a touch the row goes the whole way when the slide counts (`commits`: a quarter of the
// way, or a flick), else back, so a short drag opens it and the snap's halfway does not decide.
// ^ The snap is off while the finger is down and until the row is there: it ends a scroll of ours
// that starts at the touch's end halfway (measured: at 37 of 52 px, or back at 0).
// data-open fills Remove only while the row is slid open.
const rows = new WeakMap();
const ending = new WeakMap(); // row -> the settle that has not arrived yet
const room = (row) => row.scrollWidth - row.clientWidth;
const snap = (row, on) => { row.style.scrollSnapType = on ? "" : "none"; };
// ^ The finger's last move can start a fling that goes on after touchend and after our scrollTo
// (measured: a quick 20 px drag, or the reset after a flick, ended fully open). overflow hidden
// ends it; the row is released when it is at `left`, or after 600 ms, or when a finger lands.
function settle({ currentTarget: row, timeStamp }) {
  const now = rows.get(row);
  rows.delete(row);
  if (!now) return;
  const travel = now.open ? room(row) - row.scrollLeft : row.scrollLeft;
  const went = commits(travel, room(row) / 4, { speed: speedAt(now.trail, timeStamp) });
  const left = now.open === went ? 0 : room(row);
  const token = {};
  ending.set(row, token);
  const done = () => {
    if (ending.get(row) !== token) return;
    ending.delete(row);
    row.removeEventListener("scrollend", arrived);
    row.style.overflowX = "";
    snap(row, true);
  };
  const arrived = () => Math.abs(row.scrollLeft - left) < 1 && done();
  row.style.overflowX = "hidden";
  row.addEventListener("scrollend", arrived);
  setTimeout(done, 600);
  row.scrollTo({ left, behavior: motion() ? "smooth" : "auto" });
  requestAnimationFrame(arrived);
}
export const slide = {
  onPointerDown: ({ currentTarget: row, pointerType }) => {
    if (pointerType !== "touch") return;
    ending.delete(row);
    row.style.overflowX = "";
    rows.set(row, { open: row.scrollLeft > room(row) / 2, trail: [] });
    snap(row, false);
  },
  onScroll: ({ currentTarget: row, timeStamp }) => {
    row.toggleAttribute("data-open", row.scrollLeft > 0);
    const now = rows.get(row);
    if (now) now.trail = track(now.trail, timeStamp, (now.open ? -1 : 1) * row.scrollLeft);
  },
  onTouchEnd: settle,
  onTouchCancel: settle,
};
